//! 上流からの取得と中継のテスト。
//!
//! このフェーズで対象とする要求: REQ-0010 / REQ-0051（`CLAUDE.md` の実装順序 2.）。
//! `tests/cache.rs::blob_served_from_store_without_upstream_call` は
//! `docs/requirements/generated/traceability.md` の REQ-0010 の検証手段。
//!
//! 上流レジストリは wiremock でモックし、`GET /v2/<repository>/blobs/<digest>`
//! への応答を blob 本体として扱う。

use std::sync::Arc;
use std::time::Duration;

use oci_cache::cache::{BlobCache, BlobSource, ManifestCache, ManifestReference, OciBlobSource};
use oci_cache::routing::{Upstream, UpstreamCredentials};
use sha2::{Digest, Sha256};
use tempfile::tempdir;
use wiremock::matchers::{header, header_exists, method, path, query_param};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

fn digest_of(content: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(content)))
}

async fn register_blob(server: &MockServer, repository: &str, digest: &str, content: &[u8]) {
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repository}/blobs/{digest}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
        .mount(server)
        .await;
}

async fn register_manifest(server: &MockServer, repository: &str, reference: &str, content: &[u8]) {
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repository}/manifests/{reference}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
        .mount(server)
        .await;
}

fn manifest_requests_count(
    requests: &[wiremock::Request],
    repository: &str,
    reference: &str,
) -> usize {
    let manifest_path = format!("/v2/{repository}/manifests/{reference}");
    requests
        .iter()
        .filter(|request| request.url.path() == manifest_path)
        .count()
}

/// 認証方式の確認（`GET /v2/`）に対して、challenge の無い成功応答を返す
/// よう登録する。未登録のままだと wiremock の既定応答（404）になり、
/// 匿名上流と誤認せず取得を失敗させる（`OciBlobSource::auth_shape` 参照）。
async fn register_anonymous_v2(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/v2/"))
        .respond_with(ResponseTemplate::new(200))
        .mount(server)
        .await;
}

/// REQ-0010: 上流レジストリから取得した blob はダイジェストを鍵として
/// 保存され、以降の同一ダイジェストへの要求は上流レジストリへ問い合わせず
/// 保存内容から応答する。
#[tokio::test]
async fn blob_served_from_store_without_upstream_call() {
    let server = MockServer::start().await;
    let repository = "library/nginx";
    let content = b"this is a fake layer blob for the test".to_vec();
    let digest = digest_of(&content);
    register_anonymous_v2(&server).await;
    register_blob(&server, repository, &digest, &content).await;

    let upstream = Upstream::new("docker.io", server.uri());
    let store_dir = tempdir().unwrap();
    let cache = BlobCache::new(store_dir.path(), OciBlobSource::new());

    let first_path = cache.get(&upstream, repository, &digest).await.unwrap();
    assert_eq!(tokio::fs::read(&first_path).await.unwrap(), content);
    // 認証確認だけで複数回問い合わせる oci-client の内部動作を含むため、
    // ここでは絶対数ではなく「2回目で増えないこと」だけを見る
    let requests_after_first = server.received_requests().await.unwrap().len();
    assert!(requests_after_first >= 1);

    let second_path = cache.get(&upstream, repository, &digest).await.unwrap();
    assert_eq!(second_path, first_path);
    assert_eq!(tokio::fs::read(&second_path).await.unwrap(), content);

    // 2回目の要求で上流への問い合わせが増えていないこと(保存内容から応答したこと)を確認する
    let requests_after_second = server.received_requests().await.unwrap().len();
    assert_eq!(requests_after_second, requests_after_first);
}

