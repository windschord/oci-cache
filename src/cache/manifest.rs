//! マニフェストの取得と再検証によるキャッシュ。
//!
//! 対象要求: REQ-0011（タグ参照の再検証）/ REQ-0012（ダイジェスト参照は
//! 再検証しない）/ REQ-0014（上流障害時の stale 応答）/ REQ-0015（タグ
//! 参照マニフェストの既定 TTL）/ REQ-0018（TTL の設定上書き）。
//!
//! ダイジェスト参照（REQ-0012）は内容が不変なため、blob と同じダイジェスト
//! 方式の保存（[`BlobCache`]）にそのまま委ねる。マニフェストも OCI Image
//! Layout ではダイジェストで参照される blob の一種であるため（README の
//! 保存レイアウトを参照）。タグ参照（REQ-0011 / REQ-0014 / REQ-0015 /
//! REQ-0018）は上流の指す先が変わりうるため、ここで別に有効期間つきの
//! 再検証状態を持つ。この状態の永続化（`index.redb`）は保存領域を扱う
//! フェーズ（REQ-0020）で行う。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use oci_client::Reference;
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::routing::Upstream;

use super::blob::{BlobCache, BlobFetchError, BlobSource, OciBlobSource};

/// マニフェストの取得で受け入れるメディアタイプ。
///
/// 仕様準拠フェーズ（REQ-0033: Accept ヘッダによるネゴシエーション）までは
/// クライアントの Accept を反映せず、Docker/OCI の単一マニフェストと
/// マニフェストリストの双方を固定で受け入れる。
const ACCEPTED_MEDIA_TYPES: &[&str] = &[
    oci_client::manifest::OCI_IMAGE_INDEX_MEDIA_TYPE,
    oci_client::manifest::OCI_IMAGE_MEDIA_TYPE,
    oci_client::manifest::IMAGE_MANIFEST_LIST_MEDIA_TYPE,
    oci_client::manifest::IMAGE_MANIFEST_MEDIA_TYPE,
];

