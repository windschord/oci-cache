//! クライアント向けの HTTP サーバー。
//!
//! 対象要求: REQ-0006（イメージ参照に上流を識別するパス要素を要求しない）
//! / REQ-0054（TLS の受け付け）/ REQ-0030 〜 REQ-0036（OCI Distribution
//! Specification が定める取得系エンドポイントへの準拠）。
//!
//! 書き込み系エンドポイント（REQ-0031）は axum のルーティングに実装しない
//! ことで満たす。`/v2/` と `/v2/{*rest}` のいずれも `GET` しか登録しない
//! ため、他のメソッドは axum の既定動作により HTTP 405（REQ-0036）になる。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as PathExtractor, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router as AxumRouter;
use metrics_exporter_prometheus::PrometheusHandle;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::cache::{
    BlobCache, BlobFetchError, ManifestCache, ManifestFetchError, ManifestReference, OciBlobSource,
};
use crate::routing::{
    HttpUpstreamProbe, InMemoryRoutingMemo, Router as UpstreamRouter, RoutingConfig, RoutingError,
};
use crate::storage::Layout;
use crate::ui::{CacheStats, RefreshGate};
use oci_client::errors::{OciDistributionError, OciErrorCode};

mod tls;

pub use tls::serve_tls_on;

/// マニフェスト応答が本文の内容から計算したダイジェストを示すヘッダ
/// （REQ-0032）。`ManifestResponse::digest` は `oci-client` が本文から計算
/// した値（`src/cache/manifest.rs` 参照）なので、ここでそのまま反映すれば
/// 本文との一致が保証される。
const DOCKER_CONTENT_DIGEST: &str = "Docker-Content-Digest";

/// マニフェスト応答の `Content-Type`（REQ-0033）。クライアントが Accept
/// ヘッダで範囲を絞った場合、その範囲に収まるメディアタイプで応答する。
const CONTENT_TYPE: &str = "Content-Type";

/// マニフェストに `mediaType` フィールドが無い場合の既定の応答
/// `Content-Type`。OCI Distribution Specification が Accept 未指定時の
/// 既定として定める値と揃える。
const DEFAULT_MANIFEST_MEDIA_TYPE: &str = oci_client::manifest::OCI_IMAGE_MEDIA_TYPE;

/// クライアントからの要求を捌くのに必要な状態一式。
///
/// `layout`（REQ-0020〜0029）はカタログ（REQ-0040 / 0041）のタグ記録と
/// GC のためだけに使う。blob・マニフェストの実際の取得・保存は引き続き
/// `blob_cache` / `manifest_cache` が担う（同じ `root` を指すため、
/// blob の保存先自体は共有される。容量による退避を実際の配信経路に
/// 載せる結線は別途行う）。
pub struct AppState {
    router: UpstreamRouter<HttpUpstreamProbe, Arc<InMemoryRoutingMemo>>,
    upstreams: RoutingConfig,
    blob_cache: BlobCache<OciBlobSource>,
    manifest_cache: ManifestCache,
    layout: Layout,
    stats: CacheStats,
    refresh_gate: RefreshGate,
    prometheus_handle: PrometheusHandle,
    root: PathBuf,
}

impl AppState {
    pub fn new(config: RoutingConfig, root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let probe = HttpUpstreamProbe::new(reqwest::Client::new());
        let memo = Arc::new(InMemoryRoutingMemo::default());
        let router = UpstreamRouter::new(config.clone(), probe, memo);
        let blob_cache = BlobCache::new(root.clone(), OciBlobSource::new());
        let manifest_cache = ManifestCache::new(root.clone(), OciBlobSource::new());
        let layout = crate::ui::new_layout(root.clone());
        Self {
            router,
            upstreams: config,
            blob_cache,
            manifest_cache,
            layout,
            stats: CacheStats::default(),
            refresh_gate: RefreshGate::default(),
            prometheus_handle: crate::ui::prometheus_handle(),
            root,
        }
    }

    /// REQ-0046: 既定値ではなく運用者が設定した抑止時間を使う
    /// （`ManifestCache::with_tag_ttl` と同じ、既定値の上書きという形）。
    pub fn with_refresh_cooldown(mut self, cooldown: std::time::Duration) -> Self {
        self.refresh_gate = RefreshGate::new(cooldown);
        self
    }

    pub(crate) fn upstreams(&self) -> &RoutingConfig {
        &self.upstreams
    }

    pub(crate) fn manifest_cache(&self) -> &ManifestCache {
        &self.manifest_cache
    }

    pub(crate) fn layout(&self) -> &Layout {
        &self.layout
    }

    pub(crate) fn stats(&self) -> &CacheStats {
        &self.stats
    }

    pub(crate) fn refresh_gate(&self) -> &RefreshGate {
        &self.refresh_gate
    }

