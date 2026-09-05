use std::future::Future;

use super::config::Upstream;

/// 上流レジストリへの存在確認の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    Found,
    NotFound,
    /// 接続失敗や待ち時間超過など、成否を判定できなかった場合。
    Unreachable,
}

/// リポジトリ参照が上流レジストリに存在するかを確認する。
///
/// 待ち時間の管理（REQ-0057 / REQ-0059）は呼び出し側（`Router`）が担うため、
/// 実装はここで独自のタイムアウトを設ける必要はない。
pub trait UpstreamProbe: Send + Sync {
    fn probe(
        &self,
        upstream: &Upstream,
        repository: &str,
    ) -> impl Future<Output = ProbeOutcome> + Send;
}

/// `GET /v2/<repository>/tags/list` を用いた実際の HTTP による存在確認。
///
/// タグの列挙で確認するのは、リポジトリ参照（タグを含まない）の存在確認に
/// 必要な情報がこれで足りるため。
#[derive(Debug, Clone)]
pub struct HttpUpstreamProbe {
    client: reqwest::Client,
}

impl HttpUpstreamProbe {
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl UpstreamProbe for HttpUpstreamProbe {
    async fn probe(&self, upstream: &Upstream, repository: &str) -> ProbeOutcome {
        let url = format!("{}/v2/{repository}/tags/list", upstream.base_url);
        match self.client.get(url).send().await {
            Ok(response) if response.status() == reqwest::StatusCode::OK => ProbeOutcome::Found,
            Ok(response) if response.status() == reqwest::StatusCode::NOT_FOUND => {
                ProbeOutcome::NotFound
            }
            _ => ProbeOutcome::Unreachable,
        }
    }
}