/// マニフェストへの参照。タグは可変（REQ-0011）、ダイジェストは不変
/// （REQ-0012）という前提の違いが、以降のキャッシュ挙動を分ける。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestReference {
    Tag(String),
    Digest(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestResponse {
    pub content: Vec<u8>,
    pub digest: String,
    /// REQ-0014: 上流への再検証に失敗し、保存済みの内容をそのまま返した
    /// 場合に `true`。
    pub stale: bool,
}

/// マニフェストの取得。blob の取得（[`OciBlobSource`]）と上流ごとの
/// クライアント・認証方式のキャッシュを共有するため、それを包んで使う。
struct OciManifestSource {
    inner: Arc<OciBlobSource>,
}

impl OciManifestSource {
    fn new(inner: Arc<OciBlobSource>) -> Self {
        Self { inner }
    }

    async fn pull(
        &self,
        upstream: &Upstream,
        repository: &str,
        reference: &Reference,
    ) -> Result<(bytes::Bytes, String), BlobFetchError> {
        let auth = self.inner.resolve_auth(upstream, repository).await?;
        let (client, _registry) = self.inner.client_and_registry(upstream)?;
        // `pull_manifest_raw` は `pull_blob_stream` と異なり、認証済みか
        // どうかに関わらず `auth` を渡すだけで済む（内部で必要なら自ら
        // トークンを保存する）。
        client
            .pull_manifest_raw(reference, &auth, ACCEPTED_MEDIA_TYPES)
            .await
            .map_err(|source| BlobFetchError::Pull {
                upstream: upstream.id.clone(),
                source,
            })
    }

    /// タグ参照で取得する（REQ-0011 の再検証で使う）。呼び出し側が結果を
    /// ダイジェストを鍵として保存するため、ダイジェストも合わせて返す。
    async fn fetch_by_tag(
        &self,
        upstream: &Upstream,
        repository: &str,
        tag: &str,
    ) -> Result<(bytes::Bytes, String), BlobFetchError> {
        let (_client, registry) = self.inner.client_and_registry(upstream)?;
        let reference = Reference::with_tag(registry, repository.to_string(), tag.to_string());
        self.pull(upstream, repository, &reference).await
    }

    /// ダイジェスト参照で取得する（REQ-0012、`BlobSource` 経由で
    /// `BlobCache` から呼ばれる）。
    async fn fetch_by_digest(
        &self,
        upstream: &Upstream,
        repository: &str,
        digest: &str,
    ) -> Result<bytes::Bytes, BlobFetchError> {
        let (_client, registry) = self.inner.client_and_registry(upstream)?;
        let reference =
            Reference::with_digest(registry, repository.to_string(), digest.to_string());
        let (content, _returned_digest) = self.pull(upstream, repository, &reference).await?;
        Ok(content)
    }
}

/// ダイジェスト参照のマニフェスト取得を `BlobSource` として `BlobCache` に
/// 橋渡しする。ダイジェスト参照は内容が不変なため（REQ-0012）、`BlobCache`
/// が既に備える「保存済みなら再取得しない」「同時要求を1件にまとめる」
/// 「一時ファイル経由の原子的な書き込み」をそのまま再利用できる。
struct ManifestByDigestSource(Arc<OciManifestSource>);

impl BlobSource for ManifestByDigestSource {
    async fn fetch_into<W>(
        &self,
        upstream: &Upstream,
        repository: &str,
        digest: &str,
        mut writer: W,
    ) -> Result<(), BlobFetchError>
    where
        W: AsyncWrite + Send + Unpin,
    {
        let content = self.0.fetch_by_digest(upstream, repository, digest).await?;
        writer
            .write_all(&content)
            .await
            .map_err(BlobFetchError::Io)?;
        writer.flush().await.map_err(BlobFetchError::Io)?;
        Ok(())
    }
}

/// タグ参照の再検証状態。
#[derive(Debug, Clone)]
struct TagState {
    digest: String,
    fetched_at: tokio::time::Instant,
}

/// マニフェストのキャッシュ。
pub struct ManifestCache {
    digest_store: BlobCache<ManifestByDigestSource>,
    source: Arc<OciManifestSource>,
    /// `(repository, tag)` ごとの再検証状態。有効期限が切れていても保持し
    /// 続ける（REQ-0014 の stale 応答で使うため、破棄しない）。
    tag_state: Mutex<HashMap<(String, String), TagState>>,
    tag_ttl: Duration,
}

impl ManifestCache {
    /// REQ-0015: タグ参照マニフェストの既定の有効期間。
    pub const DEFAULT_TAG_TTL: Duration = Duration::from_secs(30 * 60);

    pub fn new(root: impl Into<PathBuf>, blob_source: OciBlobSource) -> Self {
        Self::with_tag_ttl(root, blob_source, Self::DEFAULT_TAG_TTL)
    }

    /// REQ-0018: 既定値ではなく運用者が設定した有効期間を使う。
    pub fn with_tag_ttl(
        root: impl Into<PathBuf>,
        blob_source: OciBlobSource,
        tag_ttl: Duration,
    ) -> Self {
        let source = Arc::new(OciManifestSource::new(Arc::new(blob_source)));
        let digest_store = BlobCache::new(root, ManifestByDigestSource(Arc::clone(&source)));
        Self {
            digest_store,
            source,
            tag_state: Mutex::new(HashMap::new()),
            tag_ttl,
        }
    }

    pub async fn get(
        &self,
        upstream: &Upstream,
        repository: &str,
        reference: &ManifestReference,
    ) -> Result<ManifestResponse, BlobFetchError> {
        match reference {
            // REQ-0012: ダイジェスト参照は保存済みなら有効期間を確認せずに
            // 応答する（`BlobCache::get` がその判定を担う）。
            ManifestReference::Digest(digest) => {
                let path = self.digest_store.get(upstream, repository, digest).await?;
                let content = tokio::fs::read(&path).await.map_err(BlobFetchError::Io)?;
                Ok(ManifestResponse {
                    content,
                    digest: digest.clone(),
                    stale: false,
                })
            }
            ManifestReference::Tag(tag) => self.get_by_tag(upstream, repository, tag).await,
        }
    }

    async fn get_by_tag(
        &self,
        upstream: &Upstream,
        repository: &str,
        tag: &str,
    ) -> Result<ManifestResponse, BlobFetchError> {
        let key = (repository.to_string(), tag.to_string());
        let now = tokio::time::Instant::now();

        // REQ-0011: 有効期間が残っていれば、保存済みの内容をそのまま返す
        if let Some(state) = self.fresh_tag_state(&key, now) {
            if let Ok(content) = self.read_stored(&state.digest).await {
                return Ok(ManifestResponse {
                    content,
                    digest: state.digest,
                    stale: false,
                });
            }
        }

        match self.source.fetch_by_tag(upstream, repository, tag).await {
            Ok((content, digest)) => {
                self.digest_store.store(&digest, &content).await?;
                self.tag_state.lock().unwrap().insert(
                    key,
                    TagState {
                        digest: digest.clone(),
                        fetched_at: now,
                    },
                );
                Ok(ManifestResponse {
                    content: content.to_vec(),
                    digest,
                    stale: false,
                })
            }
            Err(err) => {
                // REQ-0014: 再検証（上流への問い合わせ）が失敗しても、
                // 保存済みの内容があればそれを stale として返す。有効期限を
                // 過ぎていても、より新しい内容が確認できなかっただけなので
                // 使ってよい。
                let stored = self.tag_state.lock().unwrap().get(&key).cloned();
                if let Some(state) = stored {
                    if let Ok(content) = self.read_stored(&state.digest).await {
                        return Ok(ManifestResponse {
                            content,
                            digest: state.digest,
                            stale: true,
                        });
                    }
                }
                Err(err)
            }
        }
    }

    fn fresh_tag_state(
        &self,
        key: &(String, String),
        now: tokio::time::Instant,
    ) -> Option<TagState> {
        let state = self.tag_state.lock().unwrap().get(key).cloned()?;
        (now.saturating_duration_since(state.fetched_at) < self.tag_ttl).then_some(state)
    }

    async fn read_stored(&self, digest: &str) -> Result<Vec<u8>, BlobFetchError> {
        let path = self.digest_store.blob_path(digest)?;
        tokio::fs::read(&path).await.map_err(BlobFetchError::Io)
    }
}
