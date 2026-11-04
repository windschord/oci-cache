//! 設定ファイル（TOML、figment 経由）の読み込み。
//!
//! キー名と既定値は `config.example.toml` に対応する要求 ID 付きで文書化
//! されている。ここでの実装はその雛形をそのまま反映するだけで、既定値を
//! 変える場合は要求レジストリ側を先に直すこと（CLAUDE.md の要求管理ルール）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use figment::providers::{Env, Format, Toml};
use figment::Figment;
use serde::Deserialize;

use crate::routing::{RoutingConfig, UpstreamCredentials};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("設定ファイル '{path}' を読み込めません")]
    Load {
        path: PathBuf,
        // `figment::Error` は内部に複数の提供元分のエラーを蓄えられる構造
        // で、`ConfigError` 自体のサイズを不必要に膨らませる（clippy
        // `result_large_err`）。この enum は `Result` の戻り値として頻繁に
        // 運ばれるため、ここだけ Box で包んで他の変種のサイズに影響しない
        // ようにする。
        #[source]
        source: Box<figment::Error>,
    },
    #[error(
        "上流レジストリ '{0}' には対応していません（docker.io / ghcr.io / quay.io のいずれかを指定してください、REQ-0008）"
    )]
    UnknownUpstream(String),
    #[error("'{key}' の値 '{value}' を時間として解釈できません（例: \"5s\" / \"30m\" / \"1h\"）")]
    InvalidDuration { key: &'static str, value: String },
    #[error("[server] tls_cert と tls_key は両方設定するか、両方省略してください（REQ-0054）")]
    IncompleteTls,
}

/// 起動に使う、読み込み済みの設定一式。
pub struct Config {
    pub listen: String,
    /// `Some((cert, key))` なら TLS で待ち受ける（REQ-0054）。
    pub tls: Option<(PathBuf, PathBuf)>,
    pub routing: RoutingConfig,
    /// REQ-0018: タグ参照マニフェストの有効期間（既定は REQ-0015 の 30 分）。
    pub tag_manifest_ttl: Duration,
    /// REQ-0046: 強制再取得の抑止時間（既定 60 秒）。
    pub force_refresh_cooldown: Duration,
    /// REQ-0020: OCI Image Layout の保存先。
    pub storage_path: PathBuf,
}

impl Config {
    /// `path` の TOML を読み、環境変数（`OCI_CACHE_<SECTION>__<KEY>`）で
    /// 上書きして構築する。
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let raw: RawConfig = Figment::new()
            .merge(Toml::file(path))
            .merge(Env::prefixed("OCI_CACHE_").split("__"))
            .extract()
            .map_err(|source| ConfigError::Load {
                path: path.to_path_buf(),
                source: Box::new(source),
            })?;

        let tls = match (&raw.server.tls_cert, &raw.server.tls_key) {
            (Some(cert), Some(key)) => Some((cert.clone(), key.clone())),
            (None, None) => None,
            _ => return Err(ConfigError::IncompleteTls),
        };

        Ok(Self {
            listen: raw.server.listen.clone(),
            tls,
            routing: raw.routing_config()?,
            tag_manifest_ttl: parse_duration(
                "cache.tag_manifest_ttl",
                &raw.cache.tag_manifest_ttl,
            )?,
            force_refresh_cooldown: parse_duration(
                "ui.force_refresh_cooldown",
                &raw.ui.force_refresh_cooldown,
            )?,
            storage_path: PathBuf::from(&raw.storage.path),
        })
    }
}

