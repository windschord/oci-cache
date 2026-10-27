use std::future::Future;

use crate::registry_auth::{self, BearerChallenge};

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
    /// トークン取得専用のクライアント（SSRF 対策）。詳細は
    /// `registry_auth::new_token_client` を参照。
    token_client: reqwest::Client,
}

impl HttpUpstreamProbe {
    pub fn new(client: reqwest::Client) -> Self {
        Self {
            client,
            token_client: registry_auth::new_token_client(),
        }
    }

    /// 401 に Bearer challenge が付いていれば、トークンを取得して1回だけ
    /// 再試行する。docker.io は匿名 pull でもこの手順を要求するため、これが
    /// 無いと既定の最優先上流（docker.io）が実質的に機能しない。
    async fn probe_with_bearer_retry(
        &self,
        upstream: &Upstream,
        url: &str,
        challenge: &BearerChallenge,
    ) -> ProbeOutcome {
        // 存在確認は認証情報を使わない（配信・保存の可否判定は
        // `cache::blob::OciBlobSource::resolve_auth` が別に行う）。
        let Some(token) =
            registry_auth::fetch_bearer_token(&self.token_client, upstream, challenge, None).await
        else {
            return ProbeOutcome::Unreachable;
        };
        match self.client.get(url).bearer_auth(token).send().await {
            Ok(response) => outcome_from_status(response.status()),
            Err(_) => ProbeOutcome::Unreachable,
        }
    }
}

impl UpstreamProbe for HttpUpstreamProbe {
    async fn probe(&self, upstream: &Upstream, repository: &str) -> ProbeOutcome {
        let url = format!("{}/v2/{repository}/tags/list", upstream.base_url);
        let response = match self.client.get(&url).send().await {
            Ok(response) => response,
            Err(_) => return ProbeOutcome::Unreachable,
        };

        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            let challenge = response
                .headers()
                .get(reqwest::header::WWW_AUTHENTICATE)
                .and_then(|value| value.to_str().ok())
                .and_then(registry_auth::parse_bearer_challenge);
            return match challenge {
                Some(challenge) => {
                    self.probe_with_bearer_retry(upstream, &url, &challenge)
                        .await
                }
                None => ProbeOutcome::Unreachable,
            };
        }

        outcome_from_status(response.status())
    }
}

fn outcome_from_status(status: reqwest::StatusCode) -> ProbeOutcome {
    match status {
        reqwest::StatusCode::OK => ProbeOutcome::Found,
        reqwest::StatusCode::NOT_FOUND => ProbeOutcome::NotFound,
        _ => ProbeOutcome::Unreachable,
    }
}
