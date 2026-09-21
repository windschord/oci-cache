use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use oci_client::client::{Client, ClientConfig, ClientProtocol};
use oci_client::errors::OciDistributionError;
use oci_client::secrets::RegistryAuth;
use oci_client::{Reference, RegistryOperation};
use tokio::io::{AsyncWrite, BufWriter};

use crate::registry_auth::{self, BearerChallenge};
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
    #[error(
        "上流レジストリ '{upstream}' の認証方式の確認で予期しない応答（HTTP {status}）を受け取りました"
    )]
    UnexpectedAuthProbeStatus { upstream: String, status: u16 },
    #[error("上流レジストリ '{upstream}' が要求するトークン発行元 '{realm}' を信頼できません")]
    UntrustedRealm { upstream: String, realm: String },
    #[error(
        "上流レジストリ '{upstream}' のリポジトリ '{repository}' は認証情報なしでは取得できないため、保存も配信もしません"
    )]
    AuthenticationRequired {
        upstream: String,
        repository: String,
    },
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

/// 上流ごとの認証方式（`GET /v2/` の応答から判明する範囲）。
///
/// `realm` / `service` は上流ごとに固定なので使い回してよいが、`scope` は
/// リポジトリに依存するためここには含めない（呼び出しのたびに組み立てる）。
#[derive(Debug, Clone)]
enum AuthShape {
    Anonymous,
    Bearer {
        realm: String,
        service: Option<String>,
    },
}

/// `oci-client` の `pull_blob_stream` による実際の取得。
///
/// 上流ごとに `oci_client::Client` を保持する。`Client` はトークン
/// キャッシュを内部に持つため、上流ごとに使い回さないと匿名取得でも
/// 必要になる docker.io のトークン取得を毎回やり直すことになる。
pub struct OciBlobSource {
    clients: Mutex<HashMap<String, Client>>,
    /// 上流ごとに一度確認した認証方式（`AuthShape`）。`realm` の検証は
    /// 初回確認時に済んでいるため、以降は `GET /v2/` への再問い合わせを
    /// 省略できる。
    auth_shapes: Mutex<HashMap<String, AuthShape>>,
    /// 認証方式の確認（`GET /v2/`）専用のクライアント。設定済みの
    /// `Upstream.base_url` へ問い合わせるとはいえ、応答がリダイレクトを
    /// 返す場合に無条件で追従すると、悪意ある（または乗っ取られた）上流の
    /// 設定1つで任意の宛先へリクエストさせられる（SSRF）ため、
    /// `token_client` と同様にリダイレクトを追わない。
    probe_client: reqwest::Client,
    /// トークン取得専用のクライアント（SSRF 対策）。詳細は
    /// `registry_auth::new_token_client` を参照。
    token_client: reqwest::Client,
}

