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
use oci_cache::routing::Upstream;
use sha2::{Digest, Sha256};
use tempfile::tempdir;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
