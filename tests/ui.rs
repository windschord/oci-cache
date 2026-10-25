//! Web UI と観測のテスト。
//!
//! 対象要求: REQ-0017 / REQ-0040 〜 REQ-0047（`CLAUDE.md` の実装順序 5.）。
//! テスト名は `docs/requirements/ui.yaml` と `docs/requirements/cache.yaml`
//! （REQ-0017）の `verification.ref` と一致させてある。

use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
use oci_cache::routing::{RoutingConfig, Upstream};
use oci_cache::server::{build_router, AppState};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tempfile::TempDir;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn digest_of(content: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(content)))
}

async fn register_anonymous_v2(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/v2/"))
        .respond_with(ResponseTemplate::new(200))
        .mount(server)
        .await;
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

fn build_app(upstreams: Vec<Upstream>) -> (axum::Router, TempDir) {
    let config = RoutingConfig {
        upstreams,
        probe_timeout: Duration::from_secs(5),
        ..Default::default()
    };
    let store_dir = tempfile::tempdir().unwrap();
    let state = AppState::new(config, store_dir.path());
    (build_router(state), store_dir)
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, serde_json::Value) {
    let request = HttpRequest::builder().uri(uri).body(Body::empty()).unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json = if body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    (status, json)
}

async fn post_refresh(
    app: &axum::Router,
    upstream: &str,
    repository: &str,
    tag: &str,
) -> axum::http::Response<Body> {
    let body = serde_json::to_vec(&serde_json::json!({
        "upstream": upstream,
        "repository": repository,
        "tag": tag,
    }))
    .unwrap();
    let request = HttpRequest::builder()
        .method("POST")
        .uri("/api/refresh")
        .header("Content-Type", "application/json")
        .body(Body::from(body))
        .unwrap();
    app.clone().oneshot(request).await.unwrap()
}

/// REQ-0040: 利用者が Web UI を開いた時、システムは保存済みの
/// リポジトリ参照とタグの一覧を表示しなければならない。
#[tokio::test]
async fn catalog_lists_stored_repositories() {
    let server = MockServer::start().await;
    let repo = "library/nginx";
    let content = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"x","digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","size":0},"layers":[]}"#.to_vec();
    let digest = digest_of(&content);
    register_anonymous_v2(&server).await;
    register_manifest(&server, repo, "latest", &content).await;

    let (app, _store_dir) = build_app(vec![Upstream::new("docker.io", server.uri())]);

    let request = HttpRequest::builder()
        .uri(format!("/v2/{repo}/manifests/latest?ns=docker.io"))
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let (status, catalog) = get(&app, "/api/catalog").await;
    assert_eq!(status, StatusCode::OK);
    let entries = catalog.as_array().unwrap();
    assert!(
        entries
            .iter()
            .any(|e| { e["repository"] == repo && e["tag"] == "latest" && e["digest"] == digest }),
        "カタログに保存済みのリポジトリ参照・タグが見つからない: {catalog:?}"
    );
}

/// REQ-0041: Web UI が保存済みのイメージを表示する時、システムはその
/// 取得元である上流レジストリを併せて表示しなければならない。
#[tokio::test]
async fn image_entry_shows_source_upstream() {
    let docker_server = MockServer::start().await;
    let ghcr_server = MockServer::start().await;
    let repo = "library/nginx";
    let content = b"{\"schemaVersion\":2,\"layers\":[]}".to_vec();
    register_anonymous_v2(&docker_server).await;
    register_anonymous_v2(&ghcr_server).await;
    register_manifest(&docker_server, repo, "latest", &content).await;
    register_manifest(&ghcr_server, repo, "latest", &content).await;

    let (app, _store_dir) = build_app(vec![
        Upstream::new("docker.io", docker_server.uri()),
        Upstream::new("ghcr.io", ghcr_server.uri()),
    ]);

    // 明示的に ghcr.io を指定して取得する（REQ-0001 の ns ヒント）。
    let request = HttpRequest::builder()
        .uri(format!("/v2/{repo}/manifests/latest?ns=ghcr.io"))
        .body(Body::empty())
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let (_, catalog) = get(&app, "/api/catalog").await;
    let entries = catalog.as_array().unwrap();
    let entry = entries
        .iter()
        .find(|e| e["repository"] == repo && e["tag"] == "latest")
        .expect("カタログにエントリが見つからない");
    assert_eq!(entry["upstream"], "ghcr.io");
}

