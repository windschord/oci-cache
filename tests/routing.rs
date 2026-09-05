//! ルーティング解決のテスト。
//!
//! 対象要求: REQ-0001 / REQ-0002 / REQ-0003 / REQ-0007 / REQ-0009 / REQ-0056〜REQ-0059。
//! テスト名は `docs/requirements/routing.yaml` の `verification.ref` と一致させてある。
//!
//! 上流レジストリは wiremock でモックし、`GET /v2/<repository>/tags/list` への
//! 応答でリポジトリ参照の存在確認（`UpstreamProbe`）を再現する。

use std::sync::Arc;
use std::time::Duration;

use oci_cache::routing::{
    HttpUpstreamProbe, InMemoryRoutingMemo, ResolutionSource, Router, RoutingConfig, Upstream,
};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn spawn_upstream(id: &str) -> (Upstream, MockServer) {
    let server = MockServer::start().await;
    (Upstream::new(id, server.uri()), server)
}

async fn register_found(server: &MockServer, repository: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repository}/tags/list")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": repository,
            "tags": [],
        })))
        .mount(server)
        .await;
}

async fn register_not_found(server: &MockServer, repository: &str) {
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repository}/tags/list")))
        .respond_with(ResponseTemplate::new(404))
        .mount(server)
        .await;
}

async fn register_delayed_found(server: &MockServer, repository: &str, delay: Duration) {
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repository}/tags/list")))
        .respond_with(ResponseTemplate::new(200).set_delay(delay))
        .mount(server)
        .await;
}

fn probe() -> HttpUpstreamProbe {
    HttpUpstreamProbe::new(reqwest::Client::new())
}

/// REQ-0001: 名前空間ヒントが示す上流レジストリのみに問い合わせる。
#[tokio::test]
async fn ns_hint_selects_single_upstream() {
    let (docker, docker_server) = spawn_upstream("docker.io").await;
    let (ghcr, ghcr_server) = spawn_upstream("ghcr.io").await;
    let (quay, quay_server) = spawn_upstream("quay.io").await;
    let repo = "library/nginx";
    register_found(&docker_server, repo).await;
    register_found(&ghcr_server, repo).await;
    register_found(&quay_server, repo).await;

    let config = RoutingConfig {
        upstreams: vec![docker, ghcr, quay],
        probe_timeout: Duration::from_secs(5),
    };
    let router = Router::new(config, probe(), InMemoryRoutingMemo::default());

    let resolution = router.resolve(repo, Some("ghcr.io")).await.unwrap();

    assert_eq!(resolution.upstream, "ghcr.io");
    assert_eq!(resolution.source, ResolutionSource::NsHint);
    assert_eq!(ghcr_server.received_requests().await.unwrap().len(), 1);
    assert!(docker_server.received_requests().await.unwrap().is_empty());
    assert!(quay_server.received_requests().await.unwrap().is_empty());
}

/// REQ-0002: 名前空間ヒントもルーティングメモも無い時、設定順序で問い合わせ、
/// 最初に成功した上流の結果を返す。
#[tokio::test]
async fn ordered_fallback_returns_first_success() {
    let (docker, docker_server) = spawn_upstream("docker.io").await;
    let (ghcr, ghcr_server) = spawn_upstream("ghcr.io").await;
    let (quay, quay_server) = spawn_upstream("quay.io").await;
    let repo = "library/nginx";
    register_not_found(&docker_server, repo).await;
    register_found(&ghcr_server, repo).await;
    // quay.io には何も登録しない。ghcr.io で成功した時点で問い合わせが
    // 打ち切られることを、リクエスト件数 0 で確かめる

    let config = RoutingConfig {
        upstreams: vec![docker, ghcr, quay],
        probe_timeout: Duration::from_secs(5),
    };
    let router = Router::new(config, probe(), InMemoryRoutingMemo::default());

    let resolution = router.resolve(repo, None).await.unwrap();

    assert_eq!(resolution.upstream, "ghcr.io");
    assert_eq!(resolution.source, ResolutionSource::Fallback);
    assert!(quay_server.received_requests().await.unwrap().is_empty());
}

