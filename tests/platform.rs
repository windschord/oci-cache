//! 実行基盤に関わる要求のテスト。
//!
//! 対象要求: REQ-0051（`CLAUDE.md` の実装順序 2.）/ REQ-0054（TLS の受け付け）。
//! `tests/platform.rs::large_blob_relay_does_not_buffer_whole_body` は
//! `docs/requirements/generated/traceability.md` の REQ-0051 の検証手段。

use std::net::TcpListener;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use oci_cache::cache::{BlobSource, OciBlobSource};
use oci_cache::routing::{RoutingConfig, Upstream};
use oci_cache::server::{build_router, serve_tls_on, AppState};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWrite;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn digest_of(content: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(content)))
}

/// 書き込み内容とサイズを記録するだけの `AsyncWrite`。
///
/// 全体を主記憶に保持していないことを直接計測する手段が無いため、代わりに
/// 「1回の書き込みが本文全体を1つのチャンクにまとめていないこと」を見る。
/// もし取得コードが `bytes_stream` を使わずに応答全体をバッファしてから
/// 一度に書き込んでいれば、書き込み回数は1、サイズは本文全体と一致するはず
#[derive(Clone, Default)]
struct RecordingWriter {
    state: Arc<Mutex<RecordingState>>,
}

#[derive(Default)]
struct RecordingState {
    buffer: Vec<u8>,
    write_sizes: Vec<usize>,
}

impl RecordingWriter {
    fn buffer(&self) -> Vec<u8> {
        self.state.lock().unwrap().buffer.clone()
    }

    fn write_sizes(&self) -> Vec<usize> {
        self.state.lock().unwrap().write_sizes.clone()
    }
}

impl AsyncWrite for RecordingWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut state = self.state.lock().unwrap();
        state.write_sizes.push(buf.len());
        state.buffer.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// REQ-0051: blob を上流レジストリから取得してクライアントへ中継する時、
/// その blob の全体を主記憶上に保持してはならない。
#[tokio::test]
async fn large_blob_relay_does_not_buffer_whole_body() {
    let server = MockServer::start().await;
    let repository = "library/nginx";
    // TCP/HTTP の実装がチャンク単位で配送せざるを得ない大きさにする
    let content = vec![0xABu8; 16 * 1024 * 1024];
    let digest = digest_of(&content);

    // 認証方式の確認（GET /v2/）に challenge の無い成功応答を返す。未登録
    // だと wiremock の既定応答（404）になり、匿名上流と誤認せず取得を
    // 失敗させる（`OciBlobSource::auth_shape` 参照）。
    Mock::given(method("GET"))
        .and(path("/v2/"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repository}/blobs/{digest}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(content.clone()))
        .mount(&server)
        .await;

    let upstream = Upstream::new("docker.io", server.uri());
    let source = OciBlobSource::new();
    let writer = RecordingWriter::default();

    source
        .fetch_into(&upstream, repository, &digest, writer.clone())
        .await
        .unwrap();

    assert_eq!(writer.buffer(), content);

    let write_sizes = writer.write_sizes();
    assert!(
        write_sizes.len() > 1,
        "書き込みが1回にまとまっている(応答全体を主記憶にバッファしている疑いがある)"
    );
    let max_write = write_sizes.iter().copied().max().unwrap();
    assert!(
        max_write < content.len() / 4,
        "1回の書き込みサイズが本文全体に対して大きすぎる(応答全体を主記憶に保持している疑いがある): {max_write}"
    );
}

/// REQ-0054: 運用者が証明書と秘密鍵を設定した時、システムは TLS による
/// 接続を受け付けなければならない。
#[tokio::test]
async fn tls_listener_accepts_configured_certificate() {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let cert_dir = tempfile::tempdir().unwrap();
    let cert_path = cert_dir.path().join("cert.pem");
    let key_path = cert_dir.path().join("key.pem");
    std::fs::write(&cert_path, cert.pem()).unwrap();
    std::fs::write(&key_path, signing_key.serialize_pem()).unwrap();

    // OS に空きポートを選ばせ、実際に割り当てられたアドレスへ接続する
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let store_dir = tempfile::tempdir().unwrap();
    let state = AppState::new(RoutingConfig::default(), store_dir.path());
    let app = build_router(state);
    let server = tokio::spawn(serve_tls_on(listener, cert_path, key_path, app));

    // 自己署名証明書のため検証は無効化する。ここで確かめたいのは TLS の
    // ハンドシェイク自体が、設定した証明書・秘密鍵で成立することであり、
    // 証明書チェーンの信頼性ではない
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();
    let url = format!("https://127.0.0.1:{}/v2/", addr.port());
    let mut last_err = None;
    let mut response = None;
    for _ in 0..20 {
        if server.is_finished() {
            let result = server.await;
            panic!("TLS サーバーのタスクが早期に終了した: {result:?}");
        }
        match client.get(&url).send().await {
            Ok(r) => {
                response = Some(r);
                break;
            }
            Err(err) => {
                last_err = Some(err);
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        }
    }
    let response = response.unwrap_or_else(|| panic!("接続に失敗し続けた: {last_err:?}"));

    assert_eq!(response.status(), reqwest::StatusCode::OK);

    server.abort();
}