impl OciBlobSource {
    pub fn new() -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
            auth_shapes: Mutex::new(HashMap::new()),
            probe_client: registry_auth::new_token_client(),
            token_client: registry_auth::new_token_client(),
        }
    }

    /// `manifest` モジュールもマニフェストの取得で同じクライアント・認証
    /// キャッシュを再利用するため `pub(crate)`。
    pub(crate) fn client_and_registry(
        &self,
        upstream: &Upstream,
    ) -> Result<(Client, String), BlobFetchError> {
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

    /// 上流が要求する認証方式（`AuthShape`）を確認する。初回はキャッシュに
    /// 無いので `GET /v2/` で確認し、以降はキャッシュから返す。
    ///
    /// この確認自体（`realm` が信頼できるかの判定）は
    /// `registry_auth::realm_is_trusted` が担う。ここでキャッシュするのは
    /// 「確認した結果」であって、検証をスキップするわけではない。
    async fn auth_shape(&self, upstream: &Upstream) -> Result<AuthShape, BlobFetchError> {
        if let Some(shape) = self.auth_shapes.lock().unwrap().get(&upstream.id).cloned() {
            return Ok(shape);
        }

        let url = format!("{}/v2/", upstream.base_url);
        let response = self.probe_client.get(&url).send().await.map_err(|source| {
            BlobFetchError::AuthProbe {
                upstream: upstream.id.clone(),
                source,
            }
        })?;
        let status = response.status();
        // 成功応答だけを匿名と見なし、401 は有効な Bearer challenge が
        // 付いている場合に限って扱う。5xx・429・リダイレクトやパース不能な
        // 401 まで匿名として記録すると、上流が一時的に不調なだけで
        // 「この上流は匿名で取得できる」という誤った判定を `OciBlobSource`
        // の寿命いっぱいキャッシュしてしまい、以降の取得が401で失敗し
        // 続ける（このキャッシュ自体は再起動まで消えないため）。
        let shape = if status.is_success() {
            AuthShape::Anonymous
        } else if status == reqwest::StatusCode::UNAUTHORIZED {
            let challenge = response
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)
                .and_then(|value| value.to_str().ok())
                .and_then(registry_auth::parse_bearer_challenge);
            match challenge {
                Some(challenge) => AuthShape::Bearer {
                    realm: challenge.realm,
                    service: challenge.service,
                },
                None => {
                    return Err(BlobFetchError::UnexpectedAuthProbeStatus {
                        upstream: upstream.id.clone(),
                        status: status.as_u16(),
                    });
                }
            }
        } else {
            return Err(BlobFetchError::UnexpectedAuthProbeStatus {
                upstream: upstream.id.clone(),
                status: status.as_u16(),
            });
        };
        self.auth_shapes
            .lock()
            .unwrap()
            .insert(upstream.id.clone(), shape.clone());
        Ok(shape)
    }

    /// 上流が要求する認証方式を確認する。
    ///
    /// `oci_client::Client::auth` は `WWW-Authenticate` の `realm` を検証
    /// せずに使う（`RegistryAuth::Anonymous` / `Basic` を渡した場合、
    /// challenge が示す `realm` へ無条件にトークンを取りに行く）ため、
    /// ここで `registry_auth::realm_is_trusted` による検証を済ませてから
    /// `RegistryAuth::Bearer` として渡す。これにより `oci_client` 内部が
    /// 未検証の `realm` へリクエストする経路（SSRF）を経由しない。
    /// `manifest` モジュールもマニフェストの取得で同じ認証解決を再利用する
    /// ため `pub(crate)`。
    ///
    /// REQ-0013 / REQ-0019: 認証情報を使う前に、認証情報なしで同じ範囲の
    /// トークンを取得できるかを必ず確かめる。これに失敗すれば、たとえ
    /// `upstream.credentials`（REQ-0016）で取得できたとしても
    /// `AuthenticationRequired` を返し、それ以上（実際の取得も含めて）
    /// 一切進めない。非公開イメージが、認証情報を持たない下流の利用者へ
    /// 配信される経路になるのを防ぐため（保存の可否だけの判定ではない）。
    pub(crate) async fn resolve_auth(
        &self,
        upstream: &Upstream,
        repository: &str,
    ) -> Result<RegistryAuth, BlobFetchError> {
        match self.auth_shape(upstream).await? {
            AuthShape::Anonymous => Ok(RegistryAuth::Anonymous),
            AuthShape::Bearer { realm, service } => {
                // `scope` はリポジトリごとに異なるため、キャッシュ済みの
                // `realm` / `service` に対して呼び出しのたびに組み立てる
                // (oci_client 自身も challenge の scope は使わず、こう
                // 組み立てる)。
                let challenge = BearerChallenge {
                    realm,
                    service,
                    scope: Some(format!("repository:{repository}:pull")),
                };
                if !registry_auth::realm_is_trusted(upstream, &challenge.realm) {
                    return Err(BlobFetchError::UntrustedRealm {
                        upstream: upstream.id.clone(),
                        realm: challenge.realm.clone(),
                    });
                }

                let anonymous_token = registry_auth::fetch_bearer_token(
                    &self.token_client,
                    upstream,
                    &challenge,
                    None,
                )
                .await
                .ok_or_else(|| BlobFetchError::AuthenticationRequired {
                    upstream: upstream.id.clone(),
                    repository: repository.to_string(),
                })?;

                // REQ-0016: 実際の取得には、設定されていれば運用者の認証
                // 情報を使う（Docker Hub の要求回数制限緩和のため）。認証
                // 情報が無ければ、上で確認した匿名トークンをそのまま使う
                // （scope・realm は同一なので、もう一度問い合わせる必要が
                // ない）。
                let token = match upstream.credentials.as_ref() {
                    Some(credentials) => registry_auth::fetch_bearer_token(
                        &self.token_client,
                        upstream,
                        &challenge,
                        Some(credentials),
                    )
                    .await
                    .ok_or_else(|| BlobFetchError::UntrustedRealm {
                        upstream: upstream.id.clone(),
                        realm: challenge.realm.clone(),
                    })?,
                    None => anonymous_token,
                };
                Ok(RegistryAuth::Bearer(token))
            }
        }
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

        // `oci_client::Client::auth` は `RegistryAuth::Anonymous` を渡しても
        // 内部でもう一度 `GET /v2/` を行う（`Bearer` だけが即座に返る）。
        // 匿名だと判明している上流ではこの呼び出し自体を省略する。トークン
        // を要求されない上流では `auth_store` に何も登録されないため、
        // 実際の blob 取得も無認証のまま正しく進む。
        if !matches!(auth, RegistryAuth::Anonymous) {
            client
                .auth(&reference, &auth, RegistryOperation::Pull)
                .await
                .map_err(|source| BlobFetchError::Auth {
                    upstream: upstream.id.clone(),
                    source,
                })?;
        }

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

/// ディスクへの書き込みをまとめる緩衝サイズ。ネットワークから届くチャンクを
/// 逐一 `tokio::fs::File` へ書くと、書き込みのたびに内部の
/// `spawn_blocking` 呼び出しが発生する。`BufWriter` でまとめることで
/// その回数を減らす。
const WRITE_BUFFER_SIZE: usize = 64 * 1024;

/// `path` を書き込み先として、まだ存在しない一時ファイルを予約して開く。
async fn create_unique_tmp_file(
    path: &Path,
) -> Result<(PathBuf, BufWriter<tokio::fs::File>), BlobFetchError> {
    loop {
        let n = NEXT_TMP_ID.fetch_add(1, Ordering::Relaxed);
        let candidate = path.with_extension(format!("tmp.{}.{n}", std::process::id()));
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
            .await
        {
            Ok(file) => return Ok((candidate, BufWriter::with_capacity(WRITE_BUFFER_SIZE, file))),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(BlobFetchError::Io(err)),
        }
    }
}

/// ダイジェストごとに進行中の取得を1件に絞るための予約。
///
/// `waiters` はこのエントリを掴んでいる（掴もうとしている）呼び出しの数。
/// 0 になったエントリだけを `BlobCache::in_flight` から取り除くことで、
/// 「削除した直後に別の呼び出しが同じ digest で新しいエントリを作ってしまい
/// 取りこぼす」競合を避ける。
#[derive(Default)]
struct InFlightEntry {
    lock: tokio::sync::Mutex<()>,
    waiters: AtomicUsize,
}

/// `acquire_in_flight` が返す予約。`Drop` で必ず1回だけ解放する。
///
/// 呼び出し元が `entry.lock` を待っている間、または取得処理中に
/// キャンセルされる（呼び出し元の Future が途中で破棄される）と、以降の
/// コードは実行されない。解放を関数内の明示的な呼び出しに頼ると、その
/// パスを踏めないまま `waiters` が戻らず、そのダイジェストのエントリが
/// 永遠に片付かなくなる（`in_flight` に残り続ける）。`Drop` はキャンセル
/// されても必ず走るため、ここに解放処理を置く。
struct InFlightReservation<'a, S> {
    cache: &'a BlobCache<S>,
    digest: String,
    entry: Arc<InFlightEntry>,
}

