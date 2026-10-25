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
use std::path::Path;
use std::sync::Arc;

use axum::extract::{Path as PathExtractor, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router as AxumRouter;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::cache::{
    BlobCache, ManifestCache, ManifestFetchError, ManifestReference, OciBlobSource,
};
use crate::routing::{
    HttpUpstreamProbe, InMemoryRoutingMemo, Router as UpstreamRouter, RoutingConfig, RoutingError,
};

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
pub struct AppState {
    router: UpstreamRouter<HttpUpstreamProbe, Arc<InMemoryRoutingMemo>>,
    upstreams: RoutingConfig,
    blob_cache: BlobCache<OciBlobSource>,
    manifest_cache: ManifestCache,
}

impl AppState {
    pub fn new(config: RoutingConfig, root: impl Into<std::path::PathBuf>) -> Self {
        let root = root.into();
        let probe = HttpUpstreamProbe::new(reqwest::Client::new());
        let memo = Arc::new(InMemoryRoutingMemo::default());
        let router = UpstreamRouter::new(config.clone(), probe, memo);
        let blob_cache = BlobCache::new(root.clone(), OciBlobSource::new());
        let manifest_cache = ManifestCache::new(root, OciBlobSource::new());
        Self {
            router,
            upstreams: config,
            blob_cache,
            manifest_cache,
        }
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
                Err(ManifestFetchError::Blob(_)) => StatusCode::BAD_GATEWAY.into_response(),
            }
        }
        "blobs" => match state.blob_cache.get(upstream, repository, reference).await {
            // REQ-0035: クライアントが Range を指定した場合は、その範囲だけ
            // を返す。
            Ok(path) => {
                let range = headers
                    .get(axum::http::header::RANGE)
                    .and_then(|value| value.to_str().ok());
                serve_file(&path, range).await
            }
            Err(_) => StatusCode::BAD_GATEWAY.into_response(),
        },
        _ => StatusCode::NOT_FOUND.into_response(),
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