/// REQ-0042: 利用者が Web UI を開いた時、システムはキャッシュの命中回数と
/// 不命中回数、および保存領域の使用量を表示しなければならない。
#[tokio::test]
async fn stats_endpoint_reports_hit_and_miss() {
    let server = MockServer::start().await;
    let repo = "library/nginx";
    let content = b"blob content for stats test".to_vec();
    let digest = digest_of(&content);
    register_anonymous_v2(&server).await;
    register_blob(&server, repo, &digest, &content).await;

    let (app, _store_dir) = build_app(vec![Upstream::new("docker.io", server.uri())]);

    let (_, before) = get(&app, "/api/stats").await;
    let misses_before = before["blob_misses"].as_u64().unwrap();
    let hits_before = before["blob_hits"].as_u64().unwrap();

    let blob_uri = format!("/v2/{repo}/blobs/{digest}?ns=docker.io");
    let first = HttpRequest::builder()
        .uri(&blob_uri)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(first).await.unwrap().status(),
        StatusCode::OK
    );

    let (_, after_miss) = get(&app, "/api/stats").await;
    assert_eq!(
        after_miss["blob_misses"].as_u64().unwrap(),
        misses_before + 1,
        "1回目の取得が不命中として数えられていない"
    );
    assert!(
        after_miss["storage_bytes_used"].as_u64().unwrap() > 0,
        "blob を保存した後も使用量が0のままになっている"
    );

    let second = HttpRequest::builder()
        .uri(&blob_uri)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(second).await.unwrap().status(),
        StatusCode::OK
    );

    let (_, after_hit) = get(&app, "/api/stats").await;
    assert_eq!(
        after_hit["blob_hits"].as_u64().unwrap(),
        hits_before + 1,
        "2回目の取得（保存済み）が命中として数えられていない"
    );
    assert_eq!(
        after_hit["blob_misses"].as_u64().unwrap(),
        misses_before + 1,
        "2回目の取得で不命中が余分に増えている"
    );
}

/// REQ-0043: システムは Web UI の静的資産を実行ファイルに埋め込み、
/// 追加のファイル配置を伴わずに配信しなければならない。
#[tokio::test]
async fn assets_served_from_embedded_bundle() {
    let (app, _store_dir) = build_app(vec![]);

    let request = HttpRequest::builder().uri("/").body(Body::empty()).unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("Content-Type")
            .and_then(|v| v.to_str().ok()),
        Some("text/html")
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let on_disk = std::fs::read("ui/index.html").unwrap();
    assert_eq!(
        body.as_ref(),
        on_disk.as_slice(),
        "配信された内容が ui/index.html と一致しない（埋め込みが機能していない）"
    );
}

fn metric_value(text: &str, name: &str) -> Option<f64> {
    text.lines().find_map(|line| {
        let rest = line.strip_prefix(name)?;
        let rest = rest.trim_start();
        if !rest.starts_with(|c: char| c.is_ascii_digit() || c == '-') {
            return None;
        }
        rest.split_whitespace().next()?.parse().ok()
    })
}

