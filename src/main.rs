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
fn parse_args(mut args: impl Iterator<Item = String>) -> Result<PathBuf, String> {
    while let Some(arg) = args.next() {
        if arg == "--config" {
            return args
                .next()
                .map(PathBuf::from)
                .ok_or_else(|| "--config には設定ファイルのパスが必要です".to_string());
        }
    }
    Err("使い方: oci-cache --config <path>".to_string())
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
