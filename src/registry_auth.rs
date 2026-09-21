//! 上流レジストリとの Bearer トークン認証に共通する処理。
//!
//! `WWW-Authenticate` の `realm` を無条件に信頼すると、設定した上流が
//! （あるいはそれを騙る中間者が）任意のホストへこのサーバーにリクエスト
//! させられる（SSRF）。`routing::probe`（存在確認）と `cache::blob`
//! （blob 本体の取得）の双方がこの検証を必要とするため、ここに集約する。

use reqwest::Url;
use serde::Deserialize;

use crate::routing::Upstream;

pub struct BearerChallenge {
    pub realm: String,
    pub service: Option<String>,
    pub scope: Option<String>,
}

/// `WWW-Authenticate: Bearer realm="...",service="...",scope="..."` を解析する。
pub fn parse_bearer_challenge(header_value: &str) -> Option<BearerChallenge> {
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
pub fn realm_is_trusted(upstream: &Upstream, realm: &str) -> bool {
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

#[derive(Debug, Deserialize)]
struct TokenResponse {
    token: Option<String>,
    access_token: Option<String>,
}

/// `realm` を検証したうえでトークンを取得する。信頼できない `realm` なら
/// 何もリクエストせずに `None` を返す（SSRF 対策）。
pub async fn fetch_bearer_token(
    token_client: &reqwest::Client,
    upstream: &Upstream,
    challenge: &BearerChallenge,
) -> Option<String> {
    if !realm_is_trusted(upstream, &challenge.realm) {
        return None;
    }
    let mut query = Vec::new();
    if let Some(service) = &challenge.service {
        query.push(("service", service.as_str()));
    }
    if let Some(scope) = &challenge.scope {
        query.push(("scope", scope.as_str()));
    }
    let response = token_client
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

/// トークン取得専用の reqwest クライアントを作る。リダイレクトを追わない
/// 設定にしている。`realm` の信頼はホスト名の一致で判定しているが、応答が
/// リダイレクトを返す場合はその判定をすり抜けてしまうため（SSRF 対策）。
pub fn new_token_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        // リダイレクトを追う既定クライアントへフォールバックすると、SSRF
        // 対策の前提（トークン取得はリダイレクトを追わない）が崩れる。既定
        // 設定からのビルドが失敗するのは環境自体が壊れている場合のみなので、
        // 黙って迂回させず起動時に失敗させる。
        .expect("トークン取得専用の reqwest クライアントの構築に失敗しました")
}
