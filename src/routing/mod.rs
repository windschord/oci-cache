//! ルーティング解決。
//!
//! 対象要求: REQ-0001 / REQ-0002 / REQ-0003 / REQ-0007 / REQ-0009 / REQ-0056〜REQ-0059。
//! 上流からの実際の取得・中継は次のフェーズ（README の実装順序 2.）で扱うため、
//! ここでは「どの上流を使うか」を決めるところまでを扱う。

mod config;
mod memo;
mod probe;
mod router;

pub use config::{RoutingConfig, Upstream};
pub use memo::{InMemoryRoutingMemo, RoutingMemoStore};
pub use probe::{HttpUpstreamProbe, ProbeOutcome, UpstreamProbe};
pub use router::{Resolution, ResolutionSource, Router, RoutingError};
