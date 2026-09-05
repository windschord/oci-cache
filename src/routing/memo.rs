use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// リポジトリ参照と、その参照を解決した上流レジストリの対応の記録。
///
/// 永続化（`index.redb` への保存）は保存領域を扱うフェーズ（REQ-0020）で
/// 行う。ここでは `Router` が依存するインターフェースだけを定める。
pub trait RoutingMemoStore: Send + Sync {
    /// 記録された上流レジストリの識別子を返す。
    fn get(&self, repository: &str) -> Option<String>;
    /// 解決結果を記録する（REQ-0003）。
    fn put(&self, repository: &str, upstream_id: &str);
    /// 1件の記録を破棄する（REQ-0056）。
    fn discard(&self, repository: &str);
    /// すべての記録を破棄する（REQ-0058: 設定順序の変更時）。
    fn clear_all(&self);
    /// 直近に記録された上流順序の指紋。順序変更の検出に使う（REQ-0058）。
    fn order_fingerprint(&self) -> Option<String>;
    fn set_order_fingerprint(&self, fingerprint: &str);
}

#[derive(Default)]
struct Inner {
    entries: HashMap<String, String>,
    order_fingerprint: Option<String>,
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

    fn put(&self, repository: &str, upstream_id: &str) {
        self.inner
            .lock()
            .unwrap()
            .entries
            .insert(repository.to_string(), upstream_id.to_string());
    }

    fn discard(&self, repository: &str) {
        self.inner.lock().unwrap().entries.remove(repository);
    }

    fn clear_all(&self) {
        self.inner.lock().unwrap().entries.clear();
    }

    fn order_fingerprint(&self) -> Option<String> {
        self.inner.lock().unwrap().order_fingerprint.clone()
    }

    fn set_order_fingerprint(&self, fingerprint: &str) {
        self.inner.lock().unwrap().order_fingerprint = Some(fingerprint.to_string());
    }
}

// 同じメモを複数の `Router` から共有できるようにする（設定再読み込みの表現に使う）。
impl<T: RoutingMemoStore + ?Sized> RoutingMemoStore for Arc<T> {
    fn get(&self, repository: &str) -> Option<String> {
        (**self).get(repository)
    }

    fn put(&self, repository: &str, upstream_id: &str) {
        (**self).put(repository, upstream_id)
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
}