impl<S> Drop for InFlightReservation<'_, S> {
    fn drop(&mut self) {
        self.cache
            .release_in_flight(&self.digest, Arc::clone(&self.entry));
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
    /// 同一ダイジェストへの並行な要求を1回の取得にまとめるための予約表。
    /// 無ければ、未保存の同じ blob へ同時に来た要求がそれぞれ独立に上流へ
    /// 問い合わせてしまう（内容が壊れはしないが、帯域と要求回数制限を
    /// 無駄に消費する）。
    in_flight: Mutex<HashMap<String, Arc<InFlightEntry>>>,
}

impl<S> BlobCache<S> {
    pub fn new(root: impl Into<PathBuf>, source: S) -> Self {
        Self {
            root: root.into(),
            source,
            in_flight: Mutex::new(HashMap::new()),
        }
    }

    async fn exists(&self, path: &Path) -> Result<bool, BlobFetchError> {
        tokio::fs::try_exists(path)
            .await
            .map_err(BlobFetchError::Io)
    }

    /// ダイジェストごとの進行中ロックを予約する。返り値を drop すると
    /// （キャンセルされた場合を含め）必ず1回だけ解放される。
    fn acquire_in_flight(&self, digest: &str) -> InFlightReservation<'_, S> {
        let entry = {
            let mut map = self.in_flight.lock().unwrap();
            let entry = map.entry(digest.to_string()).or_default().clone();
            entry.waiters.fetch_add(1, Ordering::SeqCst);
            entry
        };
        InFlightReservation {
            cache: self,
            digest: digest.to_string(),
            entry,
        }
    }

    fn release_in_flight(&self, digest: &str, entry: Arc<InFlightEntry>) {
        let mut map = self.in_flight.lock().unwrap();
        if entry.waiters.fetch_sub(1, Ordering::SeqCst) == 1 {
            // 自分が最後の待機者だった場合のみ削除する。削除する前に
            // map の中身が自分の掴んでいたエントリと同じかを確かめるのは、
            // このチェックの直前に別の呼び出しが数え終えて新しいエントリを
            // 作り直していた場合、それを消してしまわないようにするため。
            if let Some(current) = map.get(digest) {
                if Arc::ptr_eq(current, &entry) {
                    map.remove(digest);
                }
            }
        }
    }

    /// `manifest` モジュールが、既知のダイジェストから保存済みの内容を
    /// 読み戻すために使うため `pub(crate)`。
    pub(crate) fn blob_path(&self, digest: &str) -> Result<PathBuf, BlobFetchError> {
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

impl<S> BlobCache<S>
where
    S: BlobSource,
{
    /// 保存済みならそのパスを返し（上流へは問い合わせない）、無ければ
    /// 上流から取得して保存してからパスを返す。
    ///
    /// 同じダイジェストへの並行な要求は、最初の1件だけが実際に取得を行い、
    /// 残りはその完了を待ってから保存済みの内容を返す。
    pub async fn get(
        &self,
        upstream: &Upstream,
        repository: &str,
        digest: &str,
    ) -> Result<PathBuf, BlobFetchError> {
        let path = self.blob_path(digest)?;
        if self.exists(&path).await? {
            return Ok(path);
        }

        let reservation = self.acquire_in_flight(digest);
        let guard = reservation.entry.lock.lock().await;

        // ロック待ちの間に別の呼び出しが取得を終えているかもしれないので
        // 再確認する。ここでヒットすれば上流へは一切問い合わせずに済む。
        let result = if self.exists(&path).await? {
            Ok(path)
        } else {
            self.fetch_and_store(upstream, repository, digest, &path)
                .await
        };

        drop(guard);
        result
        // `reservation` はここで drop され、進行中ロックを解放する
        // （途中でキャンセルされた場合も同様）。
    }

    async fn fetch_and_store(
        &self,
        upstream: &Upstream,
        repository: &str,
        digest: &str,
        path: &Path,
    ) -> Result<PathBuf, BlobFetchError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(BlobFetchError::Io)?;
        }

        let (tmp_path, mut writer) = create_unique_tmp_file(path).await?;
        if let Err(err) = self
            .source
            .fetch_into(upstream, repository, digest, &mut writer)
            .await
        {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(err);
        }
        finalize_tmp_file(tmp_path, writer, path).await
    }
}