/// 保存先は OCI Image Layout に沿った `blobs/<algorithm>/<hex>`
/// （README の保存レイアウト）に配置される。
#[tokio::test]
async fn blob_is_stored_under_digest_addressed_path() {
    let server = MockServer::start().await;
    let repository = "library/nginx";
    let content = b"another fake layer blob".to_vec();
    let digest = digest_of(&content);
    let hex = digest.strip_prefix("sha256:").unwrap();
    register_anonymous_v2(&server).await;
    register_blob(&server, repository, &digest, &content).await;

    let upstream = Upstream::new("docker.io", server.uri());
    let store_dir = tempdir().unwrap();
    let cache = BlobCache::new(store_dir.path(), OciBlobSource::new());

    let stored_path = cache.get(&upstream, repository, &digest).await.unwrap();

    assert_eq!(
        stored_path,
        store_dir.path().join("blobs").join("sha256").join(hex)
    );
}

/// 上流が `WWW-Authenticate` で Bearer challenge を返す場合、`realm` が
/// 上流の設定と一致する信頼できる発行元であればトークンを取得して blob を
/// 取得できる。docker.io は匿名 pull でもこの手順を要求する。
#[tokio::test]
async fn blob_fetch_succeeds_after_bearer_challenge() {
    let server = MockServer::start().await;
    let repository = "library/nginx";
    let content = b"blob behind a bearer challenge".to_vec();
    let digest = digest_of(&content);
    let realm = format!("{}/token", server.uri());

    Mock::given(method("GET"))
        .and(path("/v2/"))
        .respond_with(ResponseTemplate::new(401).insert_header(
            "WWW-Authenticate",
            format!(r#"Bearer realm="{realm}",service="registry.docker.io""#),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "test-token",
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repository}/blobs/{digest}")))
        .and(header("authorization", "Bearer test-token"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(content.clone()))
        .mount(&server)
        .await;

    let upstream = Upstream::new("docker.io", server.uri());
    let store_dir = tempdir().unwrap();
    let cache = BlobCache::new(store_dir.path(), OciBlobSource::new());

    let stored_path = cache.get(&upstream, repository, &digest).await.unwrap();

    assert_eq!(tokio::fs::read(&stored_path).await.unwrap(), content);
}

/// `WWW-Authenticate` の `realm` が上流の設定（`base_url` / `token_host`）と
/// 異なるホストを指す場合、トークンを取得せず取得を失敗させる
/// （CodeRabbit レビュー指摘: 上流が返す `realm` を無条件に信頼すると、
/// 設定した上流1つが悪意を持つだけでこのサーバーに任意のホストへ
/// リクエストさせられる SSRF になる）。
#[tokio::test]
async fn blob_fetch_rejects_bearer_realm_with_untrusted_host() {
    let server = MockServer::start().await;
    let attacker = MockServer::start().await;
    let repository = "library/nginx";
    let content = b"should never be fetched".to_vec();
    let digest = digest_of(&content);
    let forged_realm = format!("{}/token", attacker.uri());

    Mock::given(method("GET"))
        .and(path("/v2/"))
        .respond_with(ResponseTemplate::new(401).insert_header(
            "WWW-Authenticate",
            format!(r#"Bearer realm="{forged_realm}",service="registry.docker.io""#),
        ))
        .mount(&server)
        .await;
    // 攻撃者側のトークンエンドポイントが呼ばれたかどうかを確かめられるよう
    // 応答は用意しておく
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "stolen-token",
        })))
        .mount(&attacker)
        .await;

    let upstream = Upstream::new("docker.io", server.uri());
    let store_dir = tempdir().unwrap();
    let cache = BlobCache::new(store_dir.path(), OciBlobSource::new());

    let result = cache.get(&upstream, repository, &digest).await;

    assert!(result.is_err());
    assert!(attacker.received_requests().await.unwrap().is_empty());
}

