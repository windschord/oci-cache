//! OCI Distribution Specification への準拠のテスト。
//!
//! 対象要求: REQ-0030 〜 REQ-0036（`CLAUDE.md` の実装順序 4.）。
//! テスト名は `docs/requirements/api.yaml` の `verification.ref` と一致させてある。
//!
//! `oci_distribution_spec_pull_suite`（REQ-0030）だけは上流の公式
//! conformance スイート（Go 製、ネットワークアクセスと `go` が必要）を
//! `scripts/conformance.sh` 経由で実行するため `#[ignore]` にしてある。
//! 既定の `cargo test` では走らず、`.github/workflows/conformance.yml` が
//! 別途 `scripts/conformance.sh` を直接呼ぶ。他のテストは wiremock で
//! 上流を再現する高速なテストで、通常の `cargo test` で実行される。

use axum::body::Body;
use axum::http::{Request as HttpRequest, StatusCode};
use oci_cache::routing::{RoutingConfig, Upstream};
use oci_cache::server::{build_router, AppState};
use sha2::{Digest, Sha256};
use std::time::Duration;
use tower::ServiceExt;
use wiremock::matchers::{header_regex, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn digest_of(content: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(content)))
}

/// 認証方式の確認（`GET /v2/`）に challenge の無い成功応答を返すよう登録する。
/// 未登録のままだと wiremock の既定応答（404）になり、匿名上流と誤認せず
/// 取得を失敗させる。
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

fn single_upstream_app(server: &MockServer) -> axum::Router {
    let config = RoutingConfig {
        upstreams: vec![Upstream::new("docker.io", server.uri())],
        probe_timeout: Duration::from_secs(5),
        ..Default::default()
    };
    let store_dir = tempfile::tempdir().unwrap();
    let state = AppState::new(config, store_dir.path());
    build_router(state)
}

/// REQ-0030: OCI Distribution Specification の公式 conformance スイート
/// （取得系）に準拠する。上流（docker.io 等）への実際のネットワーク
/// アクセスと `go` ツールチェーンを要するため `#[ignore]`。実行手順は
/// `scripts/conformance.sh` を参照。CI では `.github/workflows/conformance.yml`
/// が別途このスクリプトを直接呼ぶ（詳細は同ファイルのコメントを参照）。
#[test]
#[ignore = "ネットワークアクセスと go ツールチェーンが必要。scripts/conformance.sh を手動で実行すること"]
fn oci_distribution_spec_pull_suite() {
    let repo_root = env!("CARGO_MANIFEST_DIR");
    let status = std::process::Command::new("./scripts/conformance.sh")
        .current_dir(repo_root)
        .status()
        .expect("scripts/conformance.sh の起動に失敗した");
    assert!(
        status.success(),
        "OCI distribution-spec の conformance スイートが失敗した: {status:?}"
    );
}

/// REQ-0031: システムは書き込み系エンドポイントを実装してはならない。
/// blob アップロード開始・マニフェストの PUT・DELETE のいずれも、成功
/// （2xx）を返してはならない。
#[tokio::test]
async fn push_endpoints_are_absent() {
    let server = MockServer::start().await;
    let app = single_upstream_app(&server);
    let repo = "library/nginx";

    for request in [
        HttpRequest::builder()
            .method("POST")
            .uri(format!("/v2/{repo}/blobs/uploads/"))
            .body(Body::empty())
            .unwrap(),
        HttpRequest::builder()
            .method("PUT")
            .uri(format!("/v2/{repo}/manifests/latest"))
            .body(Body::from("{}"))
            .unwrap(),
        HttpRequest::builder()
            .method("DELETE")
            .uri(format!("/v2/{repo}/manifests/latest"))
            .body(Body::empty())
            .unwrap(),
    ] {
        let response = app.clone().oneshot(request).await.unwrap();
        assert!(
            !response.status().is_success(),
            "書き込み系エンドポイントが成功応答を返している: {:?}",
            response.status()
        );
    }
}

/// REQ-0036: クライアントが書き込み系エンドポイントへ要求を送信した時、
/// システムは HTTP 405 Method Not Allowed を返さなければならない。
#[tokio::test]
async fn push_request_is_rejected_as_not_allowed() {
    let server = MockServer::start().await;
    let app = single_upstream_app(&server);
    let repo = "library/nginx";

    let request = HttpRequest::builder()
        .method("POST")
        .uri(format!("/v2/{repo}/blobs/uploads/"))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
}

