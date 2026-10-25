//! Web UI と観測（REQ-0040 〜 REQ-0047 / REQ-0017 / REQ-0044）。
//!
//! `crate::server::AppState` が持つ状態（`crate::storage::Layout` による
//! 保持内容の記録、`crate::cache::ManifestCache` の強制再取得）をそのまま
//! 使い、Web UI 向けの API（カタログ・統計・メトリクス・強制再取得）と
//! 静的資産の配信（REQ-0043）をここに集める。`crate::server` は OCI
//! Distribution Specification のエンドポイントに専念させるため、分離した。
//!
//! Web UI 自体（`ui/` 配下）は依存を持たない静的資産として実行ファイルへ
//! 埋め込む（REQ-0043）。これらの API を fetch して表示する。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use axum::extract::{Json, State};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};
use rust_embed::Embed;
use serde::{Deserialize, Serialize};
use tokio::time::{Duration, Instant};

use crate::server::AppState;
use crate::storage::Layout;

const CONTENT_TYPE: &str = "Content-Type";
const RETRY_AFTER: &str = "Retry-After";

/// `ui/` 配下の静的資産（REQ-0043）。実行ファイルへ埋め込む。デバッグ
/// ビルドでは `rust-embed` の既定動作によりファイルシステムから都度読む
/// ため、UI の見た目を調整するたびに再ビルドする必要が無い。
#[derive(Embed)]
#[folder = "ui/"]
struct UiAssets;

/// 全要求の計測対象ごとの命中・不命中（REQ-0042）。`AppState` が1つ持ち、
/// サーバーの生存期間だけ数える（再起動で失われる。永続化は要求されて
/// いない）。
#[derive(Default)]
pub struct CacheStats {
    blob_hits: AtomicU64,
    blob_misses: AtomicU64,
    manifest_hits: AtomicU64,
    manifest_misses: AtomicU64,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct CacheStatsSnapshot {
    pub blob_hits: u64,
    pub blob_misses: u64,
    pub manifest_hits: u64,
    pub manifest_misses: u64,
}

impl CacheStats {
    /// 上流へ問い合わせずに応答できた（`hit`）か、問い合わせが必要だった
    /// （`miss`）かを blob 1件について記録する。あわせて Prometheus の
    /// グローバルレコーダへも計上する（REQ-0044）。
    pub fn record_blob(&self, hit: bool) {
        if hit {
            self.blob_hits.fetch_add(1, Ordering::Relaxed);
            metrics::counter!("oci_cache_blob_hits_total").increment(1);
        } else {
            self.blob_misses.fetch_add(1, Ordering::Relaxed);
            metrics::counter!("oci_cache_blob_misses_total").increment(1);
        }
    }

    /// マニフェスト1件について同様に記録する。
    pub fn record_manifest(&self, hit: bool) {
        if hit {
            self.manifest_hits.fetch_add(1, Ordering::Relaxed);
            metrics::counter!("oci_cache_manifest_hits_total").increment(1);
        } else {
            self.manifest_misses.fetch_add(1, Ordering::Relaxed);
            metrics::counter!("oci_cache_manifest_misses_total").increment(1);
        }
    }

    pub fn snapshot(&self) -> CacheStatsSnapshot {
        CacheStatsSnapshot {
            blob_hits: self.blob_hits.load(Ordering::Relaxed),
            blob_misses: self.blob_misses.load(Ordering::Relaxed),
            manifest_hits: self.manifest_hits.load(Ordering::Relaxed),
            manifest_misses: self.manifest_misses.load(Ordering::Relaxed),
        }
    }
}

/// `(upstream, repository, tag)` ごとの強制再取得の抑止（REQ-0045）。
pub struct RefreshGate {
    cooldown: Duration,
    last_refresh: Mutex<HashMap<(String, String, String), Instant>>,
}

impl RefreshGate {
    /// REQ-0046: 既定の抑止時間は60秒。
    pub const DEFAULT_COOLDOWN: Duration = Duration::from_secs(60);

    pub fn new(cooldown: Duration) -> Self {
        Self {
            cooldown,
            last_refresh: Mutex::new(HashMap::new()),
        }
    }

    /// REQ-0045: 抑止時間内なら記録を変えずに残り時間を返す
    /// （`Err`）。抑止時間外なら現在時刻を記録して受け付ける（`Ok`）。
    pub fn try_acquire(&self, key: &(String, String, String)) -> Result<(), Duration> {
        let now = Instant::now();
        let mut guard = self.last_refresh.lock().unwrap();
        if let Some(last) = guard.get(key) {
            let elapsed = now.saturating_duration_since(*last);
            if elapsed < self.cooldown {
                return Err(self.cooldown - elapsed);
            }
        }
        guard.insert(key.clone(), now);
        Ok(())
    }

