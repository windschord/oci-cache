//! OCI Image Layout による永続化（REQ-0020 〜 REQ-0029）。
//!
//! 保存レイアウトは README のとおり:
//!
//! ```text
//! <root>/
//!   blobs/sha256/<digest>            全上流で共有（重複排除, REQ-0021）
//!   upstreams/<registry>/oci-layout  上流ごとに独立した OCI Image Layout
//!   upstreams/<registry>/index.json  タグ空間は上流ごと（REQ-0022）
//!   upstreams/<registry>/blobs       共有ストアへのシンボリックリンク
//!   index.redb                      最終参照時刻・使用量（REQ-0023）
//! ```
//!
//! `upstreams/<registry>/` 配下に `oci-layout` と、共有 `blobs/` への
//! シンボリックリンクを揃えることで、重複排除（REQ-0021）を保ったまま
//! 上流ごとのディレクトリ単体を skopeo/crane から検査できる有効な
//! OCI Image Layout にできる（REQ-0020 / REQ-0022 の両立）。

mod access_index;
mod tag_index;

pub use access_index::AccessIndex;
pub use tag_index::TagIndex;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use tokio::sync::Mutex as AsyncMutex;

use crate::cache::{BlobCache, BlobFetchError, OciBlobSource};
use crate::routing::Upstream;

const IMAGE_LAYOUT_VERSION: &str = "1.0.0";

/// 保存領域全体を束ねる。blob の取得・保存（REQ-0010 / REQ-0051 は
/// `crate::cache` が担う）に加えて、容量に基づく退避（REQ-0023）、
/// 上流ごとのタグ管理（REQ-0022）、参照されない blob の回収（REQ-0025）を
/// 行う。
pub struct Layout {
    root: PathBuf,
    blobs: BlobCache<OciBlobSource>,
    access: AccessIndex,
    capacity_bytes: u64,
    /// タグ索引の読み書きを直列化する。読み・変更・書き込みの間に別の
    /// 書き込みが割り込むと、どちらかの更新が失われる（lost update）ため。
    tag_write_lock: AsyncMutex<()>,
}