impl<S> BlobCache<S> {
    /// 上流から別の経路（例: タグ参照でのマニフェスト取得）で内容を取得
    /// 済みの場合に、そのダイジェストを鍵として保存する。既に保存済みなら
    /// そのパスを返すだけで書き込みは行わない。
    ///
    /// `get` と異なり `source` を使わず、呼び出し側が既に持っている内容を
    /// そのまま書き込む。マニフェストのタグ参照はダイジェストを事前に
    /// 知り得ないため、`get`（ダイジェストが既知であることが前提）は使えない。
    pub async fn store(&self, digest: &str, content: &[u8]) -> Result<PathBuf, BlobFetchError> {
        let path = self.blob_path(digest)?;
        if self.exists(&path).await? {
            return Ok(path);
        }

        let reservation = self.acquire_in_flight(digest);
        let guard = reservation.entry.lock.lock().await;

        let result = if self.exists(&path).await? {
            Ok(path)
        } else {
            self.write_and_store(content, &path).await
        };

        drop(guard);
        result
    }

    async fn write_and_store(
        &self,
        content: &[u8],
        path: &Path,
    ) -> Result<PathBuf, BlobFetchError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(BlobFetchError::Io)?;
        }

        let (tmp_path, mut writer) = create_unique_tmp_file(path).await?;
        if let Err(err) = write_all_and_flush(&mut writer, content).await {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(BlobFetchError::Io(err));
        }
        finalize_tmp_file(tmp_path, writer, path).await
    }
}

