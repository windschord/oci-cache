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
    HttpUpstreamProbe, InMemoryRoutingMemo, ProbeOutcome, ResolutionSource, Router, RoutingConfig,
    RoutingError, RoutingMemoStore, Upstream, UpstreamProbe,
};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

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

/// 到達不能な上流が1件でもあれば「すべての上流で不存在」とは報告しない
/// （CodeRabbit レビュー指摘: Unreachable と NotFound の混同）。
#[tokio::test]
async fn fallback_reports_unreachable_when_some_upstream_times_out() {
    let (docker, docker_server) = spawn_upstream("docker.io").await;
    let (ghcr, ghcr_server) = spawn_upstream("ghcr.io").await;
    let repo = "library/nginx";
    register_delayed_found(&docker_server, repo, Duration::from_millis(200)).await;
    register_not_found(&ghcr_server, repo).await;

    let config = RoutingConfig {
        upstreams: vec![docker, ghcr],
        probe_timeout: Duration::from_millis(50),
    };
    let router = Router::new(config, probe(), InMemoryRoutingMemo::default());

    let error = router.resolve(repo, None).await.unwrap_err();

    assert!(matches!(error, RoutingError::Unreachable(_)));
}

/// すべての上流が明確に不存在を返した時だけ「不存在」を報告する。
#[tokio::test]
async fn fallback_reports_not_found_when_all_upstreams_miss() {
    let (docker, docker_server) = spawn_upstream("docker.io").await;
    let (ghcr, ghcr_server) = spawn_upstream("ghcr.io").await;
    let repo = "library/nginx";
    register_not_found(&docker_server, repo).await;
    register_not_found(&ghcr_server, repo).await;

    let config = RoutingConfig {
        upstreams: vec![docker, ghcr],
        probe_timeout: Duration::from_secs(5),
    };
    let router = Router::new(config, probe(), InMemoryRoutingMemo::default());

    let error = router.resolve(repo, None).await.unwrap_err();

    assert!(matches!(error, RoutingError::NotFoundOnAnyUpstream));
}

/// 設定再読み込み中（新しい `Router` が作られた後）に、探索を続けていた古い
/// `Router` の結果がメモへ書き戻らないことを確かめる
/// （CodeRabbit レビュー指摘: 世代をまたいだ書き込みのレース）。
#[tokio::test]
async fn routing_memo_rejects_write_from_stale_generation() {
    let (docker, docker_server) = spawn_upstream("docker.io").await;
    let (ghcr, _ghcr_server) = spawn_upstream("ghcr.io").await;
    let repo = "library/nginx";
    // 旧 Router の探索を長引かせ、その間に設定の再読み込みを起こす
    register_delayed_found(&docker_server, repo, Duration::from_millis(150)).await;

    let memo = Arc::new(InMemoryRoutingMemo::default());

    let config_a = RoutingConfig {
        upstreams: vec![docker.clone(), ghcr.clone()],
        probe_timeout: Duration::from_secs(5),
    };
    let router_a = Router::new(config_a, probe(), memo.clone());

    // 旧 Router の解決を開始するが、まだ完了させない
    let stale_resolution = tokio::spawn({
        let repo = repo.to_string();
        async move { router_a.resolve(&repo, None).await }
    });

    // 旧 Router がまだ探索中のうちに、設定順序を変えて Router を作り直す
    tokio::time::sleep(Duration::from_millis(30)).await;
    let config_b = RoutingConfig {
        upstreams: vec![ghcr, docker],
        probe_timeout: Duration::from_secs(5),
    };
    let _router_b = Router::new(config_b, probe(), memo.clone());

    // 旧 Router の探索自体は成功するが、もう有効でない世代の書き込みは
    // 反映されない
    let stale_result = stale_resolution.await.unwrap().unwrap();
    assert_eq!(stale_result.upstream, "docker.io");
    assert!(memo.get(repo).is_none());
}

struct MissingAuthorizationHeader;

impl wiremock::Match for MissingAuthorizationHeader {
    fn matches(&self, request: &Request) -> bool {
        !request.headers.contains_key("authorization")
    }
}

