//! TLS での待ち受け（REQ-0054）。
//!
//! Docker Engine は既定で TLS による接続を要求するため、証明書と秘密鍵が
//! 設定されていれば TLS で待ち受ける。

use std::path::Path;

use axum_server::tls_rustls::RustlsConfig;

/// 既に bind 済みの `TcpListener` に対して TLS で待ち受ける。
///
/// 呼び出し側が listener を用意する形にしているのは、テストで OS に
/// 空きポートを選ばせたうえ（`TcpListener::bind("127.0.0.1:0")`）で、実際に
/// 割り当てられたアドレスを `local_addr()` から取得できるようにするため。
pub async fn serve_tls_on(
    listener: std::net::TcpListener,
    cert_path: impl AsRef<Path>,
    key_path: impl AsRef<Path>,
    app: axum::Router,
) -> std::io::Result<()> {
    // tokio の `TcpListener::from_std` はブロッキングモードの listener を
    // 受け付けない（非同期ランタイムに登録できないため）。呼び出し側に
    // この前提を強いないよう、ここで明示的に切り替える。
    listener.set_nonblocking(true)?;
    let config = RustlsConfig::from_pem_file(cert_path, key_path).await?;
    axum_server::tls_rustls::from_tcp_rustls(listener, config)?
        .serve(app.into_make_service())
        .await
}
