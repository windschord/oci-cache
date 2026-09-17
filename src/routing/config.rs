use std::time::Duration;

/// 1つの上流レジストリ。
///
/// `id` はルーティングメモや名前空間ヒント（`?ns=`）との照合に使う識別子
/// （例: `docker.io`）で、`base_url` は実際に問い合わせる先
/// （例: `https://registry-1.docker.io`）。両者を分けているのは、
/// containerd が送る名前空間ヒントの値が実際の接続先ホスト名と
/// 一致するとは限らないため。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upstream {
    pub id: String,
    pub base_url: String,
    /// Bearer トークン取得先（`realm`）として信頼するホスト名。
    ///
    /// 未設定なら `base_url` 自身のホストを信頼する。docker.io のように
    /// トークン発行元（`auth.docker.io`）が registry 本体
    /// （`registry-1.docker.io`）と異なるホストの場合に指定する。これが
    /// 無いと、応答に含まれる `realm` を無条件に信頼することになり、
    /// 設定した上流が悪意を持てば任意のホストへリクエストさせられてしまう
    /// （SSRF）。
    pub token_host: Option<String>,
}

impl Upstream {
    pub fn new(id: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            base_url: base_url.into(),
            token_host: None,
        }
    }

    pub fn with_token_host(mut self, token_host: impl Into<String>) -> Self {
        self.token_host = Some(token_host.into());
        self
    }
}

/// ルーティング解決に使う設定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutingConfig {
    /// 上流レジストリの一覧。問い合わせ順序を兼ねる（REQ-0009）。
    pub upstreams: Vec<Upstream>,
    /// 上流1つあたりの問い合わせを打ち切るまでの時間（REQ-0057 / REQ-0059）。
    pub probe_timeout: Duration,
}

impl RoutingConfig {
    /// `id` に一致する上流を探す。
    pub fn find(&self, id: &str) -> Option<&Upstream> {
        self.upstreams.iter().find(|upstream| upstream.id == id)
    }
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            // REQ-0008: 初期対応する上流レジストリ
            upstreams: vec![
                Upstream::new("docker.io", "https://registry-1.docker.io")
                    .with_token_host("auth.docker.io"),
                Upstream::new("ghcr.io", "https://ghcr.io"),
                Upstream::new("quay.io", "https://quay.io"),
            ],
            // REQ-0059: 上流問い合わせの既定待ち時間
            probe_timeout: Duration::from_secs(5),
        }
    }
}