/// 401 に対して `WWW-Authenticate` の Bearer challenge を解決し、トークンを
/// 使って再試行する。docker.io は匿名 pull でもこの手順を要求するため、これが
/// 無いと既定の最優先上流（docker.io）が実質的に機能しない
/// （CodeRabbit レビュー指摘）。
#[tokio::test]
async fn probe_retries_after_bearer_challenge() {
    let server = MockServer::start().await;
    let repo = "library/nginx";
    let realm = format!("{}/token", server.uri());

    Mock::given(method("GET"))
        .and(path(format!("/v2/{repo}/tags/list")))
        .and(MissingAuthorizationHeader)
        .respond_with(ResponseTemplate::new(401).insert_header(
            "WWW-Authenticate",
            format!(
                r#"Bearer realm="{realm}",service="registry.docker.io",scope="repository:{repo}:pull""#
            ),
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
        .and(path(format!("/v2/{repo}/tags/list")))
        .and(header("authorization", "Bearer test-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "name": repo,
            "tags": [],
        })))
        .mount(&server)
        .await;

    let upstream = Upstream::new("docker.io", server.uri());
    let outcome = probe().probe(&upstream, repo).await;

    assert_eq!(outcome, ProbeOutcome::Found);
}

/// `WWW-Authenticate` の `realm` が上流の設定（`base_url` / `token_host`）と
/// 異なるホストを指す場合、トークンを取得せず `Unreachable` として扱う
/// （CodeRabbit レビュー指摘: 上流が返す `realm` を無条件に信頼すると、
/// 設定した上流1つが悪意を持つだけでこのサーバーに任意のホストへ
/// リクエストさせられる SSRF になる）。
#[tokio::test]
async fn probe_rejects_bearer_realm_with_untrusted_host() {
    let server = MockServer::start().await;
    let attacker = MockServer::start().await;
    let repo = "library/nginx";
    let forged_realm = format!("{}/token", attacker.uri());

    Mock::given(method("GET"))
        .and(path(format!("/v2/{repo}/tags/list")))
        .respond_with(ResponseTemplate::new(401).insert_header(
            "WWW-Authenticate",
            format!(
                r#"Bearer realm="{forged_realm}",service="registry.docker.io",scope="repository:{repo}:pull""#
            ),
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
    let outcome = probe().probe(&upstream, repo).await;

    assert_eq!(outcome, ProbeOutcome::Unreachable);
    assert!(attacker.received_requests().await.unwrap().is_empty());
}

/// `activate_order` は指紋が変わるたびに世代を進め、同じ指紋の再呼び出しでは
/// 進めない。
#[test]
fn activate_order_increments_generation_for_distinct_fingerprints() {
    let memo = InMemoryRoutingMemo::default();

    let g1 = memo.activate_order("docker.io\u{0}ghcr.io");
    let g2 = memo.activate_order("docker.io\u{0}ghcr.io");
    let g3 = memo.activate_order("ghcr.io\u{0}docker.io");

    assert_eq!(g1, g2);
    assert_ne!(g2, g3);
}

/// `activate_order` を異なる指紋で並行に呼び出しても、返る世代番号が
/// 衝突しない（CodeRabbit レビュー指摘: 指紋の比較・消去・採番を別々の
/// 呼び出しに分けていた旧実装は、異なる設定順序で `Router::new` が並行に
/// 呼ばれた際、互いの消去より前に指紋比較が行われることで同じ世代を
/// 採番しうる欠陥があった。`std::sync::Barrier` で複数スレッドの呼び出し
/// 開始を揃え、この欠陥が再現する条件を強制的に作る）。
#[test]
fn concurrent_activate_order_calls_yield_distinct_generations() {
    use std::sync::Barrier;
    use std::thread;

    let memo = Arc::new(InMemoryRoutingMemo::default());
    let fingerprints = ["a\u{0}b", "b\u{0}a", "a\u{0}b\u{0}c", "c\u{0}b\u{0}a"];
    let barrier = Arc::new(Barrier::new(fingerprints.len()));

    let handles: Vec<_> = fingerprints
        .iter()
        .map(|fingerprint| {
            let memo = memo.clone();
            let barrier = barrier.clone();
            let fingerprint = fingerprint.to_string();
            thread::spawn(move || {
                barrier.wait();
                memo.activate_order(&fingerprint)
            })
        })
        .collect();

    let mut generations: Vec<u64> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    generations.sort_unstable();
    generations.dedup();

    assert_eq!(generations.len(), fingerprints.len());
}
