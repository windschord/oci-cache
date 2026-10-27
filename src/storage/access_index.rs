//! blob ごとの最終参照時刻とサイズを記録する永続索引（REQ-0023 の LRU 退避、
//! README の `index.redb` に相当）。
//!
//! 「最終参照時刻」は壁時計ではなく、`touch` が呼ばれるたびに増える論理的な
//! 連番で表す。壁時計（秒単位）では同じ秒内の複数回の参照が区別できず、
//! 退避順序が不定になるため。

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};

const SIZE_TABLE: TableDefinition<&str, u64> = TableDefinition::new("blob_size");
const ACCESS_TABLE: TableDefinition<&str, u64> = TableDefinition::new("blob_last_access");

pub struct AccessIndex {
    db: Database,
    next_seq: AtomicU64,
}

impl AccessIndex {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, redb::Error> {
        let db = Database::create(path)?;
        let mut max_seq = 0u64;
        let write_txn = db.begin_write()?;
        {
            write_txn.open_table(SIZE_TABLE)?;
            let access = write_txn.open_table(ACCESS_TABLE)?;
            for entry in access.iter()? {
                let (_, value) = entry?;
                max_seq = max_seq.max(value.value());
            }
        }
        write_txn.commit()?;
        Ok(Self {
            db,
            next_seq: AtomicU64::new(max_seq + 1),
        })
    }

    /// `digest` を参照済みとして記録する。既存の記録があれば上書きする。
    pub fn touch(&self, digest: &str, size: u64) -> Result<(), redb::Error> {
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let write_txn = self.db.begin_write()?;
        {
            let mut sizes = write_txn.open_table(SIZE_TABLE)?;
            sizes.insert(digest, size)?;
            let mut access = write_txn.open_table(ACCESS_TABLE)?;
            access.insert(digest, seq)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// `digest` の記録を削除する（退避で blob 本体を消した後に使う）。
    pub fn remove(&self, digest: &str) -> Result<(), redb::Error> {
        let write_txn = self.db.begin_write()?;
        {
            let mut sizes = write_txn.open_table(SIZE_TABLE)?;
            sizes.remove(digest)?;
            let mut access = write_txn.open_table(ACCESS_TABLE)?;
            access.remove(digest)?;
        }
        write_txn.commit()?;
        Ok(())
    }

    /// 記録済みの blob サイズの合計（REQ-0023 の退避要否判定に使う）。
    pub fn total_size(&self) -> Result<u64, redb::Error> {
        let read_txn = self.db.begin_read()?;
        let sizes = read_txn.open_table(SIZE_TABLE)?;
        let mut total = 0u64;
        for entry in sizes.iter()? {
            let (_, value) = entry?;
            total += value.value();
        }
        Ok(total)
    }

    /// 最も古く参照された blob とそのサイズを返す（REQ-0023）。
    pub fn least_recently_used(&self) -> Result<Option<(String, u64)>, redb::Error> {
        let read_txn = self.db.begin_read()?;
        let access = read_txn.open_table(ACCESS_TABLE)?;
        let sizes = read_txn.open_table(SIZE_TABLE)?;

        let mut oldest: Option<(String, u64)> = None; // (digest, seq)
        for entry in access.iter()? {
            let (key, value) = entry?;
            let seq = value.value();
            if oldest
                .as_ref()
                .is_none_or(|(_, oldest_seq)| seq < *oldest_seq)
            {
                oldest = Some((key.value().to_string(), seq));
            }
        }

        match oldest {
            Some((digest, _)) => {
                let size = sizes
                    .get(digest.as_str())?
                    .map(|guard| guard.value())
                    .unwrap_or(0);
                Ok(Some((digest, size)))
            }
            None => Ok(None),
        }
    }
}
