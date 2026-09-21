use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// リポジトリ参照と、その参照を解決した上流レジストリの対応の記録。
///
/// 永続化（`index.redb` への保存）は保存領域を扱うフェーズ（REQ-0020）で
/// 行う。ここでは `Router` が依存するインターフェースだけを定める。
///
/// `generation` は設定順序（指紋）が変わるたびに進む世代番号。同じメモを
/// 複数の `Router` で共有する場合、設定の再読み込みで新しい `Router` が
/// 作られた後も、まだ探索を続けていた古い `Router` が `put` を呼び出せる
/// （`resolve` の `await` 中に再読み込みが起きうるため）。古い `Router` は
/// 自身が生成された時点の世代を渡すので、実装はそれが現在の世代と一致する
/// 場合だけ書き込みを反映し、古い順序の結果が再読み込み後のメモへ書き戻る
/// のを防ぐ。
pub trait RoutingMemoStore: Send + Sync {
    /// 記録された上流レジストリの識別子を返す。
    fn get(&self, repository: &str) -> Option<String>;
    /// 解決結果を記録する（REQ-0003）。`generation` が現在の世代と一致しない
    /// 場合、書き込みは無視される。
    fn put(&self, repository: &str, upstream_id: &str, generation: u64);
    /// 1件の記録を破棄する（REQ-0056）。
    fn discard(&self, repository: &str);
    /// 設定順序の指紋を有効化する（REQ-0058）。記録済みの指紋と異なれば、
    /// 記録を消去して世代を1つ進める。呼び出し元（`Router::new`）が以降の
    /// `put` に使う世代を返す。
    ///
    /// 指紋の比較・消去・世代の採番を1回の呼び出しに閉じ込めているのは、
    /// これらを別々の呼び出しに分けると、異なる設定順序で `Router::new` が
    /// 並行に呼ばれた際に、両方が同じ世代を採番してしまいうるため
    /// （それぞれの指紋比較が互いの消去より前に行われた場合）。
    fn activate_order(&self, fingerprint: &str) -> u64;

    /// 全上流でリポジトリ参照が不存在だった記録を、`expires_at` を有効期限
    /// として残す（REQ-0004）。`Router` は `RoutingError::NotFoundOnAnyUpstream`
    /// を確定できた場合にのみ呼び出す。
    fn put_negative(&self, repository: &str, expires_at: tokio::time::Instant);
    /// `repository` の不存在記録が `now` の時点でまだ有効期限内であれば
    /// `true` を返す。有効期限を過ぎた記録は破棄する。
    fn is_negative_cached(&self, repository: &str, now: tokio::time::Instant) -> bool;
}

#[derive(Default)]
struct Inner {
    entries: HashMap<String, String>,
    negative_entries: HashMap<String, tokio::time::Instant>,
    order_fingerprint: Option<String>,
    generation: u64,
}

/// `RoutingMemoStore` のインメモリ実装。
#[derive(Default)]
pub struct InMemoryRoutingMemo {
    inner: Mutex<Inner>,
}

impl RoutingMemoStore for InMemoryRoutingMemo {
    fn get(&self, repository: &str) -> Option<String> {
        self.inner.lock().unwrap().entries.get(repository).cloned()
    }

    fn put(&self, repository: &str, upstream_id: &str, generation: u64) {
        let mut inner = self.inner.lock().unwrap();
        if inner.generation != generation {
            // 古い世代からの書き込みは無視する（設定再読み込みのレース対策）
            return;
        }
        inner
            .entries
            .insert(repository.to_string(), upstream_id.to_string());
    }

    fn discard(&self, repository: &str) {
        self.inner.lock().unwrap().entries.remove(repository);
    }

    fn activate_order(&self, fingerprint: &str) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        if inner.order_fingerprint.as_deref() != Some(fingerprint) {
            inner.entries.clear();
            inner.generation += 1;
            inner.order_fingerprint = Some(fingerprint.to_string());
        }
        inner.generation
    }

    fn put_negative(&self, repository: &str, expires_at: tokio::time::Instant) {
        self.inner
            .lock()
            .unwrap()
            .negative_entries
            .insert(repository.to_string(), expires_at);
    }

    fn is_negative_cached(&self, repository: &str, now: tokio::time::Instant) -> bool {
        let mut inner = self.inner.lock().unwrap();
        match inner.negative_entries.get(repository) {
            Some(expires_at) if *expires_at > now => true,
            Some(_) => {
                inner.negative_entries.remove(repository);
                false
            }
            None => false,
        }
    }
}

// 同じメモを複数の `Router` から共有できるようにする（設定再読み込みの表現に使う）。
impl<T: RoutingMemoStore + ?Sized> RoutingMemoStore for Arc<T> {
    fn get(&self, repository: &str) -> Option<String> {
        (**self).get(repository)
    }

    fn put(&self, repository: &str, upstream_id: &str, generation: u64) {
        (**self).put(repository, upstream_id, generation)
    }

    fn discard(&self, repository: &str) {
        (**self).discard(repository)
    }

    fn activate_order(&self, fingerprint: &str) -> u64 {
        (**self).activate_order(fingerprint)
    }

    fn put_negative(&self, repository: &str, expires_at: tokio::time::Instant) {
        (**self).put_negative(repository, expires_at)
    }

    fn is_negative_cached(&self, repository: &str, now: tokio::time::Instant) -> bool {
        (**self).is_negative_cached(repository, now)
    }
}