/// 同一ダイジェストへの並行な要求は、最初の1件だけが実際に上流へ
/// 問い合わせ、残りはその完了を待って保存済みの内容を返す。無ければ
/// 未保存の同じ blob への同時要求がそれぞれ独立に取得してしまい、帯域と
/// 上流の要求回数制限を無駄に消費する。
#[tokio::test]
async fn concurrent_requests_for_same_digest_are_coalesced() {
    let server = MockServer::start().await;
    let repository = "library/nginx";
    let content = b"coalesced blob content".to_vec();
    let digest = digest_of(&content);

    register_anonymous_v2(&server).await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repository}/blobs/{digest}")))
        .respond_with(
            // 並行に来た要求が確実に「取得中」のタイミングで重なるよう、
            // 応答を少し遅らせる
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(200))
                .set_body_bytes(content.clone()),
        )
        .mount(&server)
        .await;

    let upstream = Upstream::new("docker.io", server.uri());
    let store_dir = tempdir().unwrap();
    let cache = Arc::new(BlobCache::new(store_dir.path(), OciBlobSource::new()));

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let cache = Arc::clone(&cache);
            let upstream = upstream.clone();
            let digest = digest.clone();
            tokio::spawn(async move { cache.get(&upstream, repository, &digest).await })
        })
        .collect();

    for handle in handles {
        let path = handle.await.unwrap().unwrap();
        assert_eq!(tokio::fs::read(&path).await.unwrap(), content);
    }

    let blob_path = format!("/v2/{repository}/blobs/{digest}");
    let blob_requests = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == blob_path)
        .count();
    assert_eq!(blob_requests, 1);
}

async fn fetch_to_temp_file(
    source: &OciBlobSource,
    upstream: &Upstream,
    repository: &str,
    digest: &str,
) -> Vec<u8> {
    let dir = tempdir().unwrap();
    let path = dir.path().join("out");
    let file = tokio::fs::File::create(&path).await.unwrap();
    source
        .fetch_into(upstream, repository, digest, file)
        .await
        .unwrap();
    tokio::fs::read(&path).await.unwrap()
}

/// 匿名上流（`WWW-Authenticate` challenge を返さない）で複数回 blob を
/// 取得しても、認証方式の確認（`GET /v2/`）は初回の1回だけで済む。
#[tokio::test]
async fn repeated_fetch_reuses_cached_auth_shape_for_anonymous_upstream() {
    let server = MockServer::start().await;
    let repository = "library/nginx";
    let first = b"first blob".to_vec();
    let second = b"second blob".to_vec();
    let first_digest = digest_of(&first);
    let second_digest = digest_of(&second);
    register_anonymous_v2(&server).await;
    register_blob(&server, repository, &first_digest, &first).await;
    register_blob(&server, repository, &second_digest, &second).await;

    let upstream = Upstream::new("docker.io", server.uri());
    let source = OciBlobSource::new();

    let out1 = fetch_to_temp_file(&source, &upstream, repository, &first_digest).await;
    assert_eq!(out1, first);
    let out2 = fetch_to_temp_file(&source, &upstream, repository, &second_digest).await;
    assert_eq!(out2, second);

    let v2_requests = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/v2/")
        .count();
    assert_eq!(v2_requests, 1);
}