impl Layout {
    /// `capacity_bytes` が `None` なら、保存先ファイルシステムの全容量の
    /// 50% を既定値として使う（REQ-0026）。`Some` を渡せばそれを使う
    /// （REQ-0027）。
    pub fn new(root: impl Into<PathBuf>, capacity_bytes: Option<u64>) -> std::io::Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(root.join("blobs").join("sha256"))?;
        let capacity_bytes = match capacity_bytes {
            Some(bytes) => bytes,
            None => default_capacity_bytes(&root)?,
        };
        let access = AccessIndex::open(root.join("index.redb")).map_err(redb_to_io)?;
        Ok(Self {
            blobs: BlobCache::new(root.clone(), OciBlobSource::new()),
            root,
            access,
            capacity_bytes,
            tag_write_lock: AsyncMutex::new(()),
        })
    }

    pub fn capacity_bytes(&self) -> u64 {
        self.capacity_bytes
    }

    /// 保存済みならそのパスを返し、無ければ上流から取得して保存する
    /// （REQ-0010 は `BlobCache` が担う）。取得・既存の双方で最終参照時刻を
    /// 更新し、必要なら退避する（REQ-0023 / REQ-0028: 退避済みでも
    /// `BlobCache::get` が上流から再取得するため、ここでの特別な処理は
    /// 不要）。
    pub async fn get_blob(
        &self,
        upstream: &Upstream,
        repository: &str,
        digest: &str,
    ) -> Result<PathBuf, BlobFetchError> {
        let path = self.blobs.get(upstream, repository, digest).await?;
        self.touch_and_evict(digest, &path).await?;
        Ok(path)
    }

    /// 既に取得済みの内容（例: マニフェスト）をダイジェストを鍵として保存する。
    pub async fn store_blob(
        &self,
        digest: &str,
        content: &[u8],
    ) -> Result<PathBuf, BlobFetchError> {
        let path = self.blobs.store(digest, content).await?;
        self.touch_and_evict(digest, &path).await?;
        Ok(path)
    }

    async fn touch_and_evict(&self, digest: &str, path: &Path) -> Result<(), BlobFetchError> {
        let size = tokio::fs::metadata(path)
            .await
            .map_err(BlobFetchError::Io)?
            .len();
        self.access
            .touch(digest, size)
            .map_err(redb_to_blob_error)?;
        self.evict_if_needed(digest).await
    }

    /// REQ-0023: 保存済みデータの合計サイズが上限に達した時、最終参照時刻が
    /// 古い blob から順に削除する。
    ///
    /// `protect` は直前に `touch` したばかりの digest（＝呼び出し元がこの
    /// 直後にパスを返す blob）。`touch` は呼ぶたびに論理連番を進めるため
    /// 通常は最も新しい（＝最も退避されにくい）扱いになるが、1個の blob
    /// 単体で `capacity_bytes` を超える場合は、他に削れる blob が無くなった
    /// 末尾で `protect` 自身が最古の候補として選ばれうる。それを削除すると
    /// 呼び出し元が返そうとしているパスの実体が消え、直後の読み出しが
    /// 失敗する（CodeRabbit レビュー指摘）。容量を満たせない場合は諦めて
    /// 超過を許容する。
    async fn evict_if_needed(&self, protect: &str) -> Result<(), BlobFetchError> {
        loop {
            let total = self.access.total_size().map_err(redb_to_blob_error)?;
            if total <= self.capacity_bytes {
                return Ok(());
            }
            let Some((digest, _size)) = self
                .access
                .least_recently_used()
                .map_err(redb_to_blob_error)?
            else {
                // 記録が無ければこれ以上削れない（整合性が崩れているだけ
                // なので、無限ループにせずここで諦める）
                return Ok(());
            };
            if digest == protect {
                return Ok(());
            }
            self.remove_blob(&digest).await?;
        }
    }

    async fn remove_blob(&self, digest: &str) -> Result<(), BlobFetchError> {
        let path = self.blobs.blob_path(digest)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(BlobFetchError::Io(err)),
        }
        self.access.remove(digest).map_err(redb_to_blob_error)
    }

    /// `upstream_id` のタグ情報ディレクトリを用意し、このディレクトリ単体が
    /// 有効な OCI Image Layout になるようにする（REQ-0020 / REQ-0022）。
    fn ensure_upstream_layout(&self, upstream_id: &str) -> std::io::Result<PathBuf> {
        let dir = self.root.join("upstreams").join(upstream_id);
        std::fs::create_dir_all(&dir)?;

        let layout_marker = dir.join("oci-layout");
        if !layout_marker.exists() {
            std::fs::write(
                &layout_marker,
                format!(r#"{{"imageLayoutVersion":"{IMAGE_LAYOUT_VERSION}"}}"#),
            )?;
        }

        let blobs_link = dir.join("blobs");
        if !blobs_link.exists() {
            // 相対シンボリックリンクで共有の blobs/ を参照する。実体を
            // 複製すると REQ-0021 の重複排除が崩れるため
            std::os::unix::fs::symlink("../../blobs", &blobs_link)?;
        }

        Ok(dir)
    }

    fn tag_index(&self, upstream_id: &str) -> Result<TagIndex, BlobFetchError> {
        let dir = self
            .ensure_upstream_layout(upstream_id)
            .map_err(BlobFetchError::Io)?;
        Ok(TagIndex::new(dir))
    }

    pub async fn get_tag(
        &self,
        upstream_id: &str,
        repository: &str,
        tag: &str,
    ) -> Result<Option<String>, BlobFetchError> {
        self.tag_index(upstream_id)?
            .get(repository, tag)
            .await
            .map_err(BlobFetchError::Io)
    }

    /// タグを記録する。既存の記録を別のダイジェストへ置き換えた場合は
    /// 参照されなくなった blob を回収する（REQ-0025）。
    pub async fn put_tag(
        &self,
        upstream_id: &str,
        repository: &str,
        tag: &str,
        digest: &str,
    ) -> Result<(), BlobFetchError> {
        let _guard = self.tag_write_lock.lock().await;
        let index = self.tag_index(upstream_id)?;
        let previous = index
            .put(repository, tag, digest)
            .await
            .map_err(BlobFetchError::Io)?;
        if previous.is_some() {
            self.collect_garbage().await?;
        }
        Ok(())
    }

    pub async fn remove_tag(
        &self,
        upstream_id: &str,
        repository: &str,
        tag: &str,
    ) -> Result<(), BlobFetchError> {
        let _guard = self.tag_write_lock.lock().await;
        let index = self.tag_index(upstream_id)?;
        let removed = index
            .remove(repository, tag)
            .await
            .map_err(BlobFetchError::Io)?;
        if removed {
            self.collect_garbage().await?;
        }
        Ok(())
    }

    /// REQ-0025: どのタグ（どの上流も含む）からも辿れなくなった blob を
    /// 削除する。戻り値は削除したダイジェストの一覧。
    pub async fn collect_garbage(&self) -> Result<Vec<String>, BlobFetchError> {
        let live = self.compute_live_set().await?;
        let mut removed = Vec::new();

        let blobs_dir = self.root.join("blobs").join("sha256");
        let mut entries = match tokio::fs::read_dir(&blobs_dir).await {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(removed),
            Err(err) => return Err(BlobFetchError::Io(err)),
        };
        while let Some(entry) = entries.next_entry().await.map_err(BlobFetchError::Io)? {
            let hex = entry.file_name().to_string_lossy().into_owned();
            let digest = format!("sha256:{hex}");
            if !live.contains(&digest) {
                self.remove_blob(&digest).await?;
                removed.push(digest);
            }
        }
        Ok(removed)
    }

    /// `upstreams/` 配下に実在する上流 ID の一覧（ディレクトリ名）。
    /// `compute_live_set` と `catalog` の双方が、記録済みのタグを
    /// 上流ごとに読むために使う。
    async fn list_upstream_ids(&self) -> Result<Vec<String>, BlobFetchError> {
        let upstreams_dir = self.root.join("upstreams");
        let mut upstream_ids = Vec::new();
        match tokio::fs::read_dir(&upstreams_dir).await {
            Ok(mut entries) => {
                while let Some(entry) = entries.next_entry().await.map_err(BlobFetchError::Io)? {
                    if entry
                        .file_type()
                        .await
                        .map_err(BlobFetchError::Io)?
                        .is_dir()
                    {
                        upstream_ids.push(entry.file_name().to_string_lossy().into_owned());
                    }
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(BlobFetchError::Io(err)),
        }
        Ok(upstream_ids)
    }

    /// 保存済みのリポジトリ参照とタグの一覧（REQ-0040）。各項目は取得元の
    /// 上流（REQ-0041）を伴う。
    pub async fn catalog(&self) -> Result<Vec<CatalogEntry>, BlobFetchError> {
        let mut entries = Vec::new();
        for upstream_id in self.list_upstream_ids().await? {
            let tags = self
                .tag_index(&upstream_id)?
                .list()
                .await
                .map_err(BlobFetchError::Io)?;
            for (repository, tag, digest) in tags {
                entries.push(CatalogEntry {
                    upstream_id: upstream_id.clone(),
                    repository,
                    tag,
                    digest,
                });
            }
        }
        Ok(entries)
    }

    /// 現在どの上流のどのタグからも（直接・間接に）辿れるダイジェストの集合。
    ///
    /// タグが直接指すダイジェストだけでなく、そこから `referenced_digests`
    /// で新たに見つかったダイジェストも作業キューで再帰的に辿る。マニフェ
    /// ストリスト（Image Index）がタグから指される場合、その子マニフェスト
    /// 自体は1階層目で live に入るが、子マニフェストが参照する config・
    /// layers は2階層目であり、1階層分しか辿らないと誤って回収してしまう
    /// （CodeRabbit レビュー指摘）。
    async fn compute_live_set(&self) -> Result<HashSet<String>, BlobFetchError> {
        let mut live = HashSet::new();
        let mut queue = Vec::new();

        for upstream_id in self.list_upstream_ids().await? {
            let digests = self
                .tag_index(&upstream_id)?
                .all_digests()
                .await
                .map_err(BlobFetchError::Io)?;
            for manifest_digest in digests {
                if live.insert(manifest_digest.clone()) {
                    queue.push(manifest_digest);
                }
            }
        }

        while let Some(digest) = queue.pop() {
            for referenced in self.referenced_digests(&digest).await {
                if live.insert(referenced.clone()) {
                    queue.push(referenced);
                }
            }
        }
        Ok(live)
    }

    /// マニフェストの内容（config・layers・manifests）が参照するダイジェスト。
    /// 読めない・解釈できない場合は空（そのマニフェスト自身は呼び出し元が
    /// 既に live 集合へ加えている）。
    async fn referenced_digests(&self, manifest_digest: &str) -> Vec<String> {
        let Ok(path) = self.blobs.blob_path(manifest_digest) else {
            return Vec::new();
        };
        let Ok(bytes) = tokio::fs::read(&path).await else {
            return Vec::new();
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
            return Vec::new();
        };

        let mut digests = Vec::new();
        if let Some(digest) = value
            .get("config")
            .and_then(|c| c.get("digest"))
            .and_then(|d| d.as_str())
        {
            digests.push(digest.to_string());
        }
        for key in ["layers", "manifests"] {
            if let Some(entries) = value.get(key).and_then(|v| v.as_array()) {
                for entry in entries {
                    if let Some(digest) = entry.get("digest").and_then(|d| d.as_str()) {
                        digests.push(digest.to_string());
                    }
                }
            }
        }
        digests
    }
}

/// 保存済みのリポジトリ参照・タグ1件（REQ-0040 / REQ-0041）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    pub upstream_id: String,
    pub repository: String,
    pub tag: String,
    pub digest: String,
}

/// 保存先ファイルシステムの全容量（バイト）。
pub fn filesystem_capacity(path: &Path) -> std::io::Result<u64> {
    let stat = rustix::fs::statvfs(path).map_err(std::io::Error::from)?;
    Ok(stat.f_blocks * stat.f_frsize)
}

/// 共有 blob ストア（`<root>/blobs/sha256/`）が実際に使っている容量
/// （バイト）。REQ-0042 の保存領域の使用量表示に使う。`AccessIndex`
/// の合計（`touch` された blob だけを数える）とは異なり、ディスク上の
/// 実体をそのまま数える。`crate::cache` 経由の取得（`Layout` を経由
/// しない）で保存された blob も正しく数えるため。
pub async fn blobs_disk_usage(root: &Path) -> std::io::Result<u64> {
    let blobs_dir = root.join("blobs").join("sha256");
    let mut entries = match tokio::fs::read_dir(&blobs_dir).await {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(err) => return Err(err),
    };
    let mut total = 0u64;
    while let Some(entry) = entries.next_entry().await? {
        total += entry.metadata().await?.len();
    }
    Ok(total)
}

/// REQ-0026: 既定の上限はファイルシステム全容量の50%。
fn default_capacity_bytes(root: &Path) -> std::io::Result<u64> {
    Ok(filesystem_capacity(root)? / 2)
}

fn redb_to_io(err: redb::Error) -> std::io::Error {
    std::io::Error::other(err)
}

fn redb_to_blob_error(err: redb::Error) -> BlobFetchError {
    BlobFetchError::Io(redb_to_io(err))
}