/// REQ-0032: マニフェストの応答には Docker-Content-Digest ヘッダを含め、
/// その値を応答本文から計算したダイジェストと一致させなければならない。
#[tokio::test]
async fn content_digest_header_matches_body() {
    let server = MockServer::start().await;
    let repo = "library/nginx";
    let content = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","size":0},"layers":[]}"#.to_vec();
    register_anonymous_v2(&server).await;
    register_manifest(&server, repo, "latest", &content).await;

    let app = single_upstream_app(&server);
    let request = HttpRequest::builder()
        .uri(format!("/v2/{repo}/manifests/latest?ns=docker.io"))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let header = response
        .headers()
        .get("Docker-Content-Digest")
        .and_then(|v| v.to_str().ok())
        .unwrap()
        .to_string();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    // 検証したい相手（応答ヘッダ）の出力を前提に検証対象を組み立てると
    // トートロジーになるため、受信した本文そのものから独立に計算する。
    assert_eq!(header, digest_of(&body));
    assert_eq!(body.as_ref(), content.as_slice());
}

/// REQ-0034: 要求されたマニフェストがマニフェストリストである時、
/// システムはプラットフォームの選択を行わずマニフェストリストのまま
/// 返さなければならない（内容は一切変更されない）。
#[tokio::test]
async fn manifest_list_passed_through_unmodified() {
    let server = MockServer::start().await;
    let repo = "library/nginx";
    // 子マニフェストが実在しなくても、このテストはシステムが本文を
    // そのまま中継することだけを確認する
    let index_content = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:1111111111111111111111111111111111111111111111111111111111111111","size":1,"platform":{"architecture":"amd64","os":"linux"}},{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:2222222222222222222222222222222222222222222222222222222222222222","size":1,"platform":{"architecture":"arm64","os":"linux"}}]}"#.to_vec();
    register_anonymous_v2(&server).await;
    register_manifest(&server, repo, "latest", &index_content).await;

    let app = single_upstream_app(&server);
    let request = HttpRequest::builder()
        .uri(format!("/v2/{repo}/manifests/latest?ns=docker.io"))
        .header(
            "Accept",
            "application/vnd.oci.image.index.v1+json,application/vnd.oci.image.manifest.v1+json",
        )
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("Content-Type")
            .and_then(|v| v.to_str().ok()),
        Some("application/vnd.oci.image.index.v1+json")
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    // プラットフォームを選んで中身を書き換えていないことを、バイト列の
    // 完全一致で確認する(1プラットフォームへの単純化や整形による差異も
    // 見逃さない)。
    assert_eq!(body.as_ref(), index_content.as_slice());
}

