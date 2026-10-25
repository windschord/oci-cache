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

use oci_client::errors::{OciDistributionError, OciErrorCode};
use oci_client::secrets::RegistryAuth;
use oci_client::{Reference, RegistryOperation};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::routing::Upstream;

use super::blob::{BlobCache, BlobFetchError, BlobSource, OciBlobSource};

/// マニフェストの取得で受け入れるメディアタイプの既定値。
///
/// クライアントが Accept ヘッダで範囲を絞らなかった場合（REQ-0033）は、
/// Docker/OCI の単一マニフェストとマニフェストリストの双方を受け入れる。
/// `server` モジュールがクライアントの Accept ヘッダと照合するため
/// `pub(crate)`。
pub(crate) const ACCEPTED_MEDIA_TYPES: &[&str] = &[
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
    /// 本文の `mediaType` フィールド（REQ-0033 の Content-Type 応答・
    /// Accept との適合判定に使う）。本文が解釈できない、または
    /// `mediaType` を含まない場合は `None`。
    pub media_type: Option<String>,
}

/// マニフェストの取得に伴う失敗。[`BlobFetchError`] に加えて、クライアント
/// が指定した Accept の範囲に収まるメディアタイプを提供できない場合
/// （REQ-0033）を区別する。
#[derive(Debug, thiserror::Error)]
pub enum ManifestFetchError {
    #[error(transparent)]
    Blob(#[from] BlobFetchError),
    #[error("クライアントが指定した Accept ヘッダに合致するメディアタイプのマニフェストを提供できません")]
    NotAcceptable,
}

/// 本文から `mediaType` フィールドを取り出す。解釈できない、または
/// フィールドが無ければ `None`（REQ-0033: 無ければ Accept で制限しない）。
fn parse_media_type(content: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(content)
        .ok()?
        .get("mediaType")?
        .as_str()
        .map(str::to_string)
}

/// `media_type` が `accept`（クライアントが指定した Accept の範囲）に
/// 収まっているかを判定する。`accept` が `None`（Accept 指定なし、または
/// `*/*`）なら常に適合する。`media_type` が不明なら保守的に適合させる
/// （どの範囲にも収まらないと断定する根拠が無いため）。
fn is_acceptable(media_type: Option<&str>, accept: Option<&[&str]>) -> bool {
    match (media_type, accept) {
        (_, None) => true,
        (None, Some(_)) => true,
        (Some(media_type), Some(accept)) => accept.contains(&media_type),
    }
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
        accepted_media_types: &[&str],
    ) -> Result<(bytes::Bytes, String), BlobFetchError> {
        let auth = self.inner.resolve_auth(upstream, repository).await?;
        let (client, _registry) = self.inner.client_and_registry(upstream)?;

        // `oci_client::Client` はトークンを `(registry, repository, operation)`
        // で分けてキャッシュするが、`RegistryAuth::Bearer` を渡した場合の
        // 内部の再交渉ロジックは、リポジトリを見ずに渡された値をそのまま
        // 返す（`_auth` 参照）。`store_auth_if_needed` が保持する認証情報は
        // レジストリ単位のため、ここで明示的に `client.auth` を呼んで
        // このリポジトリのキーへ確実に登録しないと、同じ `Client`
        // （＝同じ上流）で複数のリポジトリを扱った際、後発のリポジトリが
        // 別のリポジトリ用のトークンを誤って使い回す（blob 取得
        // （`OciBlobSource::fetch_into`）と同じ理由で必要）。
        if !matches!(auth, RegistryAuth::Anonymous) {
            client
                .auth(reference, &auth, RegistryOperation::Pull)
                .await
                .map_err(|source| BlobFetchError::Auth {
                    upstream: upstream.id.clone(),
                    source,
                })?;
        }

        client
            .pull_manifest_raw(reference, &auth, accepted_media_types)
            .await
            .map_err(|source| BlobFetchError::Pull {
                upstream: upstream.id.clone(),
                source,
            })
    }