    pub(crate) fn prometheus_handle(&self) -> &PrometheusHandle {
        &self.prometheus_handle
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }
}

/// アプリケーションを組み立てる。REQ-0006: `/v2/<repository>/...` に上流を
/// 識別する追加のパス要素は無い。`{*rest}` はリポジトリ参照に含まれる `/`
/// をそのまま受け取るために使う（マニフェスト・blob の別と参照は、末尾から
/// 2要素として `rest` の中から取り出す。REQ-0006 は要素を「追加」しない
/// ことを求めるものであり、リポジトリ参照自身が持つ `/` を禁じるものでは
/// ない）。
pub fn build_router(state: AppState) -> AxumRouter {
    AxumRouter::new()
        .route("/v2/", get(handle_base_check))
        .route("/v2/{*rest}", get(handle_v2_request))
        // REQ-0040〜0047 / REQ-0017 / REQ-0044: Web UI と観測。
        // `crate::ui` が実装を持つ（`crate::server` は OCI のエンドポイント
        // に専念させる）。
        .route("/api/catalog", get(crate::ui::get_catalog))
        .route("/api/stats", get(crate::ui::get_stats))
        .route("/api/refresh", post(crate::ui::post_refresh))
        .route("/metrics", get(crate::ui::get_metrics))
        // REQ-0043: 埋め込んだ Web UI の静的資産。上記以外の経路すべてを
        // 受け、`/` は `index.html` に読み替える。
        .fallback(get(crate::ui::serve_asset))
        .with_state(Arc::new(state))
}

async fn handle_base_check() -> StatusCode {
    StatusCode::OK
}