/// REQ-0033: クライアントが Accept ヘッダで受け入れ可能なメディアタイプを
/// 指定した時、システムは指定された範囲に含まれるメディアタイプで
/// マニフェストを返さなければならない。
#[tokio::test]
async fn manifest_media_type_negotiation() {
    let server = MockServer::start().await;
    let repo = "library/nginx";
    register_anonymous_v2(&server).await;

    let oci_content = br#"{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","size":0},"layers":[]}"#.to_vec();
    let docker_content = br#"{"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.v2+json","config":{"mediaType":"application/vnd.docker.container.image.v1+json","digest":"sha256:0000000000000000000000000000000000000000000000000000000000000001","size":0},"layers":[]}"#.to_vec();

    // 同じ参照でも、クライアントが要求したメディアタイプによって上流への
    // 問い合わせを分ける(≒「指定された範囲」を実際に使っていることの
    // 証拠)。上流モックは Accept ヘッダに応じて別の内容を返す。
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repo}/manifests/multi")))
        .and(header_regex("accept", r"oci\.image\.manifest"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(oci_content.clone()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repo}/manifests/multi")))
        .and(header_regex(
            "accept",
            r"docker\.distribution\.manifest\.v2",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(docker_content.clone()))
        .mount(&server)
        .await;

    let app = single_upstream_app(&server);

    let oci_only_request = HttpRequest::builder()
        .uri(format!("/v2/{repo}/manifests/multi?ns=docker.io"))
        .header("Accept", "application/vnd.oci.image.manifest.v1+json")
        .body(Body::empty())
        .unwrap();
    let oci_response = app.clone().oneshot(oci_only_request).await.unwrap();
    assert_eq!(oci_response.status(), StatusCode::OK);
    assert_eq!(
        oci_response
            .headers()
            .get("Content-Type")
            .and_then(|v| v.to_str().ok()),
        Some("application/vnd.oci.image.manifest.v1+json")
    );
    let oci_body = axum::body::to_bytes(oci_response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(oci_body.as_ref(), oci_content.as_slice());

    let docker_only_request = HttpRequest::builder()
        .uri(format!("/v2/{repo}/manifests/multi?ns=docker.io"))
        .header(
            "Accept",
            "application/vnd.docker.distribution.manifest.v2+json",
        )
        .body(Body::empty())
        .unwrap();
    let docker_response = app.clone().oneshot(docker_only_request).await.unwrap();
    assert_eq!(docker_response.status(), StatusCode::OK);
    assert_eq!(
        docker_response
            .headers()
            .get("Content-Type")
            .and_then(|v| v.to_str().ok()),
        Some("application/vnd.docker.distribution.manifest.v2+json")
    );
    let docker_body = axum::body::to_bytes(docker_response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(docker_body.as_ref(), docker_content.as_slice());

    // システムが解釈できないメディアタイプしか受け入れないクライアントには、
    // 範囲外のマニフェストを返すのではなく 406 を返す。
    let unsupported_request = HttpRequest::builder()
        .uri(format!("/v2/{repo}/manifests/multi?ns=docker.io"))
        .header("Accept", "text/plain")
        .body(Body::empty())
        .unwrap();
    let unsupported_response = app.oneshot(unsupported_request).await.unwrap();
    assert_eq!(unsupported_response.status(), StatusCode::NOT_ACCEPTABLE);
}

/// 上流に存在しないマニフェスト・blob を要求した時、システムは 502（上流
/// 障害）ではなく 404 を返さなければならない（OCI Distribution
/// Specification の要求。CodeRabbit レビュー指摘: 存在しないタグへの要求を
/// 502 として返すと、クライアントが一時的な障害として再試行してしまう）。
#[tokio::test]
async fn upstream_not_found_is_reported_as_404_not_502() {
    let server = MockServer::start().await;
    let repo = "library/nginx";
    register_anonymous_v2(&server).await;

    Mock::given(method("GET"))
        .and(path(format!("/v2/{repo}/manifests/missing")))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "errors": [{"code": "MANIFEST_UNKNOWN", "message": "manifest unknown"}],
        })))
        .mount(&server)
        .await;
    let missing_digest = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repo}/blobs/{missing_digest}")))
        .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
            "errors": [{"code": "BLOB_UNKNOWN", "message": "blob unknown"}],
        })))
        .mount(&server)
        .await;

    let app = single_upstream_app(&server);

    let manifest_request = HttpRequest::builder()
        .uri(format!("/v2/{repo}/manifests/missing?ns=docker.io"))
        .body(Body::empty())
        .unwrap();
    let manifest_response = app.clone().oneshot(manifest_request).await.unwrap();
    assert_eq!(manifest_response.status(), StatusCode::NOT_FOUND);

    let blob_request = HttpRequest::builder()
        .uri(format!("/v2/{repo}/blobs/{missing_digest}?ns=docker.io"))
        .body(Body::empty())
        .unwrap();
    let blob_response = app.oneshot(blob_request).await.unwrap();
    assert_eq!(blob_response.status(), StatusCode::NOT_FOUND);
}

/// REQ-0035: クライアントが Range ヘッダを指定して blob を要求した時、
/// システムは指定された範囲のみを返さなければならない。
#[tokio::test]
async fn range_request_returns_partial_content() {
    let server = MockServer::start().await;
    let repo = "library/nginx";
    let content = b"0123456789abcdef".to_vec();
    let digest = digest_of(&content);
    register_anonymous_v2(&server).await;
    register_blob(&server, repo, &digest, &content).await;

    let app = single_upstream_app(&server);
    let request = HttpRequest::builder()
        .uri(format!("/v2/{repo}/blobs/{digest}?ns=docker.io"))
        .header("Range", "bytes=2-5")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        response
            .headers()
            .get("Content-Range")
            .and_then(|v| v.to_str().ok()),
        Some("bytes 2-5/16")
    );
    assert_eq!(
        response
            .headers()
            .get("Content-Length")
            .and_then(|v| v.to_str().ok()),
        Some("4")
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), &content[2..=5]);
}