/// `BufWriter` に書き込んだ内容を明示的に flush する。`finalize_tmp_file`
/// が呼ぶ `sync_all` は内側の `File` に対して直接行われ、`BufWriter` の
/// バッファを経由しないため、flush していない内容は永続化されない。
async fn write_all_and_flush(
    writer: &mut BufWriter<tokio::fs::File>,
    content: &[u8],
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    writer.write_all(content).await?;
    writer.flush().await
}

/// 一時ファイルへの書き込み完了後の後始末（`fetch_and_store` /
/// `write_and_store` で共通）。flush だけでは write の完了が保証される
/// だけで、電源断からの復旧を保証しない。rename の前に一時ファイルを
/// fsync し、rename 後は親ディレクトリも fsync してディレクトリエントリ
/// の更新自体を永続化する。
async fn finalize_tmp_file(
    tmp_path: PathBuf,
    writer: BufWriter<tokio::fs::File>,
    path: &Path,
) -> Result<PathBuf, BlobFetchError> {
    writer
        .get_ref()
        .sync_all()
        .await
        .map_err(BlobFetchError::Io)?;
    drop(writer);
    tokio::fs::rename(&tmp_path, path)
        .await
        .map_err(BlobFetchError::Io)?;
    if let Some(parent) = path.parent() {
        let dir = tokio::fs::File::open(parent)
            .await
            .map_err(BlobFetchError::Io)?;
        dir.sync_all().await.map_err(BlobFetchError::Io)?;
    }
    Ok(path.to_path_buf())
}