async fn handle_v2_request(
    State(state): State<Arc<AppState>>,
    PathExtractor(rest): PathExtractor<String>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let Some((repository, verb, reference)) = split_v2_path(&rest) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let ns_hint = params.get("ns").map(String::as_str);

    let resolution = match state.router.resolve(repository, ns_hint).await {
        Ok(resolution) => resolution,
        Err(RoutingError::NotFoundOnAnyUpstream) => return StatusCode::NOT_FOUND.into_response(),
        Err(RoutingError::UnknownUpstream(_)) => return StatusCode::BAD_REQUEST.into_response(),
        Err(RoutingError::Unreachable(_)) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    let Some(upstream) = state.upstreams.find(&resolution.upstream) else {
        // ルーティングメモに記録された上流が設定から取り除かれている場合。
        // `Router` が次回の要求では記録を破棄して解決し直すため、この要求
        // に限っては応答できない
        return StatusCode::BAD_GATEWAY.into_response();
    };

    match verb {
        "manifests" => {
            let manifest_reference = if is_digest(reference) {
                ManifestReference::Digest(reference.to_string())
            } else {
                ManifestReference::Tag(reference.to_string())
            };
            // REQ-0033: クライアントの Accept ヘッダが示す範囲に収まる
            // メディアタイプだけを返す。ヘッダが無い、または `*/*` を含む
            // 場合は既定の全範囲を許容する（絞らない）。
            let accept_types = headers
                .get(axum::http::header::ACCEPT)
                .and_then(|value| value.to_str().ok())
                .map(parse_accept_media_types)
                .filter(|types| !types.is_empty() && !types.iter().any(|t| t == "*/*"));
            let accept_refs: Option<Vec<&str>> = accept_types
                .as_ref()
                .map(|types| types.iter().map(String::as_str).collect());

            match state
                .manifest_cache
                .get_filtered(
                    upstream,
                    repository,
                    &manifest_reference,
                    accept_refs.as_deref(),
                )
                .await
            {
                Ok(response) => {
                    // REQ-0042: 命中・不命中を記録する。
                    state.stats.record_manifest(response.hit);
                    // REQ-0040 / REQ-0041: タグ参照が解決できた時に、保持
                    // 内容の一覧（`crate::storage::Layout`）へ記録する。
                    // ダイジェスト参照は、それ自体が何らかのタグの指す
                    // マニフェストを辿った結果として既に記録されている
                    // はずなので、ここでは記録しない。
                    if let ManifestReference::Tag(tag) = &manifest_reference {
                        let _ = state
                            .layout
                            .put_tag(&upstream.id, repository, tag, &response.digest)
                            .await;
                    }
                    let content_type = response
                        .media_type
                        .unwrap_or_else(|| DEFAULT_MANIFEST_MEDIA_TYPE.to_string());
                    (
                        StatusCode::OK,
                        [
                            (DOCKER_CONTENT_DIGEST, response.digest),
                            (CONTENT_TYPE, content_type),
                        ],
                        response.content,
                    )
                        .into_response()
                }
                Err(ManifestFetchError::NotAcceptable) => {
                    StatusCode::NOT_ACCEPTABLE.into_response()
                }
                Err(ManifestFetchError::Blob(err)) => upstream_error_status(&err).into_response(),
            }
        }
        "blobs" => {
            // REQ-0042: 命中・不命中の判定は取得前の存在確認で行う。
            // `BlobCache::get` 自身は命中・不命中を返り値で区別しない。
            let hit = match state.blob_cache.blob_path(reference) {
                Ok(path) => tokio::fs::try_exists(&path).await.unwrap_or(false),
                Err(_) => false,
            };
            match state.blob_cache.get(upstream, repository, reference).await {
                Ok(path) => {
                    state.stats.record_blob(hit);
                    // REQ-0035: クライアントが Range を指定した場合は、
                    // その範囲だけを返す。
                    let range = headers
                        .get(axum::http::header::RANGE)
                        .and_then(|value| value.to_str().ok());
                    serve_file(&path, range).await
                }
                Err(err) => upstream_error_status(&err).into_response(),
            }
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// 上流から取得できなかった時に返す応答を決める。上流自体に該当する
/// マニフェスト・blob・リポジトリが無い場合は 404 を返す。OCI Distribution
/// Specification は存在しない対象に 404 を要求しており、502 はゲートウェイ
/// 障害を示すためクライアントが一時的な障害として再試行するおそれがある
/// （CodeRabbit レビュー指摘）。それ以外の失敗（接続不能・認証拒否等）は
/// 502 のままにする。
fn upstream_error_status(err: &BlobFetchError) -> StatusCode {
    match err {
        BlobFetchError::Pull { source, .. } if is_upstream_not_found(source) => {
            StatusCode::NOT_FOUND
        }
        _ => StatusCode::BAD_GATEWAY,
    }
}

/// `oci-client` は blob とマニフェストで上流の 404 を異なる形に包む。
/// マニフェスト取得（`pull_manifest_raw`）は本文を OCI のエラー封筒として
/// 解釈するため `RegistryError` になるが、blob 取得（`pull_blob_stream`）は
/// `reqwest::Response::error_for_status` を経由するため本文を解釈せず
/// `RequestError` になる。両方を見なければ blob の 404 を取り落とす。
fn is_upstream_not_found(err: &OciDistributionError) -> bool {
    match err {
        OciDistributionError::RequestError(err) => {
            err.status() == Some(reqwest::StatusCode::NOT_FOUND)
        }
        OciDistributionError::RegistryError { envelope, .. } => envelope.errors.iter().any(|e| {
            matches!(
                e.code,
                OciErrorCode::ManifestUnknown
                    | OciErrorCode::BlobUnknown
                    | OciErrorCode::NameUnknown
            )
        }),
        _ => false,
    }
}

/// `Accept` ヘッダの値をメディアタイプの一覧へ分解する（REQ-0033）。
/// `;q=0.9` のような品質値はこの実装では区別せず、タイプの判定にだけ使う。
fn parse_accept_media_types(header_value: &str) -> Vec<String> {
    header_value
        .split(',')
        .map(|part| part.split(';').next().unwrap_or("").trim().to_string())
        .filter(|part| !part.is_empty())
        .collect()
}

/// `<repository>/<manifests|blobs>/<reference>` を末尾から分解する。
/// リポジトリ参照は `/` を含みうる（例: `library/nginx`）ため、先頭からの
/// 分解ではなく末尾2要素を切り出す。
fn split_v2_path(rest: &str) -> Option<(&str, &str, &str)> {
    let (head, reference) = rest.rsplit_once('/')?;
    let (repository, verb) = head.rsplit_once('/')?;
    if repository.is_empty() || verb.is_empty() || reference.is_empty() {
        return None;
    }
    Some((repository, verb, reference))
}

/// マニフェストへの参照がダイジェスト（`sha256:...`）かタグかを判別する。
/// タグは `:` を含めない文法のため、`:` の有無で区別できる。
fn is_digest(reference: &str) -> bool {
    reference.contains(':')
}

/// REQ-0051 の趣旨（blob 全体を主記憶に載せない）は取得元（上流／保存済み
/// キャッシュ）を問わないため、保存済みファイルもストリームとして応答する。
/// `range` は `Range` ヘッダの生の値（REQ-0035）。無ければ全体を返す。
async fn serve_file(path: &Path, range: Option<&str>) -> Response {
    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    let total = match file.metadata().await {
        Ok(metadata) => metadata.len(),
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    match range.and_then(parse_range_header) {
        None => (
            [
                (ACCEPT_RANGES, "bytes".to_string()),
                (CONTENT_LENGTH, total.to_string()),
            ],
            axum::body::Body::from_stream(tokio_util::io::ReaderStream::new(file)),
        )
            .into_response(),
        Some(range) => match range.resolve(total) {
            None => (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(CONTENT_RANGE, format!("bytes */{total}"))],
            )
                .into_response(),
            Some((start, end)) => {
                let mut file = file;
                if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
                    return StatusCode::INTERNAL_SERVER_ERROR.into_response();
                }
                let len = end - start + 1;
                let stream = tokio_util::io::ReaderStream::new(file.take(len));
                (
                    StatusCode::PARTIAL_CONTENT,
                    [
                        (ACCEPT_RANGES, "bytes".to_string()),
                        (CONTENT_RANGE, format!("bytes {start}-{end}/{total}")),
                        (CONTENT_LENGTH, len.to_string()),
                    ],
                    axum::body::Body::from_stream(stream),
                )
                    .into_response()
            }
        },
    }
}

const ACCEPT_RANGES: &str = "Accept-Ranges";
const CONTENT_RANGE: &str = "Content-Range";
const CONTENT_LENGTH: &str = "Content-Length";

/// `Range: bytes=<start>-<end>` 形式のヘッダから範囲を取り出す
/// （REQ-0035）。複数範囲（コンマ区切り）や解釈できない値は非対応として
/// `None` を返し、呼び出し側は全体を返す応答にフォールバックする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ByteRange {
    start: Option<u64>,
    end: Option<u64>,
}

fn parse_range_header(value: &str) -> Option<ByteRange> {
    let spec = value.strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (start_s, end_s) = spec.split_once('-')?;
    let start = match start_s {
        "" => None,
        s => Some(s.parse::<u64>().ok()?),
    };
    let end = match end_s {
        "" => None,
        s => Some(s.parse::<u64>().ok()?),
    };
    if start.is_none() && end.is_none() {
        return None;
    }
    Some(ByteRange { start, end })
}

impl ByteRange {
    /// `total` は資源全体のバイト数。範囲を満たせない場合は `None`
    /// （416 Range Not Satisfiable）。満たせる場合は両端を含む
    /// `(start, end)` を返す。`end` は `-<N>`（末尾から N バイト）の
    /// サフィックス形式も扱うため `start.is_none()` の場合は `end` を
    /// 長さとして解釈する。
    fn resolve(&self, total: u64) -> Option<(u64, u64)> {
        if total == 0 {
            return None;
        }
        match (self.start, self.end) {
            (Some(start), Some(end)) => {
                if start > end || start >= total {
                    None
                } else {
                    Some((start, end.min(total - 1)))
                }
            }
            (Some(start), None) => {
                if start >= total {
                    None
                } else {
                    Some((start, total - 1))
                }
            }
            (None, Some(suffix_len)) => {
                if suffix_len == 0 {
                    None
                } else {
                    let len = suffix_len.min(total);
                    Some((total - len, total - 1))
                }
            }
            (None, None) => None,
        }
    }
}

#[cfg(test)]
mod range_tests {
    use super::*;

    #[test]
    fn parses_bounded_range() {
        assert_eq!(
            parse_range_header("bytes=2-5"),
            Some(ByteRange {
                start: Some(2),
                end: Some(5)
            })
        );
    }

    #[test]
    fn parses_open_ended_range() {
        assert_eq!(
            parse_range_header("bytes=10-"),
            Some(ByteRange {
                start: Some(10),
                end: None
            })
        );
    }

    #[test]
    fn parses_suffix_range() {
        assert_eq!(
            parse_range_header("bytes=-500"),
            Some(ByteRange {
                start: None,
                end: Some(500)
            })
        );
    }

    #[test]
    fn rejects_multi_range_and_malformed_values() {
        assert_eq!(parse_range_header("bytes=0-1,2-3"), None);
        assert_eq!(parse_range_header("bytes=abc-5"), None);
        assert_eq!(parse_range_header("not-bytes=0-5"), None);
    }

    #[test]
    fn resolves_bounded_range_clamped_to_total() {
        // 要求の終端が全体を超える場合は全体の末尾まで切り詰める
        let range = ByteRange {
            start: Some(2),
            end: Some(100),
        };
        assert_eq!(range.resolve(16), Some((2, 15)));
    }

    #[test]
    fn resolves_open_ended_range() {
        let range = ByteRange {
            start: Some(10),
            end: None,
        };
        assert_eq!(range.resolve(16), Some((10, 15)));
    }

    #[test]
    fn resolves_suffix_range_clamped_to_total() {
        // 末尾 500 バイトを要求しても全体が 16 バイトしか無ければ全体を返す
        let range = ByteRange {
            start: None,
            end: Some(500),
        };
        assert_eq!(range.resolve(16), Some((0, 15)));
    }

    #[test]
    fn rejects_range_starting_beyond_total() {
        let range = ByteRange {
            start: Some(16),
            end: Some(20),
        };
        assert_eq!(range.resolve(16), None);
    }

    #[test]
    fn rejects_inverted_range() {
        let range = ByteRange {
            start: Some(5),
            end: Some(2),
        };
        assert_eq!(range.resolve(16), None);
    }
}
