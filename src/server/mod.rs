//! クライアント向けの HTTP サーバー。
//!
//! 対象要求: REQ-0006（イメージ参照に上流を識別するパス要素を要求しない）
//! / REQ-0054（TLS の受け付け）。
//!
//! OCI Distribution Specification が定める取得系エンドポイントへの本格的な
//! 準拠（`Docker-Content-Digest` ヘッダ、Accept によるメディアタイプの
//! ネゴシエーション、Range 取得、マニフェストリストの素通し）は仕様準拠
//! フェーズ（REQ-0030 〜 REQ-0036）で扱う。ここでは、上流の決定
//! （`crate::routing`）と取得・キャッシュ（`crate::cache`）を実際の HTTP
//! 経路に載せるところまでを扱う。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use axum::extract::{Path as PathExtractor, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router as AxumRouter;

use crate::cache::{BlobCache, ManifestCache, ManifestReference, OciBlobSource};
use crate::routing::{
    HttpUpstreamProbe, InMemoryRoutingMemo, Router as UpstreamRouter, RoutingConfig, RoutingError,
};

mod tls;

pub use tls::serve_tls_on;

/// マニフェスト応答が本文の内容から計算したダイジェストを示すヘッダ
/// （REQ-0032 の一部）。ヘッダ名自体は仕様準拠フェーズ（REQ-0030〜0036）で
/// 本格的に扱うが、`ManifestResponse::digest` を応答へ反映しないと
/// containerd 等が内容を検証できず取得に失敗しうるため、ここで設定する。
const DOCKER_CONTENT_DIGEST: &str = "Docker-Content-Digest";

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
            match state
                .manifest_cache
                .get(upstream, repository, &manifest_reference)
                .await
            {
                Ok(response) => (
                    StatusCode::OK,
                    [(DOCKER_CONTENT_DIGEST, response.digest)],
                    response.content,
                )
                    .into_response(),
                Err(_) => StatusCode::BAD_GATEWAY.into_response(),
            }
        }
        "blobs" => match state.blob_cache.get(upstream, repository, reference).await {
            Ok(path) => serve_file(&path).await,
            Err(_) => StatusCode::BAD_GATEWAY.into_response(),
        },
        _ => StatusCode::NOT_FOUND.into_response(),
    }
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
async fn serve_file(path: &Path) -> Response {
    match tokio::fs::File::open(path).await {
        Ok(file) => {
            let stream = tokio_util::io::ReaderStream::new(file);
            axum::body::Body::from_stream(stream).into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