/// REQ-0044: 監視系がメトリクスの取得を要求した時、システムは
/// Prometheus 形式でキャッシュ統計を返さなければならない。
#[tokio::test]
async fn metrics_endpoint_exposes_prometheus_format() {
    let server = MockServer::start().await;
    let repo = "library/nginx";
    let content_a = b"metrics test blob a".to_vec();
    let content_b = b"metrics test blob b".to_vec();
    let digest_a = digest_of(&content_a);
    let digest_b = digest_of(&content_b);
    register_anonymous_v2(&server).await;
    register_blob(&server, repo, &digest_a, &content_a).await;
    register_blob(&server, repo, &digest_b, &content_b).await;

    let (app, _store_dir) = build_app(vec![Upstream::new("docker.io", server.uri())]);

    // 不命中を1件発生させ、計測対象のメトリクスを登録させる
    // （Prometheus のテキスト出力は、一度も記録されていないメトリクスを
    // 含まない）。
    let first = HttpRequest::builder()
        .uri(format!("/v2/{repo}/blobs/{digest_a}?ns=docker.io"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(first).await.unwrap().status(),
        StatusCode::OK
    );

    let before = HttpRequest::builder()
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let before_response = app.clone().oneshot(before).await.unwrap();
    assert_eq!(before_response.status(), StatusCode::OK);
    let content_type = before_response
        .headers()
        .get("Content-Type")
        .and_then(|v| v.to_str().ok())
        .unwrap()
        .to_string();
    assert!(
        content_type.starts_with("text/plain"),
        "Prometheus 形式の応答は text/plain であるべき: {content_type}"
    );
    let before_text = String::from_utf8(
        axum::body::to_bytes(before_response.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    let before_misses = metric_value(&before_text, "oci_cache_blob_misses_total")
        .expect("oci_cache_blob_misses_total がメトリクス出力に無い");

    // 2件目の不命中（別ダイジェスト）を発生させる。同じテストバイナリ内の
    // 他のテストも同じグローバルレコーダを共有するため、絶対値ではなく
    // このテスト自身が起こした増分だけを確認する。
    let second = HttpRequest::builder()
        .uri(format!("/v2/{repo}/blobs/{digest_b}?ns=docker.io"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(second).await.unwrap().status(),
        StatusCode::OK
    );

    let after = HttpRequest::builder()
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let after_text = String::from_utf8(
        axum::body::to_bytes(app.oneshot(after).await.unwrap().into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    let after_misses = metric_value(&after_text, "oci_cache_blob_misses_total").unwrap();

    assert_eq!(
        after_misses,
        before_misses + 1.0,
        "2件目の不命中がメトリクスへ反映されていない"
    );
}

/// REQ-0017: 利用者が Web UI から再取得を指示した時、システムは有効期間の
/// 残りにかかわらず上流レジストリへ問い合わせ、保存内容を更新しなければ
/// ならない。
#[tokio::test]
async fn force_refresh_bypasses_remaining_ttl() {
    let server = MockServer::start().await;
    let repo = "library/nginx";
    let tag = "latest";
    let content_a = br#"{"schemaVersion":2,"first":true}"#.to_vec();
    let content_b = br#"{"schemaVersion":2,"first":false}"#.to_vec();
    register_anonymous_v2(&server).await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repo}/manifests/{tag}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(content_a.clone()))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    register_manifest(&server, repo, tag, &content_b).await;

    let (app, _store_dir) = build_app(vec![Upstream::new("docker.io", server.uri())]);

    let first = HttpRequest::builder()
        .uri(format!("/v2/{repo}/manifests/{tag}?ns=docker.io"))
        .body(Body::empty())
        .unwrap();
    let first_response = app.clone().oneshot(first).await.unwrap();
    assert_eq!(first_response.status(), StatusCode::OK);
    let first_body = axum::body::to_bytes(first_response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(first_body.as_ref(), content_a.as_slice());

    // 強制再取得（REQ-0017）は有効期間（既定30分）が残っていても上流へ
    // 問い合わせる。
    let refresh_response = post_refresh(&app, "docker.io", repo, tag).await;
    assert_eq!(refresh_response.status(), StatusCode::OK);

    let follow_up = HttpRequest::builder()
        .uri(format!("/v2/{repo}/manifests/{tag}?ns=docker.io"))
        .body(Body::empty())
        .unwrap();
    let follow_up_response = app.oneshot(follow_up).await.unwrap();
    assert_eq!(follow_up_response.status(), StatusCode::OK);
    let follow_up_body = axum::body::to_bytes(follow_up_response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        follow_up_body.as_ref(),
        content_b.as_slice(),
        "強制再取得後も有効期間内の古い内容が返っている"
    );
}

/// REQ-0045: 利用者が強制再取得を指示した時、システムは同一のイメージに
/// 対する次の強制再取得の指示を、設定された抑止時間が経過するまで受け
/// 付けてはならない。
#[tokio::test]
async fn force_refresh_rejected_within_cooldown() {
    let server = MockServer::start().await;
    let repo = "library/nginx";
    let tag = "latest";
    register_anonymous_v2(&server).await;
    register_manifest(&server, repo, tag, b"{\"schemaVersion\":2}").await;

    let (app, _store_dir) = build_app(vec![Upstream::new("docker.io", server.uri())]);

    let first = post_refresh(&app, "docker.io", repo, tag).await;
    assert_eq!(first.status(), StatusCode::OK);

    let second = post_refresh(&app, "docker.io", repo, tag).await;
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    let retry_after: u64 = second
        .headers()
        .get("Retry-After")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .expect("Retry-After ヘッダが無い");
    assert!(retry_after > 0 && retry_after <= 60);
}

/// REQ-0046: システムは強制再取得を受け付けない抑止時間の既定を60秒と
/// しなければならない。
///
/// `RefreshGate` を直接検証する。HTTP 経路（wiremock の実ソケット）を
/// 介すると、`start_paused` の自動時刻送進がバックグラウンドの通信処理
/// と絡んで境界値のタイミングが安定しないため、抑止時間そのものの検証は
/// 時間制御だけで完結させる。経路の結線自体は
/// `force_refresh_rejected_within_cooldown` が確認する。
#[tokio::test(start_paused = true)]
async fn force_refresh_cooldown_default_is_60_seconds() {
    let gate = oci_cache::ui::RefreshGate::default();
    let key = (
        "docker.io".to_string(),
        "library/nginx".to_string(),
        "latest".to_string(),
    );

    assert!(gate.try_acquire(&key).is_ok());

    tokio::time::advance(Duration::from_secs(59)).await;
    assert!(
        gate.try_acquire(&key).is_err(),
        "59秒経過時点ではまだ抑止時間内のはず"
    );

    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(
        gate.try_acquire(&key).is_ok(),
        "61秒経過後は既定の60秒の抑止時間を過ぎているはず"
    );
}

/// `RefreshGate` が抑止時間を過ぎた記録を溜め続けないことを確認する
/// （CodeRabbit レビュー指摘: `post_refresh` は認証を要求せず任意の
/// `repository` / `tag` を受け付けるため、掃除しなければ再起動まで
/// メモリ使用量が増え続ける）。
#[tokio::test(start_paused = true)]
async fn refresh_gate_purges_expired_entries() {
    let gate = oci_cache::ui::RefreshGate::new(Duration::from_secs(1));
    for i in 0..50 {
        let key = (
            "docker.io".to_string(),
            format!("library/repo-{i}"),
            "latest".to_string(),
        );
        assert!(gate.try_acquire(&key).is_ok());
    }
    assert_eq!(gate.active_entries(), 50);

    tokio::time::advance(Duration::from_secs(2)).await;

    // 新しい1件を受け付けるだけで、期限切れの記録がすべて掃除される。
    let fresh_key = (
        "docker.io".to_string(),
        "library/fresh".to_string(),
        "latest".to_string(),
    );
    assert!(gate.try_acquire(&fresh_key).is_ok());
    assert_eq!(
        gate.active_entries(),
        1,
        "抑止時間を過ぎた記録が掃除されずに溜まっている"
    );
}

/// REQ-0047: 強制再取得の抑止時間が経過していない時、システムは Web UI
/// 上で強制再取得の操作を実行できない状態として表示しなければならない。
///
/// 抑止時間をごく短く設定して検証する（`AppState::with_refresh_cooldown`、
/// REQ-0046 の既定値上書きと同じ仕組み）。`tokio::time::pause` は使わない。
/// wiremock は実ソケットで待ち受けるため、一時停止した時刻を自動的に
/// 進める仕組みと組み合わせると、バックグラウンドの通信処理の都合で
/// 境界値のタイミングが安定しないため。
#[tokio::test]
async fn force_refresh_control_disabled_during_cooldown() {
    let server = MockServer::start().await;
    let repo = "library/nginx";
    let tag = "latest";
    let content = b"{\"schemaVersion\":2}".to_vec();
    register_anonymous_v2(&server).await;
    register_manifest(&server, repo, tag, &content).await;

    // `refresh_cooldown_remaining_seconds` は秒単位（整数）で公開するため、
    // 1秒未満の抑止時間では残り時間が切り捨てられて常に0に見えてしまう。
    // テストでも実際の単位と同じ粒度で確認する。
    let cooldown = Duration::from_secs(2);
    let config = RoutingConfig {
        upstreams: vec![Upstream::new("docker.io", server.uri())],
        probe_timeout: Duration::from_secs(5),
        ..Default::default()
    };
    let store_dir = tempfile::tempdir().unwrap();
    let state = AppState::new(config, store_dir.path()).with_refresh_cooldown(cooldown);
    let app = build_router(state);

    // カタログに載せるため、まず通常の取得を行う。
    let initial = HttpRequest::builder()
        .uri(format!("/v2/{repo}/manifests/{tag}?ns=docker.io"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(initial).await.unwrap().status(),
        StatusCode::OK
    );

    let refresh_response = post_refresh(&app, "docker.io", repo, tag).await;
    assert_eq!(refresh_response.status(), StatusCode::OK);

    let (_, during_cooldown) = get(&app, "/api/catalog").await;
    let entry = during_cooldown
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["repository"] == repo && e["tag"] == tag)
        .expect("カタログにエントリが見つからない");
    let remaining = entry["refresh_cooldown_remaining_seconds"]
        .as_u64()
        .unwrap();
    assert!(
        remaining > 0,
        "抑止中は残り時間が正でなければならない: {remaining}"
    );

    tokio::time::sleep(cooldown + Duration::from_millis(100)).await;

    let (_, after_cooldown) = get(&app, "/api/catalog").await;
    let entry_after = after_cooldown
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["repository"] == repo && e["tag"] == tag)
        .expect("カタログにエントリが見つからない");
    assert_eq!(
        entry_after["refresh_cooldown_remaining_seconds"]
            .as_u64()
            .unwrap(),
        0,
        "抑止時間が過ぎた後も操作不可として表示され続けている"
    );
}