/// REQ-0003: 順序フォールバックで上流が確定した後は、同一リポジトリ参照への
/// 要求で他の上流を再探索しない。
#[tokio::test]
async fn routing_memo_skips_probe_on_second_request() {
    let (docker, docker_server) = spawn_upstream("docker.io").await;
    let (ghcr, ghcr_server) = spawn_upstream("ghcr.io").await;
    let (quay, quay_server) = spawn_upstream("quay.io").await;
    let repo = "library/nginx";
    register_found(&docker_server, repo).await;

    let config = RoutingConfig {
        upstreams: vec![docker, ghcr, quay],
        probe_timeout: Duration::from_secs(5),
    };
    let router = Router::new(config, probe(), InMemoryRoutingMemo::default());

    let first = router.resolve(repo, None).await.unwrap();
    assert_eq!(first.source, ResolutionSource::Fallback);

    let second = router.resolve(repo, None).await.unwrap();
    assert_eq!(second.upstream, "docker.io");
    assert_eq!(second.source, ResolutionSource::Memo);

    // ghcr.io と quay.io は一度も探索されていない（順序フォールバックの再実行が無い）
    assert!(ghcr_server.received_requests().await.unwrap().is_empty());
    assert!(quay_server.received_requests().await.unwrap().is_empty());
}

/// REQ-0007: 同一のリポジトリ参照が複数の上流に存在する時、設定順序が最も
/// 先である上流の応答を返す。
#[tokio::test]
async fn name_collision_resolved_by_configured_order() {
    let (docker, docker_server) = spawn_upstream("docker.io").await;
    let (ghcr, ghcr_server) = spawn_upstream("ghcr.io").await;
    let (quay, quay_server) = spawn_upstream("quay.io").await;
    let repo = "library/nginx";
    register_found(&docker_server, repo).await;
    register_found(&ghcr_server, repo).await;

    let config = RoutingConfig {
        upstreams: vec![docker, ghcr, quay],
        probe_timeout: Duration::from_secs(5),
    };
    let router = Router::new(config, probe(), InMemoryRoutingMemo::default());

    let resolution = router.resolve(repo, None).await.unwrap();

    assert_eq!(resolution.upstream, "docker.io");
    // 設定順序で先に成功した docker.io で打ち切られ、ghcr.io へは問い合わせない
    assert!(ghcr_server.received_requests().await.unwrap().is_empty());
    assert!(quay_server.received_requests().await.unwrap().is_empty());
}

/// REQ-0009: 運用者が設定した順序を上流への問い合わせ順序として使う。
#[tokio::test]
async fn configured_upstream_order_is_used() {
    let (docker, docker_server) = spawn_upstream("docker.io").await;
    let (ghcr, ghcr_server) = spawn_upstream("ghcr.io").await;
    let (quay, quay_server) = spawn_upstream("quay.io").await;
    let repo = "library/nginx";
    register_found(&docker_server, repo).await;
    register_found(&quay_server, repo).await;

    // 既定 (docker.io → ghcr.io → quay.io) とは異なる順序を設定する
    let config = RoutingConfig {
        upstreams: vec![quay, ghcr, docker],
        probe_timeout: Duration::from_secs(5),
    };
    let router = Router::new(config, probe(), InMemoryRoutingMemo::default());

    let resolution = router.resolve(repo, None).await.unwrap();

    assert_eq!(resolution.upstream, "quay.io");
    // 設定順序で quay.io が先頭に来ているため、docker.io へは問い合わせない
    assert!(docker_server.received_requests().await.unwrap().is_empty());
    let _ = ghcr_server; // ghcr.io には何も登録していないため、成功しても失敗しても結果は変わらない
}