    /// タグ参照で取得する（REQ-0011 の再検証で使う）。呼び出し側が結果を
    /// ダイジェストを鍵として保存するため、ダイジェストも合わせて返す。
    /// `accepted_media_types` はクライアントの Accept ヘッダと交差させた
    /// 範囲（REQ-0033）。タグの指す先は Accept によって変わりうるため、
    /// 上流への問い合わせ自体をこの範囲に絞る。
    async fn fetch_by_tag(
        &self,
        upstream: &Upstream,
        repository: &str,
        tag: &str,
        accepted_media_types: &[&str],
    ) -> Result<(bytes::Bytes, String), BlobFetchError> {
        let (_client, registry) = self.inner.client_and_registry(upstream)?;
        let reference = Reference::with_tag(registry, repository.to_string(), tag.to_string());
        self.pull(upstream, repository, &reference, accepted_media_types)
            .await
    }

    /// ダイジェスト参照で取得する（REQ-0012、`BlobSource` 経由で
    /// `BlobCache` から呼ばれる）。内容はダイジェストで不変に決まるため、
    /// クライアントの Accept では上流への問い合わせを絞らず、常に既定の
    /// 全範囲（`ACCEPTED_MEDIA_TYPES`）で取得する。Accept との適合判定は
    /// 呼び出し側（[`ManifestCache::get_filtered`]）が取得後の内容に対して
    /// 行う。
    async fn fetch_by_digest(
        &self,
        upstream: &Upstream,
        repository: &str,
        digest: &str,
    ) -> Result<bytes::Bytes, BlobFetchError> {
        let (_client, registry) = self.inner.client_and_registry(upstream)?;
        let reference =
            Reference::with_digest(registry, repository.to_string(), digest.to_string());
        let (content, _returned_digest) = self
            .pull(upstream, repository, &reference, ACCEPTED_MEDIA_TYPES)
            .await?;
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
    /// `(upstream.id, repository, tag)` ごとの再検証状態。有効期限が切れて
    /// いても保持し続ける（REQ-0014 の stale 応答で使うため、破棄しない）。
    /// `upstream.id` を含めるのは、REQ-0007 の名前衝突や `?ns=` による
    /// 明示的な上流指定により、同じ `(repository, tag)` が複数の上流を
    /// 指しうるため（CodeRabbit レビュー指摘: 含めないと、ある上流から
    /// 保存した内容を別の上流への要求にも誤って使い回す）。
    tag_state: Mutex<HashMap<(String, String, String), TagState>>,
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
        match self
            .get_filtered(upstream, repository, reference, None)
            .await
        {
            Ok(response) => Ok(response),
            Err(ManifestFetchError::Blob(err)) => Err(err),
            // `accept: None` は常に適合させる（`is_acceptable` 参照）ため、
            // この枝には到達しない。
            Err(ManifestFetchError::NotAcceptable) => {
                unreachable!("accept が None の取得は NotAcceptable を返さない")
            }
        }
    }

    /// REQ-0033: クライアントが Accept ヘッダで範囲を絞った場合
    /// （`accept: Some(..)`）、その範囲に収まるメディアタイプのマニフェスト
    /// だけを返す。`None` は範囲指定なし（既定の全範囲を許容）を表す。
    pub async fn get_filtered(
        &self,
        upstream: &Upstream,
        repository: &str,
        reference: &ManifestReference,
        accept: Option<&[&str]>,
    ) -> Result<ManifestResponse, ManifestFetchError> {
        match reference {
            // REQ-0012: ダイジェスト参照は保存済みなら有効期間を確認せずに
            // 応答する（`BlobCache::get` がその判定を担う）。内容は不変なので
            // 上流への問い合わせ自体は絞らず、取得後に Accept と照合する。
            ManifestReference::Digest(digest) => {
                let path = self.digest_store.get(upstream, repository, digest).await?;
                let content = tokio::fs::read(&path).await.map_err(BlobFetchError::Io)?;
                let media_type = parse_media_type(&content);
                if !is_acceptable(media_type.as_deref(), accept) {
                    return Err(ManifestFetchError::NotAcceptable);
                }
                Ok(ManifestResponse {
                    content,
                    digest: digest.clone(),
                    stale: false,
                    media_type,
                })
            }
            ManifestReference::Tag(tag) => self.get_by_tag(upstream, repository, tag, accept).await,
        }
    }

