use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// リポジトリ参照と、その参照を解決した上流レジストリの対応の記録。
///
/// 永続化（`index.redb` への保存）は保存領域を扱うフェーズ（REQ-0020）で
/// 行う。ここでは `Router` が依存するインターフェースだけを定める。
///
/// `generation` は設定順序が変わるたび（`clear_all` のたび）に進む世代番号。
/// 同じメモを複数の `Router` で共有する場合、設定の再読み込みで新しい
/// `Router` が作られた後も、まだ探索を続けていた古い `Router` が `put` を
/// 呼び出せる（`resolve` の `await` 中に再読み込みが起きうるため）。古い
/// `Router` は自身が生成された時点の世代を渡すので、実装はそれが現在の
/// 世代と一致する場合だけ書き込みを反映し、古い順序の結果が再読み込み後の
/// メモへ書き戻るのを防ぐ。
pub trait RoutingMemoStore: Send + Sync {
    /// 記録された上流レジストリの識別子を返す。
    fn get(&self, repository: &str) -> Option<String>;
    /// 解決結果を記録する（REQ-0003）。`generation` が現在の世代と一致しない
    /// 場合、書き込みは無視される。
    fn put(&self, repository: &str, upstream_id: &str, generation: u64);
    /// 1件の記録を破棄する（REQ-0056）。
    fn discard(&self, repository: &str);
    /// すべての記録を破棄し、世代を1つ進める（REQ-0058: 設定順序の変更時）。
    fn clear_all(&self);
    /// 直近に記録された上流順序の指紋。順序変更の検出に使う（REQ-0058）。
    fn order_fingerprint(&self) -> Option<String>;
    fn set_order_fingerprint(&self, fingerprint: &str);
    /// 現在の世代番号。`Router::new` が `put` に渡す値を得るために使う。
    fn generation(&self) -> u64;
}

#[derive(Default)]
struct Inner {
    entries: HashMap<String, String>,
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

    fn clear_all(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.entries.clear();
        inner.generation += 1;
    }

    fn order_fingerprint(&self) -> Option<String> {
        self.inner.lock().unwrap().order_fingerprint.clone()
    }

    fn set_order_fingerprint(&self, fingerprint: &str) {
        self.inner.lock().unwrap().order_fingerprint = Some(fingerprint.to_string());
    }

    fn generation(&self) -> u64 {
        self.inner.lock().unwrap().generation
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

    fn clear_all(&self) {
        (**self).clear_all()
    }

    fn order_fingerprint(&self) -> Option<String> {
        (**self).order_fingerprint()
    }

    fn set_order_fingerprint(&self, fingerprint: &str) {
        (**self).set_order_fingerprint(fingerprint)
    }

    fn generation(&self) -> u64 {
        (**self).generation()
    }
}