    /// REQ-0047: 現在の抑止残り時間。抑止中でなければ `Duration::ZERO`。
    /// `try_acquire` と異なり記録を変えない（表示のためだけに呼べる）。
    pub fn remaining(&self, key: &(String, String, String)) -> Duration {
        let now = Instant::now();
        let guard = self.last_refresh.lock().unwrap();
        match guard.get(key) {
            Some(last) => {
                let elapsed = now.saturating_duration_since(*last);
                self.cooldown.saturating_sub(elapsed)
            }
            None => Duration::ZERO,
        }
    }
}

impl Default for RefreshGate {
    fn default() -> Self {
        Self::new(Self::DEFAULT_COOLDOWN)
    }
}

/// Prometheus のグローバルレコーダは1プロセスに1個しか設定できない
/// （`metrics::set_global_recorder` が2回目以降はエラーを返す）。
/// 1つのテストバイナリ内で複数の `AppState` を作っても同じハンドルを
/// 使い回すよう、プロセス単位で一度だけ初期化する。
static PROMETHEUS_HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

pub(crate) fn prometheus_handle() -> PrometheusHandle {
    PROMETHEUS_HANDLE
        .get_or_init(|| {
            PrometheusBuilder::new()
                .install_recorder()
                .expect("Prometheus レコーダの初期化に失敗した")
        })
        .clone()
}

/// `ui/` 配下の静的資産を配信する（REQ-0043）。`/v2/`・`/api/`・`/metrics`
/// に当たらない要求はすべてここへフォールバックする。ルート（`/`）は
/// `index.html` に読み替える。
pub(crate) async fn serve_asset(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    match UiAssets::get(path) {
        Some(file) => {
            let mime = file.metadata.mimetype().to_string();
            ([(CONTENT_TYPE, mime)], file.data.into_owned()).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Serialize)]
struct CatalogEntryJson {
    upstream: String,
    repository: String,
    tag: String,
    digest: String,
    /// REQ-0047: 0 なら強制再取得を受け付けられる状態。
    refresh_cooldown_remaining_seconds: u64,
}

/// REQ-0040 / REQ-0041: 保存済みのリポジトリ参照・タグと、その取得元の
/// 上流の一覧。
pub(crate) async fn get_catalog(State(state): State<std::sync::Arc<AppState>>) -> Response {
    let entries = match state.layout().catalog().await {
        Ok(entries) => entries,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let json: Vec<CatalogEntryJson> = entries
        .into_iter()
        .map(|entry| {
            let key = (
                entry.upstream_id.clone(),
                entry.repository.clone(),
                entry.tag.clone(),
            );
            let remaining = state.refresh_gate().remaining(&key).as_secs();
            CatalogEntryJson {
                upstream: entry.upstream_id,
                repository: entry.repository,
                tag: entry.tag,
                digest: entry.digest,
                refresh_cooldown_remaining_seconds: remaining,
            }
        })
        .collect();
    axum::Json(json).into_response()
}

#[derive(Serialize)]
struct StatsJson {
    blob_hits: u64,
    blob_misses: u64,
    manifest_hits: u64,
    manifest_misses: u64,
    storage_bytes_used: u64,
}

/// REQ-0042: キャッシュの命中・不命中回数と保存領域の使用量。
pub(crate) async fn get_stats(State(state): State<std::sync::Arc<AppState>>) -> Response {
    let snapshot = state.stats().snapshot();
    let storage_bytes_used = crate::storage::blobs_disk_usage(state.root())
        .await
        .unwrap_or(0);
    axum::Json(StatsJson {
        blob_hits: snapshot.blob_hits,
        blob_misses: snapshot.blob_misses,
        manifest_hits: snapshot.manifest_hits,
        manifest_misses: snapshot.manifest_misses,
        storage_bytes_used,
    })
    .into_response()
}

/// REQ-0044: 監視系（Prometheus / VictoriaMetrics）向けの公開。
pub(crate) async fn get_metrics(State(state): State<std::sync::Arc<AppState>>) -> Response {
    let body = state.prometheus_handle().render();
    ([(CONTENT_TYPE, "text/plain; version=0.0.4")], body).into_response()
}

#[derive(Deserialize)]
pub(crate) struct RefreshRequest {
    upstream: String,
    repository: String,
    tag: String,
}

/// REQ-0017: 有効期間の残りにかかわらず上流レジストリへ問い合わせ、
/// 保存内容を更新する。REQ-0045: 同一イメージへの連続した指示は、設定
/// された抑止時間（REQ-0046）が経過するまで受け付けない。
pub(crate) async fn post_refresh(
    State(state): State<std::sync::Arc<AppState>>,
    Json(request): Json<RefreshRequest>,
) -> Response {
    let Some(upstream) = state.upstreams().find(&request.upstream) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let key = (
        request.upstream.clone(),
        request.repository.clone(),
        request.tag.clone(),
    );
    if let Err(remaining) = state.refresh_gate().try_acquire(&key) {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(RETRY_AFTER, remaining.as_secs().to_string())],
        )
            .into_response();
    }

    match state
        .manifest_cache()
        .force_refresh(upstream, &request.repository, &request.tag)
        .await
    {
        Ok(response) => {
            // カタログに反映する。保存自体は force_refresh が既に行って
            // いるため、ここはタグ→ダイジェストの記録だけを更新する
            // （REQ-0040 / REQ-0041）。
            let _ = state
                .layout()
                .put_tag(
                    &request.upstream,
                    &request.repository,
                    &request.tag,
                    &response.digest,
                )
                .await;
            axum::Json(serde_json::json!({ "digest": response.digest })).into_response()
        }
        Err(_) => StatusCode::BAD_GATEWAY.into_response(),
    }
}

/// `AppState::new` が `crate::storage::Layout` を組み立てる際に使う既定の
/// 容量（`None` = ファイルシステム全容量の50%、REQ-0026）。
pub(crate) fn new_layout(root: PathBuf) -> Layout {
    Layout::new(root, None).expect("保存領域（storage::Layout）の初期化に失敗した")
}
