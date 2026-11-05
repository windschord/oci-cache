//! oci-cache — 複数の上流レジストリに対応する、パス透過なプルスルーキャッシュ。
//!
//! 起動は `--config <path>` で指定した TOML 設定ファイルを読む
//! （雛形は `config.example.toml`、読み込みは `oci_cache::config::Config::load`）。
//!
//! 実装の進め方は `CLAUDE.md` と `docs/requirements/generated/traceability.md` を
//! 参照すること。クライアント向け HTTP サーバー（`oci_cache::server`）・
//! ルーティング（`oci_cache::routing`）・取得と保存（`oci_cache::cache`）・
//! Web UI と観測（`oci_cache::ui`）はいずれも実装済みで、ここではそれらを
//! 設定ファイルから組み立てて実際に起動するだけを担う。
//!
//! `oci_cache::server` は `oci_cache::cache`（`BlobCache` / `ManifestCache`）を
//! 直接使っており、blob・マニフェストの取得・保存自体は容量による退避・GC
//! を持つ `oci_cache::storage::Layout` へまだ結線していない（`[storage]
//! capacity` が今は効かない理由。`oci_cache::server::AppState` のモジュール
//! doc を参照）。この結線は別途行う。

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::ExitCode;

use oci_cache::config::Config;
use oci_cache::server::{build_router, serve_tls_on, AppState};

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let config_path = match parse_args(std::env::args().skip(1)) {
        Ok(path) => path,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };

    let config = match Config::load(&config_path) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("oci-cache: 設定の読み込みに失敗しました: {err}");
            // `ConfigError::Load` が保持する `figment::Error`（構文エラーや
            // 型の不一致の詳細）は `source()` を辿らないと出力されない。
            // これを出さないと、設定ファイルのどこが悪いのか運用者に
            // 伝わらない（CodeRabbit レビュー指摘）。
            let mut cause = std::error::Error::source(&err);
            while let Some(source) = cause {
                eprintln!("  原因: {source}");
                cause = source.source();
            }
            return ExitCode::FAILURE;
        }
    };

    match run(config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("oci-cache: {err}");
            ExitCode::FAILURE
        }
    }
}

/// `--config <path>` だけを受け付ける。待ち受け先やログの詳細といった他の
/// 項目に CLI オプションを設けていないのは、それらが設定ファイル
/// （figment 経由）の役割であり、CLI 引数と設定ファイルで同じ内容を
/// 二重に扱わないようにするため。
///
/// 不明な引数や `--config` の重複は拒否する。黙って無視すると、運用者が
/// 指定したつもりの内容（例: 誤って付けた `--listen`）が実際には何の
/// 効果も持たないまま起動してしまう（CodeRabbit レビュー指摘）。
fn parse_args(mut args: impl Iterator<Item = String>) -> Result<PathBuf, String> {
    let mut config_path = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                if config_path.is_some() {
                    return Err("--config は一度だけ指定してください".to_string());
                }
                let path = args
                    .next()
                    .ok_or_else(|| "--config には設定ファイルのパスが必要です".to_string())?;
                config_path = Some(PathBuf::from(path));
            }
            other => {
                return Err(format!(
                    "不明な引数です: {other}\n使い方: oci-cache --config <path>"
                ));
            }
        }
    }
    config_path.ok_or_else(|| "使い方: oci-cache --config <path>".to_string())
}

async fn run(config: Config) -> std::io::Result<()> {
    let state = AppState::new(config.routing, config.storage_path)
        .with_tag_manifest_ttl(config.tag_manifest_ttl)
        .with_refresh_cooldown(config.force_refresh_cooldown);
    let app = build_router(state);

    let listener = TcpListener::bind(&config.listen)?;
    tracing::info!(
        listen = %config.listen,
        tls = config.tls.is_some(),
        "oci-cache: 待ち受けを開始します"
    );

    match config.tls {
        // REQ-0054: 証明書と秘密鍵が設定されていれば TLS で待ち受ける。
        Some((cert, key)) => serve_tls_on(listener, cert, key, app).await,
        None => {
            listener.set_nonblocking(true)?;
            let listener = tokio::net::TcpListener::from_std(listener)?;
            axum::serve(listener, app.into_make_service()).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> impl Iterator<Item = String> {
        values
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn parses_config_path() {
        let path = parse_args(args(&["--config", "config.toml"])).unwrap();
        assert_eq!(path, PathBuf::from("config.toml"));
    }

    #[test]
    fn rejects_missing_path_value() {
        assert!(parse_args(args(&["--config"])).is_err());
    }

    #[test]
    fn rejects_no_arguments() {
        assert!(parse_args(args(&[])).is_err());
    }

    /// CodeRabbit レビュー指摘: 不明な引数（例: タイプミスした
    /// `--listen`）を黙って無視すると、運用者が指定したつもりの内容が
    /// 実際には何の効果も持たないまま起動してしまう。
    #[test]
    fn rejects_unknown_argument() {
        assert!(parse_args(args(&["--listen", "127.0.0.1:5000"])).is_err());
        assert!(parse_args(args(&[
            "--config",
            "config.toml",
            "--listen",
            "127.0.0.1:5000"
        ]))
        .is_err());
    }

    /// CodeRabbit レビュー指摘: `--config` が複数回指定された場合、最初の
    /// 1件だけを見て残りを黙って無視すると、運用者は最後に書いた方が
    /// 効くと誤解する。
    #[test]
    fn rejects_duplicate_config_argument() {
        assert!(parse_args(args(&["--config", "a.toml", "--config", "b.toml"])).is_err());
    }
}
