//! oci-cache — 複数の上流レジストリに対応する、パス透過なプルスルーキャッシュ。
//!
//! 実行ファイルとしての起動はまだ実装していない。要求は `docs/requirements/` を、
//! 対応関係は `docs/requirements/generated/traceability.md` を参照すること。
//!
//! 実装の順序は次を想定する。
//!
//! 1. ルーティング解決（REQ-0001 〜 0003 / 0006 〜 0009 / 0056 〜 0059）— `oci_cache::routing` として実装済み
//! 2. 上流からの取得と中継（REQ-0010 〜 0016 / 0018 / 0019 / 0051）— `oci_cache::cache` として実装済み
//! 3. OCI Image Layout による保存（REQ-0020 / REQ-0021 / REQ-0022）
//! 4. 仕様準拠（REQ-0030 〜 REQ-0036）
//! 5. UI と観測（REQ-0040 〜 REQ-0047）
//!
//! クライアント向け HTTP サーバー（REQ-0054 の TLS を含む）は
//! `oci_cache::server` として実装済みだが、設定ファイルの読み込みが
//! 無いため、この実行ファイルからはまだ起動できない。

fn main() {
    eprintln!("oci-cache: 未実装です。docs/requirements/ を参照してください。");
    std::process::exit(1);
}