/// `figment` がそのまま deserialize する構造体。TOML の文字列をそのまま
/// 持ち、時間の解釈（`parse_duration`）と上流の解決（`routing_config`）は
/// 別途行う。figment は複数の provider を merge した結果にまとめて
/// deserialize するため、ここで `Duration` 等へ直接変換する
/// `#[serde(deserialize_with = ...)]` を書いても provider 間の上書き
/// （TOML → 環境変数）より前に失敗してしまい、個々の値を環境変数で
/// 上書きする余地がなくなる。
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawConfig {
    server: RawServer,
    routing: RawRouting,
    cache: RawCache,
    storage: RawStorage,
    ui: RawUi,
    /// `[upstreams."<id>".auth]`（REQ-0016）。
    upstreams: HashMap<String, RawUpstreamEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct RawServer {
    listen: String,
    tls_cert: Option<PathBuf>,
    tls_key: Option<PathBuf>,
}

impl Default for RawServer {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:5000".to_string(),
            tls_cert: None,
            tls_key: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct RawRouting {
    upstreams: Vec<String>,
    probe_timeout: String,
    negative_cache_ttl: String,
}

impl Default for RawRouting {
    fn default() -> Self {
        Self {
            // REQ-0008: 初期対応する上流レジストリの既定順序
            upstreams: vec![
                "docker.io".to_string(),
                "ghcr.io".to_string(),
                "quay.io".to_string(),
            ],
            probe_timeout: "5s".to_string(),
            negative_cache_ttl: "30m".to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct RawCache {
    tag_manifest_ttl: String,
}

impl Default for RawCache {
    fn default() -> Self {
        Self {
            tag_manifest_ttl: "30m".to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct RawStorage {
    path: String,
    // REQ-0026 / REQ-0027 の容量上限は、まだ配信経路（BlobCache /
    // ManifestCache）の退避処理へ結線していない（`crate::server`
    // のモジュール doc 参照）。設定しても現時点では効果が無いため、
    // 誤って「効いている」と思わせないようここでは受け付けない。
}

impl Default for RawStorage {
    fn default() -> Self {
        Self {
            path: "./data".to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct RawUi {
    force_refresh_cooldown: String,
}

impl Default for RawUi {
    fn default() -> Self {
        Self {
            force_refresh_cooldown: "60s".to_string(),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct RawUpstreamEntry {
    auth: Option<RawUpstreamAuth>,
}

#[derive(Debug, Deserialize)]
struct RawUpstreamAuth {
    username: String,
    password: String,
}

impl RawConfig {
    /// REQ-0009: 設定順序を上流への問い合わせ順序とする。REQ-0008 により
    /// 対応する上流は固定の3つなので、既定の `RoutingConfig` から
    /// `base_url` / `token_host` を引き、順序と認証情報だけを設定で
    /// 上書きする。
    fn routing_config(&self) -> Result<RoutingConfig, ConfigError> {
        let known = RoutingConfig::default();
        let mut upstreams = Vec::with_capacity(self.routing.upstreams.len());
        for id in &self.routing.upstreams {
            let mut upstream = known
                .find(id)
                .cloned()
                .ok_or_else(|| ConfigError::UnknownUpstream(id.clone()))?;
            if let Some(auth) = self.upstreams.get(id).and_then(|entry| entry.auth.as_ref()) {
                upstream = upstream.with_credentials(UpstreamCredentials::new(
                    auth.username.clone(),
                    auth.password.clone(),
                ));
            }
            upstreams.push(upstream);
        }
        Ok(RoutingConfig {
            upstreams,
            probe_timeout: parse_duration("routing.probe_timeout", &self.routing.probe_timeout)?,
            negative_cache_ttl: parse_duration(
                "routing.negative_cache_ttl",
                &self.routing.negative_cache_ttl,
            )?,
        })
    }
}

/// `"5s"` / `"30m"` / `"1h"` のような単位付きの時間表現を解釈する。単位を
/// 省略した場合は秒として扱う。`humantime` 等の追加の crate は依存に無い
/// （技術スタックの表に無い依存を増やさない方針）ため、設定ファイルの雛形
/// が使う範囲の単位だけをここで素朴に扱う。
fn parse_duration(key: &'static str, value: &str) -> Result<Duration, ConfigError> {
    let invalid = || ConfigError::InvalidDuration {
        key,
        value: value.to_string(),
    };
    let value = value.trim();
    let (digits, unit_secs) = if let Some(prefix) = value.strip_suffix('h') {
        (prefix, 3_600)
    } else if let Some(prefix) = value.strip_suffix('m') {
        (prefix, 60)
    } else if let Some(prefix) = value.strip_suffix('s') {
        (prefix, 1)
    } else {
        (value, 1)
    };
    let amount: u64 = digits.parse().map_err(|_| invalid())?;
    let secs = amount.checked_mul(unit_secs).ok_or_else(invalid)?;
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_seconds_minutes_and_hours() {
        assert_eq!(parse_duration("k", "5s").unwrap(), Duration::from_secs(5));
        assert_eq!(
            parse_duration("k", "30m").unwrap(),
            Duration::from_secs(30 * 60)
        );
        assert_eq!(
            parse_duration("k", "1h").unwrap(),
            Duration::from_secs(3_600)
        );
    }

    #[test]
    fn treats_bare_number_as_seconds() {
        assert_eq!(parse_duration("k", "42").unwrap(), Duration::from_secs(42));
    }

    #[test]
    fn rejects_unparseable_duration() {
        assert!(parse_duration("k", "soon").is_err());
    }

    #[test]
    fn routing_config_resolves_known_upstreams_in_configured_order() {
        let raw = RawConfig {
            routing: RawRouting {
                upstreams: vec!["quay.io".to_string(), "docker.io".to_string()],
                ..RawRouting::default()
            },
            ..RawConfig::default()
        };
        let config = raw.routing_config().unwrap();
        let ids: Vec<&str> = config.upstreams.iter().map(|u| u.id.as_str()).collect();
        assert_eq!(ids, vec!["quay.io", "docker.io"]);
    }

    #[test]
    fn routing_config_rejects_unknown_upstream() {
        let raw = RawConfig {
            routing: RawRouting {
                upstreams: vec!["example.com".to_string()],
                ..RawRouting::default()
            },
            ..RawConfig::default()
        };
        assert!(matches!(
            raw.routing_config(),
            Err(ConfigError::UnknownUpstream(id)) if id == "example.com"
        ));
    }

    #[test]
    fn routing_config_applies_configured_credentials() {
        let mut upstreams = HashMap::new();
        upstreams.insert(
            "docker.io".to_string(),
            RawUpstreamEntry {
                auth: Some(RawUpstreamAuth {
                    username: "alice".to_string(),
                    password: "secret".to_string(),
                }),
            },
        );
        let raw = RawConfig {
            routing: RawRouting {
                upstreams: vec!["docker.io".to_string()],
                ..RawRouting::default()
            },
            upstreams,
            ..RawConfig::default()
        };
        let config = raw.routing_config().unwrap();
        let credentials = config.upstreams[0].credentials.as_ref().unwrap();
        assert_eq!(credentials.username, "alice");
        assert_eq!(credentials.password, "secret");
    }
}
