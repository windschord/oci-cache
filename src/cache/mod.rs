//! 上流からの取得と中継。
//!
//! 対象要求: REQ-0010（ダイジェストによる blob キャッシュ）/ REQ-0051
//! （blob 全体を主記憶に載せない）/ REQ-0011・0012・0014・0015・0018
//! （マニフェストのキャッシュと再検証）/ REQ-0013・0016・0019（上流の
//! 認証情報と、非公開イメージを保存しない判定）。取得元の決定
//! （`crate::routing`）が終わった後、この段階で実際に blob・マニフェスト
//! を取得・保存する。
//!
//! OCI Image Layout による永続化（REQ-0020 〜 REQ-0029）は `crate::storage`
//! として実装済みだが、この `cache` モジュール（`ManifestCache` のタグ
//! 状態、`BlobCache` の保存）はまだそれを経由せず、再起動で失われる
//! 前提のまま・容量による退避も行わない。`crate::server::AppState` を
//! `crate::storage::Layout` で置き換える結線は別途行う。

mod blob;
mod manifest;

pub use blob::{BlobCache, BlobFetchError, BlobSource, OciBlobSource};
pub use manifest::{ManifestCache, ManifestFetchError, ManifestReference, ManifestResponse};
