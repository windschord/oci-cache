//! 上流からの取得と中継。
//!
//! 対象要求: REQ-0010（ダイジェストによる blob キャッシュ）/ REQ-0051
//! （blob 全体を主記憶に載せない）。取得元の決定（`crate::routing`）が
//! 終わった後、この段階で実際に blob を取得・保存する。
//!
//! マニフェストの取得・キャッシュ（REQ-0011 〜 REQ-0019）と OCI Image
//! Layout による永続化（REQ-0020 〜 REQ-0022）は、次のフェーズ（README の
//! 実装順序 3.）で扱う。

mod blob;

pub use blob::{BlobCache, BlobFetchError, BlobSource, OciBlobSource};
