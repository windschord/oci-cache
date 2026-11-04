//! ルーティング解決。
//!
//! 対象要求: REQ-0001〜0009 / REQ-0056〜REQ-0059。イメージ参照に上流を
//! 識別するパス要素を要求しないこと自体（REQ-0006）は `crate::server` が
//! 担うが、それを成立させる3段の解決方式はここで実装する。
//! 上流からの実際の取得・中継は次のフェーズ（README の実装順序 2.）で扱うため、
//! ここでは「どの上流を使うか」を決めるところまでを扱う。

mod config;
mod memo;
mod probe;
mod router;

pub use config::{RoutingConfig, Upstream, UpstreamCredentials};
pub use memo::{InMemoryRoutingMemo, RoutingMemoStore};
pub use probe::{HttpUpstreamProbe, ProbeOutcome, UpstreamProbe};
pub use router::{Resolution, ResolutionSource, Router, RoutingError};
