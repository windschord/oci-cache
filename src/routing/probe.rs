use std::future::Future;

use reqwest::Url;
use serde::Deserialize;

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
    /// トークン取得専用のクライアント。リダイレクトを追わない設定にして
    /// いる。`realm` の信頼はホスト名の一致で判定しているが、応答が
    /// リダイレクトを返す場合はその判定をすり抜けてしまうため
    /// （SSRF 対策）。
    token_client: reqwest::Client,
}

impl HttpUpstreamProbe {
    pub fn new(client: reqwest::Client) -> Self {
        let token_client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            // リダイレクトを追う `client` へフォールバックすると、SSRF対策の
            // 前提（トークン取得はリダイレクトを追わない）が崩れる。既定設定
            // からのビルドが失敗するのは環境自体が壊れている場合のみなので、
            // 黙って迂回させず起動時に失敗させる。
            .expect("トークン取得専用の reqwest クライアントの構築に失敗しました");
        Self {
            client,
            token_client,
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
        let Some(token) = self.fetch_bearer_token(upstream, challenge).await else {
            return ProbeOutcome::Unreachable;
        };
        match self.client.get(url).bearer_auth(token).send().await {
            Ok(response) => outcome_from_status(response.status()),
            Err(_) => ProbeOutcome::Unreachable,
        }
    }

    async fn fetch_bearer_token(
        &self,
        upstream: &Upstream,
        challenge: &BearerChallenge,
    ) -> Option<String> {
        if !realm_is_trusted(upstream, &challenge.realm) {
            // 設定した上流以外へリクエストさせられるのを防ぐ（SSRF 対策）
            return None;
        }
        let mut query = Vec::new();
        if let Some(service) = &challenge.service {
            query.push(("service", service.as_str()));
        }
        if let Some(scope) = &challenge.scope {
            query.push(("scope", scope.as_str()));
        }
        let response = self
            .token_client
            .get(&challenge.realm)
            .query(&query)
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body = response.text().await.ok()?;
        let token_response: TokenResponse = serde_json::from_str(&body).ok()?;
        token_response.token.or(token_response.access_token)
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
                .and_then(parse_bearer_challenge);
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

/// `realm` が、この上流について信頼してよいトークン発行元かを判定する。
///
/// スキームは `base_url` と一致させる。`upstream.token_host` が明示されて
/// いればそのホストと一致するかだけを見る（ポートは問わない。運用者が
/// 明示的に指定した発行元なので、標準ポートを使う前提で足りる）。
/// 明示が無ければ `base_url` と同一のホスト・ポートに限定する。ポートまで
/// 見ないと、例えば同一ホスト上の別ポートで動く別サービスへ誘導される
/// 攻撃を防げない。上流が返す `realm` を無条件に信頼すると、悪意ある
/// （または乗っ取られた）上流の設定1つでこのサーバーに任意の宛先へ
/// リクエストさせられてしまう（SSRF）。
fn realm_is_trusted(upstream: &Upstream, realm: &str) -> bool {
    let Ok(base) = Url::parse(&upstream.base_url) else {
        return false;
    };
    let Ok(realm_url) = Url::parse(realm) else {
        return false;
    };
    if realm_url.scheme() != base.scheme() {
        return false;
    }
    match &upstream.token_host {
        Some(token_host) => realm_url.host_str() == Some(token_host.as_str()),
        None => {
            realm_url.host_str().is_some()
                && realm_url.host_str() == base.host_str()
                && realm_url.port_or_known_default() == base.port_or_known_default()
        }
    }
}

fn outcome_from_status(status: reqwest::StatusCode) -> ProbeOutcome {
    match status {
        reqwest::StatusCode::OK => ProbeOutcome::Found,
        reqwest::StatusCode::NOT_FOUND => ProbeOutcome::NotFound,
        _ => ProbeOutcome::Unreachable,
    }
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

struct BearerChallenge {
    realm: String,
    service: Option<String>,
    scope: Option<String>,
}

/// `WWW-Authenticate: Bearer realm="...",service="...",scope="..."` を解析する。
fn parse_bearer_challenge(header_value: &str) -> Option<BearerChallenge> {
    let rest = header_value.strip_prefix("Bearer ")?;
    let mut realm = None;
    let mut service = None;
    let mut scope = None;
    for part in rest.split(',') {
        let (key, value) = part.split_once('=')?;
        let value = value.trim().trim_matches('"');
        match key.trim() {
            "realm" => realm = Some(value.to_string()),
            "service" => service = Some(value.to_string()),
            "scope" => scope = Some(value.to_string()),
            _ => {}
        }
    }
    Some(BearerChallenge {
        realm: realm?,
        service,
        scope,
    })
}
