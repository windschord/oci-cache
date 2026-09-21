use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use futures_util::StreamExt;
use oci_client::client::{Client, ClientConfig, ClientProtocol};
use oci_client::errors::OciDistributionError;
use oci_client::secrets::RegistryAuth;
use oci_client::{Reference, RegistryOperation};
use tokio::io::AsyncWrite;

use crate::routing::Upstream;

#[derive(Debug, thiserror::Error)]
pub enum BlobFetchError {
    #[error("ダイジェスト '{0}' の形式が不正です")]
    InvalidDigest(String),
    #[error("上流レジストリの URL '{0}' を解釈できません")]
    InvalidUpstreamUrl(String),
    #[error("上流レジストリ '{upstream}' への認証に失敗しました")]
    Auth {
        upstream: String,
        #[source]
        source: OciDistributionError,
    },
    #[error("上流レジストリ '{upstream}' からの blob 取得に失敗しました")]
    Pull {
        upstream: String,
        #[source]
        source: OciDistributionError,
    },
    #[error("blob の保存に失敗しました")]
    Io(#[source] std::io::Error),
}

/// 上流レジストリから blob を取得する。
///
/// REQ-0051: blob の全体を主記憶に載せてはならないため、取得した内容は
/// 都度 `writer` へ書き出すストリームとして扱う（呼び出し側で完全に
/// バッファリングしない）。
pub trait BlobSource: Send + Sync {
    fn fetch_into<W>(
        &self,
        upstream: &Upstream,
        repository: &str,
        digest: &str,
        writer: W,
    ) -> impl Future<Output = Result<(), BlobFetchError>> + Send
    where
        W: AsyncWrite + Send + Unpin;
}

/// `oci-client` の `pull_blob_stream` による実際の取得。
///
/// 上流ごとに `oci_client::Client` を保持する。`Client` はトークン
/// キャッシュを内部に持つため、上流ごとに使い回さないと匿名取得でも
/// 必要になる docker.io のトークン取得を毎回やり直すことになる。
pub struct OciBlobSource {
    clients: Mutex<HashMap<String, Client>>,
}

impl OciBlobSource {
    pub fn new() -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
        }
    }

    fn client_and_registry(&self, upstream: &Upstream) -> Result<(Client, String), BlobFetchError> {
        let (registry, protocol) = parse_upstream_url(&upstream.base_url)?;
        let mut clients = self.clients.lock().unwrap();
        let client = match clients.get(&upstream.id) {
            Some(client) => client.clone(),
            None => {
                let client = Client::new(ClientConfig {
                    protocol,
                    ..Default::default()
                });
                clients.insert(upstream.id.clone(), client.clone());
                client
            }
        };
        Ok((client, registry))
    }
}

impl Default for OciBlobSource {
    fn default() -> Self {
        Self::new()
    }
}

impl BlobSource for OciBlobSource {
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
        use tokio::io::AsyncWriteExt;

        let (client, registry) = self.client_and_registry(upstream)?;
        let reference =
            Reference::with_digest(registry, repository.to_string(), digest.to_string());

        client
            .auth(
                &reference,
                &RegistryAuth::Anonymous,
                RegistryOperation::Pull,
            )
            .await
            .map_err(|source| BlobFetchError::Auth {
                upstream: upstream.id.clone(),
                source,
            })?;

        let mut sized_stream =
            client
                .pull_blob_stream(&reference, digest)
                .await
                .map_err(|source| BlobFetchError::Pull {
                    upstream: upstream.id.clone(),
                    source,
                })?;

        while let Some(chunk) = sized_stream.stream.next().await {
            let chunk = chunk.map_err(BlobFetchError::Io)?;
            writer.write_all(&chunk).await.map_err(BlobFetchError::Io)?;
        }
        Ok(())
    }
}

/// `base_url`（例: `https://registry-1.docker.io`）を `oci-client` が扱う
/// レジストリホストとプロトコルに分ける。
fn parse_upstream_url(base_url: &str) -> Result<(String, ClientProtocol), BlobFetchError> {
    if let Some(host) = base_url.strip_prefix("https://") {
        Ok((
            host.trim_end_matches('/').to_string(),
            ClientProtocol::Https,
        ))
    } else if let Some(host) = base_url.strip_prefix("http://") {
        Ok((host.trim_end_matches('/').to_string(), ClientProtocol::Http))
    } else {
        Err(BlobFetchError::InvalidUpstreamUrl(base_url.to_string()))
    }
}

/// ダイジェストを鍵とした blob の保存（REQ-0010）。
///
/// 保存先は OCI Image Layout に沿った `<root>/blobs/<algorithm>/<hex>`
/// （README の保存レイアウトを参照）。上流ごとの分離は行わない。全上流で
/// 共有して重複排除するのが REQ-0021 の前提であるため。
pub struct BlobCache<S> {
    root: PathBuf,
    source: S,
    /// 一時ファイル名の重複を避けるための連番。同一ダイジェストへの並行な
    /// 要求が同じ一時ファイルに書き込んで内容が混ざるのを防ぐ（在庫が無い
    /// 場合の重複取得そのものは、この段階では避けない）。
    tmp_counter: AtomicU64,
}

impl<S> BlobCache<S>
where
    S: BlobSource,
{
    pub fn new(root: impl Into<PathBuf>, source: S) -> Self {
        Self {
            root: root.into(),
            source,
            tmp_counter: AtomicU64::new(0),
        }
    }

    /// 保存済みならそのパスを返し（上流へは問い合わせない）、無ければ
    /// 上流から取得して保存してからパスを返す。
    pub async fn get(
        &self,
        upstream: &Upstream,
        repository: &str,
        digest: &str,
    ) -> Result<PathBuf, BlobFetchError> {
        let path = self.blob_path(digest)?;
        if tokio::fs::try_exists(&path)
            .await
            .map_err(BlobFetchError::Io)?
        {
            return Ok(path);
        }

        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(BlobFetchError::Io)?;
        }

        let tmp_path = self.tmp_path(&path);
        if let Err(err) = self
            .fetch_to_tmp(upstream, repository, digest, &tmp_path)
            .await
        {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(err);
        }
        tokio::fs::rename(&tmp_path, &path)
            .await
            .map_err(BlobFetchError::Io)?;
        Ok(path)
    }

    async fn fetch_to_tmp(
        &self,
        upstream: &Upstream,
        repository: &str,
        digest: &str,
        tmp_path: &Path,
    ) -> Result<(), BlobFetchError> {
        let file = tokio::fs::File::create(tmp_path)
            .await
            .map_err(BlobFetchError::Io)?;
        self.source
            .fetch_into(upstream, repository, digest, file)
            .await
    }

    fn tmp_path(&self, path: &Path) -> PathBuf {
        let n = self.tmp_counter.fetch_add(1, Ordering::Relaxed);
        path.with_extension(format!("tmp.{n}"))
    }

    fn blob_path(&self, digest: &str) -> Result<PathBuf, BlobFetchError> {
        let (algorithm, hex) = digest
            .split_once(':')
            .ok_or_else(|| BlobFetchError::InvalidDigest(digest.to_string()))?;
        let is_valid_algorithm = !algorithm.is_empty()
            && algorithm
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
        let is_valid_hex = !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit());
        if !is_valid_algorithm || !is_valid_hex {
            return Err(BlobFetchError::InvalidDigest(digest.to_string()));
        }
        Ok(self.root.join("blobs").join(algorithm).join(hex))
    }
}