    async fn get_by_tag(
        &self,
        upstream: &Upstream,
        repository: &str,
        tag: &str,
        accept: Option<&[&str]>,
    ) -> Result<ManifestResponse, ManifestFetchError> {
        let key = (upstream.id.clone(), repository.to_string(), tag.to_string());
        let now = tokio::time::Instant::now();

        // REQ-0033: 上流への問い合わせはクライアントの Accept と既定の全
        // 範囲の交差に絞る。交差が空なら、そもそも提供できるメディア
        // タイプが無いということなので上流へ問い合わせずに拒否する。
        let fetch_types: Vec<&str> = match accept {
            Some(requested) => {
                let intersected: Vec<&str> = ACCEPTED_MEDIA_TYPES
                    .iter()
                    .copied()
                    .filter(|supported| requested.contains(supported))
                    .collect();
                if intersected.is_empty() {
                    return Err(ManifestFetchError::NotAcceptable);
                }
                intersected
            }
            None => ACCEPTED_MEDIA_TYPES.to_vec(),
        };

        // REQ-0011: 有効期間が残っていれば、保存済みの内容をそのまま返す。
        // ただし保存済みの内容が今回の Accept に適合しない場合は、新鮮で
        // あっても使えないため上流へ取り直す（下へ続く）。
        if let Some(state) = self.fresh_tag_state(&key, now) {
            if let Ok(content) = self.read_stored(&state.digest).await {
                let media_type = parse_media_type(&content);
                if is_acceptable(media_type.as_deref(), accept) {
                    return Ok(ManifestResponse {
                        content,
                        digest: state.digest,
                        stale: false,
                        media_type,
                    });
                }
            }
        }

        match self
            .source
            .fetch_by_tag(upstream, repository, tag, &fetch_types)
            .await
        {
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
                    media_type: parse_media_type(&content),
                    content: content.to_vec(),
                    digest,
                    stale: false,
                })
            }
            // REQ-0013 相当の判断: 保存済みという理由だけで配信を続けると、
            // 取得時点では公開だった内容が非公開化された場合に、認証情報を
            // 持たない利用者へ配信し続けてしまう（CodeRabbit レビュー指摘:
            // Authorization Bypass）。stale 応答は一時的な到達不能エラー
            // （下の `Err(err)` 節）に限り、認証・認可関連の失敗はそのまま
            // 返す。`resolve_auth` 内の判定（`AuthenticationRequired` /
            // `UntrustedRealm`）だけでなく、トークン登録（`client.auth`、
            // `BlobFetchError::Auth`）や実際の取得（`pull_manifest_raw`、
            // `BlobFetchError::Pull`）が 401/403 を返す経路も同じ Authorization
            // Bypass になりうる（CodeRabbit レビュー指摘2回目: 匿名確認は
            // 成功したが、このリポジトリ自体が非公開化された場合に、この
            // 経路を見落としていた）。
            Err(err) if is_authorization_denial(&err) => Err(err.into()),
            Err(err) => {
                // REQ-0014: 再検証（上流への問い合わせ）が失敗しても、
                // 保存済みの内容があればそれを stale として返す。有効期限を
                // 過ぎていても、より新しい内容が確認できなかっただけなので
                // 使ってよい。ただし Accept に適合しない内容は、取得できる
                // 保証が無い以上 stale としても返さない。
                let stored = self.tag_state.lock().unwrap().get(&key).cloned();
                if let Some(state) = stored {
                    if let Ok(content) = self.read_stored(&state.digest).await {
                        let media_type = parse_media_type(&content);
                        if is_acceptable(media_type.as_deref(), accept) {
                            return Ok(ManifestResponse {
                                content,
                                digest: state.digest,
                                stale: true,
                                media_type,
                            });
                        }
                    }
                }
                Err(err.into())
            }
        }
    }

    fn fresh_tag_state(
        &self,
        key: &(String, String, String),
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

/// `err` が認証・認可の拒否（≒非公開化）を表すかを判定する。
///
/// `resolve_auth` 内で検出される `AuthenticationRequired` / `UntrustedRealm`
/// だけでなく、匿名確認（`GET /v2/`）自体は成功したがこのリポジトリ個別に
/// 401/403 が返る経路（`client.auth` の `BlobFetchError::Auth`、
/// `pull_manifest_raw` の `BlobFetchError::Pull`）も同じ Authorization
/// Bypass になりうる（CodeRabbit レビュー指摘）。これらを一時的な到達不能
/// エラーと区別せずに stale フォールバックすると、非公開化された内容を
/// 認証情報を持たない利用者へ配信し続けてしまう。
///
/// `BlobFetchError::Auth` は `client.auth()`（トークンの登録）の失敗を
/// 包むが、その原因は認可拒否に限らない（ネットワーク断・応答の解釈
/// 失敗等も同じ型で返る）ため、`Pull` と同様に `source` の内容で判定する
/// （CodeRabbit レビュー指摘2回目: 一律に認可拒否と断定すると、本来は
/// stale で救えたはずの一時的な失敗まで即時エラーにしてしまう）。
fn is_authorization_denial(err: &BlobFetchError) -> bool {
    match err {
        BlobFetchError::AuthenticationRequired { .. } | BlobFetchError::UntrustedRealm { .. } => {
            true
        }
        BlobFetchError::Auth { source, .. } | BlobFetchError::Pull { source, .. } => {
            is_unauthorized_or_denied(source)
        }
        _ => false,
    }
}

fn is_unauthorized_or_denied(err: &OciDistributionError) -> bool {
    match err {
        // `client.auth()` 内部のトークン要求が非 200 を返した場合
        // （トークン発行元が認可を拒否した場合）。
        OciDistributionError::AuthenticationFailure(_) => true,
        OciDistributionError::UnauthorizedError { .. } => true,
        OciDistributionError::RegistryError { envelope, .. } => envelope
            .errors
            .iter()
            .any(|e| matches!(e.code, OciErrorCode::Unauthorized | OciErrorCode::Denied)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `client.auth()` に `RegistryAuth::Bearer` を渡す現在の呼び方では
    // 常に即座に成功する（oci-client の `_auth` は Bearer を渡された場合
    // ネットワークに触れず値をそのまま返す）ため、`BlobFetchError::Auth`
    // は結合テストでは再現できない。判定ロジックそのものを単体で確認する。

    #[test]
    fn authentication_failure_is_treated_as_authorization_denial() {
        let err = BlobFetchError::Auth {
            upstream: "docker.io".to_string(),
            source: OciDistributionError::AuthenticationFailure("denied".to_string()),
        };
        assert!(is_authorization_denial(&err));
    }

    #[test]
    fn transient_auth_registration_failure_is_not_treated_as_authorization_denial() {
        // CodeRabbit レビュー指摘: `BlobFetchError::Auth` を一律に認可拒否と
        // 断定すると、認可とは無関係な一時的な失敗（ここでは汎用エラーで
        // 代用）まで stale フォールバックの対象外にしてしまう。
        let err = BlobFetchError::Auth {
            upstream: "docker.io".to_string(),
            source: OciDistributionError::GenericError(Some("timeout".to_string())),
        };
        assert!(!is_authorization_denial(&err));
    }
}
