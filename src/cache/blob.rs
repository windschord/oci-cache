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

use crate::registry_auth;
use crate::routing::Upstream;

#[derive(Debug, thiserror::Error)]
pub enum BlobFetchError {
    #[error("ダイジェスト '{0}' の形式が不正です")]
    InvalidDigest(String),
    #[error("上流レジストリの URL '{0}' を解釈できません")]
    InvalidUpstreamUrl(String),
    #[error("上流レジストリ '{upstream}' の認証方式の確認に失敗しました")]
    AuthProbe {
        upstream: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("上流レジストリ '{upstream}' が要求するトークン発行元 '{realm}' を信頼できません")]
    UntrustedRealm { upstream: String, realm: String },
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
    /// 認証方式の確認（`GET /v2/`）専用のクライアント。
    probe_client: reqwest::Client,
    /// トークン取得専用のクライアント（SSRF 対策）。詳細は
    /// `registry_auth::new_token_client` を参照。
    token_client: reqwest::Client,
}

impl OciBlobSource {
    pub fn new() -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
            probe_client: reqwest::Client::new(),
            token_client: registry_auth::new_token_client(),
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

    /// 上流が要求する認証方式を確認する。
    ///
    /// `oci_client::Client::auth` は `WWW-Authenticate` の `realm` を検証
    /// せずに使う（`RegistryAuth::Anonymous` / `Basic` を渡した場合、
    /// challenge が示す `realm` へ無条件にトークンを取りに行く）ため、
    /// ここで `registry_auth::realm_is_trusted` による検証を済ませてから
    /// `RegistryAuth::Bearer` として渡す。これにより `oci_client` 内部が
    /// 未検証の `realm` へリクエストする経路（SSRF）を経由しない。
    async fn resolve_auth(
        &self,
        upstream: &Upstream,
        repository: &str,
    ) -> Result<RegistryAuth, BlobFetchError> {
        let url = format!("{}/v2/", upstream.base_url);
        let response = self.probe_client.get(&url).send().await.map_err(|source| {
            BlobFetchError::AuthProbe {
                upstream: upstream.id.clone(),
                source,
            }
        })?;
        let challenge = response
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
            .and_then(registry_auth::parse_bearer_challenge);
        let Some(mut challenge) = challenge else {
            return Ok(RegistryAuth::Anonymous);
        };
        // `/v2/` への一般的な問い合わせなので challenge 自体には scope が
        // 含まれない。この blob が属するリポジトリの pull 権限を明示する
        // (oci_client 自身も challenge の scope は使わず、こう組み立てる)
        challenge.scope = Some(format!("repository:{repository}:pull"));
        let token = registry_auth::fetch_bearer_token(&self.token_client, upstream, &challenge)
            .await
            .ok_or_else(|| BlobFetchError::UntrustedRealm {
                upstream: upstream.id.clone(),
                realm: challenge.realm.clone(),
            })?;
        Ok(RegistryAuth::Bearer(token))
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

        let auth = self.resolve_auth(upstream, repository).await?;
        let (client, registry) = self.client_and_registry(upstream)?;
        let reference =
            Reference::with_digest(registry, repository.to_string(), digest.to_string());

        client
            .auth(&reference, &auth, RegistryOperation::Pull)
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
        // tokio::fs::File はバックグラウンドのブロッキングタスクへ書き込みを
        // 委ねるため、flush するまで実際にディスクへ書き終える保証が無い。
        // これを省くと、書き終わっていない一時ファイルを rename してしまう
        // (不完全な内容を保存済みとして扱う) おそれがある。
        writer.flush().await.map_err(BlobFetchError::Io)?;
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

/// 一時ファイル名の重複排除に使う、プロセス内で共有する連番。
///
/// 同じ `root` を指す `BlobCache` が複数存在しても（`BlobCache::new` は
/// これを禁止していない）一時ファイル名が衝突しないよう、インスタンスでは
/// なくプロセスで共有する。それでも理論上の衝突（他プロセスが同じ名前を
/// 使う等）に備え、実際の予約は `create_new` で行い、失敗したら
/// 採番をやり直す。
static NEXT_TMP_ID: AtomicU64 = AtomicU64::new(0);

/// `path` を書き込み先として、まだ存在しない一時ファイルを予約して開く。
async fn create_unique_tmp_file(path: &Path) -> Result<(PathBuf, tokio::fs::File), BlobFetchError> {
    loop {
        let n = NEXT_TMP_ID.fetch_add(1, Ordering::Relaxed);
        let candidate = path.with_extension(format!("tmp.{}.{n}", std::process::id()));
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
            .await
        {
            Ok(file) => return Ok((candidate, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(BlobFetchError::Io(err)),
        }
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
}

impl<S> BlobCache<S>
where
    S: BlobSource,
{
    pub fn new(root: impl Into<PathBuf>, source: S) -> Self {
        Self {
            root: root.into(),
            source,
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

        let (tmp_path, file) = create_unique_tmp_file(&path).await?;
        if let Err(err) = self
            .source
            .fetch_into(upstream, repository, digest, file)
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
