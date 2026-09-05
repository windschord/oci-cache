use super::config::{RoutingConfig, Upstream};
use super::memo::RoutingMemoStore;
use super::probe::{ProbeOutcome, UpstreamProbe};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionSource {
    /// 名前空間ヒントによる解決（REQ-0001）。
    NsHint,
    /// ルーティングメモによる解決（REQ-0003）。
    Memo,
    /// 順序フォールバックによる解決（REQ-0002 / REQ-0007 / REQ-0009）。
    Fallback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub upstream: String,
    pub source: ResolutionSource,
}

#[derive(Debug, thiserror::Error)]
pub enum RoutingError {
    #[error("名前空間ヒントが示す上流レジストリ '{0}' は設定されていません")]
    UnknownUpstream(String),
    #[error("上流レジストリ '{0}' への問い合わせが待ち時間内に完了しませんでした")]
    Unreachable(String),
    #[error("設定されたすべての上流レジストリでリポジトリ参照が見つかりませんでした")]
    NotFoundOnAnyUpstream,
}

/// リポジトリ参照から上流レジストリを決定する。
///
/// README に記した3段の解決方式（① 名前空間ヒント → ② ルーティングメモ →
/// ③ 順序フォールバック）をそのまま実装する。
pub struct Router<P, M> {
    config: RoutingConfig,
    probe: P,
    memo: M,
}

impl<P, M> Router<P, M>
where
    P: UpstreamProbe,
    M: RoutingMemoStore,
{
    /// 設定順序がメモの記録時から変わっていれば、その場でメモを破棄する
    /// （REQ-0058）。破棄しないと、REQ-0007 が定める「設定順序が最も先で
    /// ある上流を返す」が新しい順序の下では満たされなくなる。
    pub fn new(config: RoutingConfig, probe: P, memo: M) -> Self {
        let fingerprint = order_fingerprint(&config.upstreams);
        if memo.order_fingerprint().as_deref() != Some(fingerprint.as_str()) {
            memo.clear_all();
            memo.set_order_fingerprint(&fingerprint);
        }
        Self {
            config,
            probe,
            memo,
        }
    }

    pub async fn resolve(
        &self,
        repository: &str,
        ns_hint: Option<&str>,
    ) -> Result<Resolution, RoutingError> {
        // ① REQ-0001: 名前空間ヒントがあれば、それが示す上流だけに問い合わせる
        if let Some(hint) = ns_hint {
            return self.resolve_via_ns_hint(repository, hint).await;
        }

        // ② REQ-0003 / REQ-0056: 記録があればその上流へ直接問い合わせる。
        // 不存在が返れば記録を破棄して③へ進む
        if let Some(memoized_id) = self.memo.get(repository) {
            match self.config.find(&memoized_id) {
                Some(upstream) => match self.probe_with_timeout(upstream, repository).await {
                    ProbeOutcome::Found => {
                        return Ok(Resolution {
                            upstream: upstream.id.clone(),
                            source: ResolutionSource::Memo,
                        });
                    }
                    ProbeOutcome::NotFound => {
                        self.memo.discard(repository);
                    }
                    ProbeOutcome::Unreachable => {
                        return Err(RoutingError::Unreachable(upstream.id.clone()));
                    }
                },
                // 記録された上流が現在の設定から取り除かれている
                None => self.memo.discard(repository),
            }
        }

        // ③ REQ-0002 / REQ-0007 / REQ-0009
        self.resolve_via_fallback(repository).await
    }

    async fn resolve_via_ns_hint(
        &self,
        repository: &str,
        hint: &str,
    ) -> Result<Resolution, RoutingError> {
        let upstream = self
            .config
            .find(hint)
            .ok_or_else(|| RoutingError::UnknownUpstream(hint.to_string()))?;
        match self.probe_with_timeout(upstream, repository).await {
            ProbeOutcome::Unreachable => Err(RoutingError::Unreachable(upstream.id.clone())),
            ProbeOutcome::Found | ProbeOutcome::NotFound => Ok(Resolution {
                upstream: upstream.id.clone(),
                source: ResolutionSource::NsHint,
            }),
        }
    }

    /// 設定順序で問い合わせ、最初に成功した上流を採用する。応答しない上流は
    /// REQ-0057 により失敗として扱い、次の上流へ進める。
    async fn resolve_via_fallback(&self, repository: &str) -> Result<Resolution, RoutingError> {
        for upstream in &self.config.upstreams {
            if self.probe_with_timeout(upstream, repository).await == ProbeOutcome::Found {
                self.memo.put(repository, &upstream.id); // REQ-0003
                return Ok(Resolution {
                    upstream: upstream.id.clone(),
                    source: ResolutionSource::Fallback,
                });
            }
        }
        Err(RoutingError::NotFoundOnAnyUpstream)
    }

    async fn probe_with_timeout(&self, upstream: &Upstream, repository: &str) -> ProbeOutcome {
        tokio::time::timeout(
            self.config.probe_timeout,
            self.probe.probe(upstream, repository),
        )
        .await
        // REQ-0057: 待ち時間を超えたら失敗として扱う
        .unwrap_or(ProbeOutcome::Unreachable)
    }
}

fn order_fingerprint(upstreams: &[Upstream]) -> String {
    upstreams
        .iter()
        .map(|upstream| upstream.id.as_str())
        .collect::<Vec<_>>()
        .join("\u{0}")
}