/// 認証方式の確認（`GET /v2/`）がリダイレクトを返しても追従しない
/// （CodeRabbit レビュー指摘: 設定済みの上流が悪意を持つ、または乗っ取ら
/// れた場合、リダイレクト先として任意のホストを指定してこのサーバーに
/// 問い合わせさせられる SSRF になる）。
#[tokio::test]
async fn blob_fetch_does_not_follow_redirect_from_auth_probe() {
    let server = MockServer::start().await;
    let attacker = MockServer::start().await;
    let repository = "library/nginx";
    let content = b"should never be fetched via redirect".to_vec();
    let digest = digest_of(&content);

    Mock::given(method("GET"))
        .and(path("/v2/"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("Location", format!("{}/v2/", attacker.uri())),
        )
        .mount(&server)
        .await;
    // 攻撃者側が呼ばれたかどうかを確かめられるよう応答は用意しておく
    Mock::given(method("GET"))
        .and(path("/v2/"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&attacker)
        .await;

    let upstream = Upstream::new("docker.io", server.uri());
    let store_dir = tempdir().unwrap();
    let cache = BlobCache::new(store_dir.path(), OciBlobSource::new());

    let result = cache.get(&upstream, repository, &digest).await;

    assert!(result.is_err());
    assert!(attacker.received_requests().await.unwrap().is_empty());
}

/// 認証方式の確認（`GET /v2/`）が一時的に 5xx を返しても、その結果を
/// 「匿名」としてキャッシュしない。上流が回復すれば次の要求で正しく
/// 認証方式を再確認できる（CodeRabbit レビュー指摘）。
#[tokio::test]
async fn transient_auth_probe_failure_is_not_cached_as_anonymous() {
    let server = MockServer::start().await;
    let repository = "library/nginx";
    let content = b"blob after upstream recovers".to_vec();
    let digest = digest_of(&content);

    // 最初の1回だけ 503 を返し、それ以降は正常な匿名応答に切り替わる
    // （優先度が同じ mock は先に mount した方が優先されるため、この順序で
    // 「1回だけ落ちて、以降は回復する」上流を再現できる）。
    Mock::given(method("GET"))
        .and(path("/v2/"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    register_anonymous_v2(&server).await;
    register_blob(&server, repository, &digest, &content).await;

    let upstream = Upstream::new("docker.io", server.uri());
    let store_dir = tempdir().unwrap();
    let cache = BlobCache::new(store_dir.path(), OciBlobSource::new());

    let first_attempt = cache.get(&upstream, repository, &digest).await;
    assert!(first_attempt.is_err());

    let second_attempt = cache.get(&upstream, repository, &digest).await.unwrap();
    assert_eq!(tokio::fs::read(&second_attempt).await.unwrap(), content);
}

/// REQ-0012: ダイジェストを指定してマニフェストを要求した時、保存済み
/// マニフェストが存在すれば有効期間を確認せずに応答する（内容が不変な
/// ダイジェスト参照は再検証の必要が無い）。
#[tokio::test]
async fn digest_manifest_never_revalidated() {
    let server = MockServer::start().await;
    let repository = "library/nginx";
    let content =
        br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json"}"#.to_vec();
    let digest = digest_of(&content);
    register_anonymous_v2(&server).await;
    register_manifest(&server, repository, &digest, &content).await;

    let upstream = Upstream::new("docker.io", server.uri());
    let store_dir = tempdir().unwrap();
    let cache = ManifestCache::new(store_dir.path(), OciBlobSource::new());
    let reference = ManifestReference::Digest(digest.clone());

    let first = cache.get(&upstream, repository, &reference).await.unwrap();
    assert_eq!(first.content, content);
    assert!(!first.stale);

    let second = cache.get(&upstream, repository, &reference).await.unwrap();
    assert_eq!(second.content, content);

    let requests = server.received_requests().await.unwrap();
    // ダイジェスト参照は保存済みなら再問い合わせしないため、2回目で
    // manifests エンドポイントへの要求は増えない
    assert_eq!(manifest_requests_count(&requests, repository, &digest), 1);
}

/// 同じ上流（同じ `Client`）で複数のリポジトリのマニフェストを取得しても、
/// 後続のリポジトリが先に取得したリポジトリのトークンを誤って使い回さない
/// （CodeRabbit レビュー指摘: `oci_client::Client` は `RegistryAuth::Bearer`
/// を渡された場合、内部の再交渉をリポジトリ単位で行わずそのまま返すため、
/// `client.auth` で明示的にリポジトリ単位のトークンを登録しておかないと、
/// レジストリ単位で記憶された最初のリポジトリ用トークンを後続のリポジトリ
/// にも使ってしまう）。
#[tokio::test]
async fn manifest_fetch_uses_correct_token_per_repository() {
    let server = MockServer::start().await;
    let realm = format!("{}/token", server.uri());
    let nginx_repo = "library/nginx";
    let redis_repo = "library/redis";
    let nginx_content = br#"{"schemaVersion":2,"name":"nginx"}"#.to_vec();
    let redis_content = br#"{"schemaVersion":2,"name":"redis"}"#.to_vec();

    Mock::given(method("GET"))
        .and(path("/v2/"))
        .respond_with(ResponseTemplate::new(401).insert_header(
            "WWW-Authenticate",
            format!(r#"Bearer realm="{realm}",service="registry.docker.io""#),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(query_param(
            "scope",
            format!("repository:{nginx_repo}:pull"),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "nginx-token",
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(query_param(
            "scope",
            format!("repository:{redis_repo}:pull"),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "redis-token",
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/{nginx_repo}/manifests/latest")))
        .and(header("authorization", "Bearer nginx-token"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(nginx_content.clone()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/{redis_repo}/manifests/latest")))
        .and(header("authorization", "Bearer redis-token"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(redis_content.clone()))
        .mount(&server)
        .await;

    let upstream = Upstream::new("docker.io", server.uri());
    let store_dir = tempdir().unwrap();
    // 1つの ManifestCache（＝1つの Client）を2つのリポジトリで使い回す
    let cache = ManifestCache::new(store_dir.path(), OciBlobSource::new());
    let reference = ManifestReference::Tag("latest".to_string());

    let nginx_response = cache.get(&upstream, nginx_repo, &reference).await.unwrap();
    assert_eq!(nginx_response.content, nginx_content);

    let redis_response = cache.get(&upstream, redis_repo, &reference).await.unwrap();
    assert_eq!(redis_response.content, redis_content);
}

/// 同一のリポジトリ参照とタグが複数の上流に存在する時、タグの再検証状態を
/// 上流ごとに分離する（CodeRabbit レビュー指摘: `(repository, tag)` だけを
/// キーにすると、ある上流から保存した内容を別の上流（`?ns=` による明示的な
/// 指定や REQ-0007 の名前衝突解決を経て到達した場合）にも誤って使い回す）。
#[tokio::test]
async fn manifest_tag_cache_is_scoped_per_upstream() {
    let server_a = MockServer::start().await;
    let server_b = MockServer::start().await;
    let repository = "library/nginx";
    let tag = "latest";
    let content_a = br#"{"schemaVersion":2,"upstream":"a"}"#.to_vec();
    let content_b = br#"{"schemaVersion":2,"upstream":"b"}"#.to_vec();

    register_anonymous_v2(&server_a).await;
    register_anonymous_v2(&server_b).await;
    register_manifest(&server_a, repository, tag, &content_a).await;
    register_manifest(&server_b, repository, tag, &content_b).await;

    let upstream_a = Upstream::new("docker.io", server_a.uri());
    let upstream_b = Upstream::new("ghcr.io", server_b.uri());
    let store_dir = tempdir().unwrap();
    let cache = ManifestCache::new(store_dir.path(), OciBlobSource::new());
    let reference = ManifestReference::Tag(tag.to_string());

    let response_a = cache
        .get(&upstream_a, repository, &reference)
        .await
        .unwrap();
    assert_eq!(response_a.content, content_a);

    let response_b = cache
        .get(&upstream_b, repository, &reference)
        .await
        .unwrap();
    assert_eq!(response_b.content, content_b);
}

/// REQ-0013 相当のセキュリティ判断: タグの再検証で上流が
/// 「認証情報なしでは取得できない」と応答するようになった（＝非公開化
/// された）時、保存済みの内容を stale として配信し続けない
/// （CodeRabbit レビュー指摘: Authorization Bypass。一時的な到達不能
/// エラーと違い、この場合は内容そのものへのアクセス制御が変わっている）。
#[tokio::test(start_paused = true)]
async fn stale_response_not_served_when_content_becomes_private() {
    let server = MockServer::start().await;
    let repository = "library/nginx";
    let tag = "latest";
    let content = br#"{"schemaVersion":2,"was":"public"}"#.to_vec();
    let realm = format!("{}/token", server.uri());

    Mock::given(method("GET"))
        .and(path("/v2/"))
        .respond_with(ResponseTemplate::new(401).insert_header(
            "WWW-Authenticate",
            format!(r#"Bearer realm="{realm}",service="registry.docker.io""#),
        ))
        .mount(&server)
        .await;
    // 最初の1回だけトークンを発行する（公開状態）
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "public-token",
        })))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    // 以降のトークン要求は拒否される（非公開化された状態を再現する）
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repository}/manifests/{tag}")))
        .and(header("authorization", "Bearer public-token"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(content.clone()))
        .mount(&server)
        .await;

    let upstream = Upstream::new("docker.io", server.uri());
    let store_dir = tempdir().unwrap();
    let cache = ManifestCache::new(store_dir.path(), OciBlobSource::new());
    let reference = ManifestReference::Tag(tag.to_string());

    let first = cache.get(&upstream, repository, &reference).await.unwrap();
    assert_eq!(first.content, content);

    // 有効期間を過ぎさせ、再検証を発生させる
    tokio::time::advance(ManifestCache::DEFAULT_TAG_TTL + Duration::from_secs(1)).await;

    let result = cache.get(&upstream, repository, &reference).await;

    assert!(result.is_err());
}

/// REQ-0011: タグを指定してマニフェストを要求し、かつ保存済みマニフェスト
/// の有効期間が経過している時、上流レジストリへ再問い合わせする。
#[tokio::test(start_paused = true)]
async fn expired_tag_manifest_triggers_revalidation() {
    let server = MockServer::start().await;
    let repository = "library/nginx";
    let tag = "latest";
    register_anonymous_v2(&server).await;
    let first_content = br#"{"schemaVersion":2,"first":true}"#.to_vec();
    let second_content = br#"{"schemaVersion":2,"first":false}"#.to_vec();
    // 優先度が同じ mock は先に mount した方が優先されるため、
    // up_to_n_times(1) の枠を使い切った後は2件目の応答に切り替わる
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repository}/manifests/{tag}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(first_content.clone()))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    register_manifest(&server, repository, tag, &second_content).await;

    let upstream = Upstream::new("docker.io", server.uri());
    let store_dir = tempdir().unwrap();
    let cache = ManifestCache::new(store_dir.path(), OciBlobSource::new());
    let reference = ManifestReference::Tag(tag.to_string());

    let first = cache.get(&upstream, repository, &reference).await.unwrap();
    assert_eq!(first.content, first_content);

    // 既定の有効期間（30分）を超えて時間を進める
    tokio::time::advance(ManifestCache::DEFAULT_TAG_TTL + Duration::from_secs(1)).await;

    let second = cache.get(&upstream, repository, &reference).await.unwrap();

    assert_eq!(second.content, second_content);
    assert!(!second.stale);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(manifest_requests_count(&requests, repository, tag), 2);
}

/// REQ-0014: 再検証のための上流への問い合わせが失敗した時、保存済みの
/// 内容が存在すればそれを返し、再検証されていないことを示す。
#[tokio::test(start_paused = true)]
async fn stale_content_served_when_upstream_unreachable() {
    let server = MockServer::start().await;
    let repository = "library/nginx";
    let tag = "latest";
    let content = br#"{"schemaVersion":2,"stale":"candidate"}"#.to_vec();
    register_anonymous_v2(&server).await;
    register_manifest(&server, repository, tag, &content).await;

    let upstream = Upstream::new("docker.io", server.uri());
    let store_dir = tempdir().unwrap();
    let cache = ManifestCache::new(store_dir.path(), OciBlobSource::new());
    let reference = ManifestReference::Tag(tag.to_string());

    let first = cache.get(&upstream, repository, &reference).await.unwrap();
    assert_eq!(first.content, content);

    // 有効期間を過ぎさせたうえで、上流が応答しなくなった状況を再現する
    tokio::time::advance(ManifestCache::DEFAULT_TAG_TTL + Duration::from_secs(1)).await;
    server.reset().await;
    register_anonymous_v2(&server).await;
    // manifests エンドポイントには何も登録しないため、要求は失敗する

    let second = cache.get(&upstream, repository, &reference).await.unwrap();

    assert_eq!(second.content, content);
    assert!(second.stale);
}

/// REQ-0015: タグ参照マニフェストの既定の有効期間は30分。
#[test]
fn tag_manifest_default_ttl_is_30_minutes() {
    assert_eq!(ManifestCache::DEFAULT_TAG_TTL, Duration::from_secs(30 * 60));
}

/// REQ-0018: 運用者がタグ参照マニフェストの有効期間を設定した時、
/// システムは既定値ではなく設定された値を使う。
#[tokio::test(start_paused = true)]
async fn configured_ttl_overrides_default() {
    let server = MockServer::start().await;
    let repository = "library/nginx";
    let tag = "latest";
    register_anonymous_v2(&server).await;
    let first_content = br#"{"schemaVersion":2,"first":true}"#.to_vec();
    let second_content = br#"{"schemaVersion":2,"first":false}"#.to_vec();
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repository}/manifests/{tag}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(first_content.clone()))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    register_manifest(&server, repository, tag, &second_content).await;

    let upstream = Upstream::new("docker.io", server.uri());
    let store_dir = tempdir().unwrap();
    let custom_ttl = Duration::from_millis(50);
    let cache = ManifestCache::with_tag_ttl(store_dir.path(), OciBlobSource::new(), custom_ttl);
    let reference = ManifestReference::Tag(tag.to_string());

    let first = cache.get(&upstream, repository, &reference).await.unwrap();
    assert_eq!(first.content, first_content);

    // 既定値（30分）よりはるかに短いが、設定した TTL は超える時間だけ進める
    tokio::time::advance(custom_ttl + Duration::from_millis(50)).await;

    let second = cache.get(&upstream, repository, &reference).await.unwrap();

    // 既定値のままなら再検証は起きないはずだが、設定した短い TTL が効いて
    // いれば再検証され、新しい内容に更新される
    assert_eq!(second.content, second_content);
}

struct MissingAuthorizationHeader;

impl wiremock::Match for MissingAuthorizationHeader {
    fn matches(&self, request: &Request) -> bool {
        !request.headers.contains_key("authorization")
    }
}

/// REQ-0016: 運用者が上流レジストリの認証情報を設定した時、その認証情報を
/// 当該上流レジストリへの問い合わせに使う。ただし、認証情報を実際に使った
/// 問い合わせは HTTPS の上流に対してのみ行う（CodeRabbit レビュー指摘:
/// CWE-319 Cleartext Transmission。ネットワーク上の第三者に平文で読み取ら
/// れうる HTTP 経路へは送らない）。
///
/// wiremock は TLS を提供できないため、この統合テストでは HTTP の上流に
/// 認証情報を設定した場合に要求そのものが拒否され、認証情報はもちろん
/// 匿名トークンによる取得さえ試みられないことを確認する。認証情報の
/// 使用そのもの（Basic 認証ヘッダの付与）とスキームの組み合わせ判定は
/// `src/registry_auth.rs` の単体テスト（`uses_https_requires_both_...` /
/// `credentials_are_not_sent_over_plain_http`）で検証する。
#[tokio::test]
async fn configured_credentials_used_for_upstream_request() {
    let server = MockServer::start().await;
    let repository = "library/nginx";
    let digest = digest_of(b"gated behind configured credentials");
    let realm = format!("{}/token", server.uri());

    Mock::given(method("GET"))
        .and(path("/v2/"))
        .respond_with(ResponseTemplate::new(401).insert_header(
            "WWW-Authenticate",
            format!(r#"Bearer realm="{realm}",service="registry.docker.io""#),
        ))
        .mount(&server)
        .await;
    // 認証情報が実際に使われるなら呼ばれるはずのエンドポイント。呼ばれない
    // ことを below で確認する
    Mock::given(method("GET"))
        .and(path("/token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "should-never-be-issued",
        })))
        .mount(&server)
        .await;

    let upstream = Upstream::new("docker.io", server.uri())
        .with_credentials(UpstreamCredentials::new("produser", "s3cr3t"));
    let store_dir = tempdir().unwrap();
    let cache = BlobCache::new(store_dir.path(), OciBlobSource::new());

    let result = cache.get(&upstream, repository, &digest).await;

    assert!(result.is_err());
    // 匿名でのトークン確認（REQ-0013 / REQ-0019 向け）は HTTP でも行うが、
    // 認証情報を使った問い合わせ（Authorization ヘッダ付きのトークン要求）
    // は一切行われない。当然、blob 本体の取得にも到達しない
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path() == "/token"
                && request.headers.contains_key("authorization"))
            .count(),
        0
    );
    let blob_path = format!("/v2/{repository}/blobs/{digest}");
    assert!(requests
        .iter()
        .all(|request| request.url.path() != blob_path));
}

/// REQ-0013: 上流レジストリから取得したイメージが認証情報なしでは取得
/// できないものである時、その内容を保存しない。たとえ設定された認証情報
/// なら取得できたとしても、非公開イメージを認証を持たない利用者へ配信
/// する経路になるため、取得そのものを行わない。
#[tokio::test]
async fn authenticated_content_is_not_persisted() {
    let server = MockServer::start().await;
    let repository = "library/private";
    let content = b"should never be persisted".to_vec();
    let digest = digest_of(&content);
    let hex = digest.strip_prefix("sha256:").unwrap();
    let realm = format!("{}/token", server.uri());

    Mock::given(method("GET"))
        .and(path("/v2/"))
        .respond_with(ResponseTemplate::new(401).insert_header(
            "WWW-Authenticate",
            format!(r#"Bearer realm="{realm}",service="registry.docker.io""#),
        ))
        .mount(&server)
        .await;
    // 匿名でのトークン要求は拒否される（非公開リポジトリ）
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(MissingAuthorizationHeader)
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    // 設定した認証情報なら取得できてしまう状況を用意しておき、それが
    // 使われていないことを確かめる
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(header_exists("authorization"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "credentialed-token",
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repository}/blobs/{digest}")))
        .and(header("authorization", "Bearer credentialed-token"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(content.clone()))
        .mount(&server)
        .await;

    let upstream = Upstream::new("docker.io", server.uri())
        .with_credentials(UpstreamCredentials::new("produser", "s3cr3t"));
    let store_dir = tempdir().unwrap();
    let cache = BlobCache::new(store_dir.path(), OciBlobSource::new());

    let result = cache.get(&upstream, repository, &digest).await;

    assert!(result.is_err());
    assert!(
        !tokio::fs::try_exists(store_dir.path().join("blobs").join("sha256").join(hex))
            .await
            .unwrap()
    );
    // 認証情報を使った実際の取得が一切試みられていないことも確認する
    let blob_path = format!("/v2/{repository}/blobs/{digest}");
    let blob_requests = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == blob_path)
        .count();
    assert_eq!(blob_requests, 0);
}

/// REQ-0019: 認証情報を用いない問い合わせで取得できることを確認できた
/// イメージは保存する。
#[tokio::test]
async fn persistence_requires_anonymous_pull_success() {
    let server = MockServer::start().await;
    let repository = "library/nginx";
    let content = b"publicly available blob".to_vec();
    let digest = digest_of(&content);
    let realm = format!("{}/token", server.uri());

    Mock::given(method("GET"))
        .and(path("/v2/"))
        .respond_with(ResponseTemplate::new(401).insert_header(
            "WWW-Authenticate",
            format!(r#"Bearer realm="{realm}",service="registry.docker.io""#),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/token"))
        .and(MissingAuthorizationHeader)
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "anonymous-token",
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repository}/blobs/{digest}")))
        .and(header("authorization", "Bearer anonymous-token"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(content.clone()))
        .mount(&server)
        .await;

    // 認証情報は設定しない
    let upstream = Upstream::new("docker.io", server.uri());
    let store_dir = tempdir().unwrap();
    let cache = BlobCache::new(store_dir.path(), OciBlobSource::new());

    let stored_path = cache.get(&upstream, repository, &digest).await.unwrap();

    assert_eq!(tokio::fs::read(&stored_path).await.unwrap(), content);
    assert!(tokio::fs::try_exists(&stored_path).await.unwrap());
}
