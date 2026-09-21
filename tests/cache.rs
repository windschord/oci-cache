//! 上流からの取得と中継のテスト。
//!
//! このフェーズで対象とする要求: REQ-0010 / REQ-0051（`CLAUDE.md` の実装順序 2.）。
//! `tests/cache.rs::blob_served_from_store_without_upstream_call` は
//! `docs/requirements/generated/traceability.md` の REQ-0010 の検証手段。
//!
//! 上流レジストリは wiremock でモックし、`GET /v2/<repository>/blobs/<digest>`
//! への応答を blob 本体として扱う。

use oci_cache::cache::{BlobCache, OciBlobSource};
use oci_cache::routing::Upstream;
use sha2::{Digest, Sha256};
use tempfile::tempdir;
use wiremock::matchers::{method, path};
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

/// REQ-0010: 上流レジストリから取得した blob はダイジェストを鍵として
/// 保存され、以降の同一ダイジェストへの要求は上流レジストリへ問い合わせず
/// 保存内容から応答する。
#[tokio::test]
async fn blob_served_from_store_without_upstream_call() {
    let server = MockServer::start().await;
    let repository = "library/nginx";
    let content = b"this is a fake layer blob for the test".to_vec();
    let digest = digest_of(&content);
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