/// REQ-0056: 記録された上流が不存在を返した時、記録を破棄して順序
/// フォールバックを再実行する。
#[tokio::test]
async fn routing_memo_discarded_when_recorded_upstream_misses() {
    let (docker, docker_server) = spawn_upstream("docker.io").await;
    let (ghcr, ghcr_server) = spawn_upstream("ghcr.io").await;
    let (quay, quay_server) = spawn_upstream("quay.io").await;
    let repo = "library/nginx";
    register_found(&docker_server, repo).await;

    let config = RoutingConfig {
        upstreams: vec![docker, ghcr, quay],
        probe_timeout: Duration::from_secs(5),
    };
    let router = Router::new(config, probe(), InMemoryRoutingMemo::default());

    let first = router.resolve(repo, None).await.unwrap();
    assert_eq!(first.upstream, "docker.io");

    // docker.io からイメージが削除された状況を再現し、ghcr.io に用意する
    docker_server.reset().await;
    register_not_found(&docker_server, repo).await;
    register_found(&ghcr_server, repo).await;

    let second = router.resolve(repo, None).await.unwrap();

    assert_eq!(second.upstream, "ghcr.io");
    assert_eq!(second.source, ResolutionSource::Fallback);
    assert!(quay_server.received_requests().await.unwrap().is_empty());
}

/// REQ-0057: 設定された待ち時間を超えて応答しない上流は失敗として扱い、
/// 次の上流へ進める。
#[tokio::test]
async fn unresponsive_upstream_advances_to_next() {
    let (docker, docker_server) = spawn_upstream("docker.io").await;
    let (ghcr, ghcr_server) = spawn_upstream("ghcr.io").await;
    let (quay, quay_server) = spawn_upstream("quay.io").await;
    let repo = "library/nginx";
    // 設定した待ち時間より十分長く応答を遅らせ、打ち切りの対象にする
    register_delayed_found(&docker_server, repo, Duration::from_millis(500)).await;
    register_found(&ghcr_server, repo).await;

    let config = RoutingConfig {
        upstreams: vec![docker, ghcr, quay],
        // テストを高速に保つため既定値 (5秒) より短い待ち時間を使う
        probe_timeout: Duration::from_millis(50),
    };
    let router = Router::new(config, probe(), InMemoryRoutingMemo::default());

    let resolution = router.resolve(repo, None).await.unwrap();

    assert_eq!(resolution.upstream, "ghcr.io");
    assert!(quay_server.received_requests().await.unwrap().is_empty());
}

/// REQ-0058: 上流の設定順序が変更された時、ルーティングメモの記録を破棄し、
/// 以降の要求では順序フォールバックを再実行する。
#[tokio::test]
async fn routing_memo_discarded_when_upstream_order_changes() {
    let (docker, docker_server) = spawn_upstream("docker.io").await;
    let (ghcr, ghcr_server) = spawn_upstream("ghcr.io").await;
    let (quay, quay_server) = spawn_upstream("quay.io").await;
    let repo = "library/nginx";
    register_found(&docker_server, repo).await;
    register_found(&ghcr_server, repo).await;

    // 設定の再読み込みを、同じメモを共有した2つの Router で表現する
    let memo = Arc::new(InMemoryRoutingMemo::default());

    let config_a = RoutingConfig {
        upstreams: vec![docker.clone(), ghcr.clone(), quay.clone()],
        probe_timeout: Duration::from_secs(5),
    };
    let router_a = Router::new(config_a, probe(), memo.clone());
    let first = router_a.resolve(repo, None).await.unwrap();
    assert_eq!(first.upstream, "docker.io");

    // docker.io と ghcr.io の順序を入れ替える
    let config_b = RoutingConfig {
        upstreams: vec![ghcr, docker, quay],
        probe_timeout: Duration::from_secs(5),
    };
    let router_b = Router::new(config_b, probe(), memo);
    let second = router_b.resolve(repo, None).await.unwrap();

    assert_eq!(second.upstream, "ghcr.io");
    // メモが破棄されていなければ docker.io の記録が使われ、ghcr.io へは
    // 問い合わせが発生しないはず
    assert!(!ghcr_server.received_requests().await.unwrap().is_empty());
    let _ = quay_server;
}

/// REQ-0059: 上流レジストリへの問い合わせの既定の待ち時間は5秒。
#[test]
fn upstream_probe_timeout_default_is_5_seconds() {
    assert_eq!(
        RoutingConfig::default().probe_timeout,
        Duration::from_secs(5)
    );
}
