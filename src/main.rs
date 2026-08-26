//! oci-cache — 複数の上流レジストリに対応する、パス透過なプルスルーキャッシュ。
//!
//! 実装は未着手。要求は `docs/requirements/` を、対応関係は
//! `docs/requirements/generated/traceability.md` を参照すること。
//!
//! 実装の順序は次を想定する。
//!
//! 1. ルーティング解決（REQ-0001 / REQ-0002 / REQ-0003 / REQ-0007 / REQ-0009 / REQ-0056 〜 REQ-0059）
//! 2. 上流からの取得と中継（REQ-0010 / REQ-0051）
//! 3. OCI Image Layout による保存（REQ-0020 / REQ-0021 / REQ-0022）
//! 4. 仕様準拠（REQ-0030 〜 REQ-0036）
//! 5. UI と観測（REQ-0040 〜 REQ-0044）

fn main() {
    eprintln!("oci-cache: 未実装です。docs/requirements/ を参照してください。");
    std::process::exit(1);
}
