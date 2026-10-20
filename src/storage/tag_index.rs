//! 上流レジストリごとのタグ情報（README の `upstreams/<registry>/index.json`）。
//!
//! OCI Image Index の形式（`schemaVersion` / `mediaType` / `manifests`）に
//! 従う。タグ名は `org.opencontainers.image.ref.name` アノテーションで表す
//! （OCI Image Layout が定める慣例）。これにより `upstreams/<id>/` 配下に
//! `oci-layout` と `blobs`（共有ストアへのシンボリックリンク）を揃えれば、
//! 上流ごとのディレクトリ単体が skopeo/crane から検査できる有効な
//! OCI Image Layout になる（REQ-0020 / REQ-0022）。

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const REF_NAME_ANNOTATION: &str = "org.opencontainers.image.ref.name";
const MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const INDEX_MEDIA_TYPE: &str = "application/vnd.oci.image.index.v1+json";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManifestDescriptor {
    #[serde(rename = "mediaType")]
    media_type: String,
    digest: String,
    size: u64,
    /// OCI の記述子では `annotations` は任意（REQ-0022 で参照する OCI
    /// Image Layout の慣例上は `org.opencontainers.image.ref.name` を
    /// 期待するが、仕様上は省略できる）。無い記述子も読み込めるよう既定値
    /// を許す（CodeRabbit レビュー指摘）。
    #[serde(default)]
    annotations: HashMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct IndexFile {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    #[serde(rename = "mediaType")]
    media_type: String,
    manifests: Vec<ManifestDescriptor>,
}

impl Default for IndexFile {
    fn default() -> Self {
        Self {
            schema_version: 2,
            media_type: INDEX_MEDIA_TYPE.to_string(),
            manifests: Vec::new(),
        }
    }
}

/// 1つの上流レジストリに対応するタグ情報。
pub struct TagIndex {
    path: PathBuf,
}

impl TagIndex {
    /// `dir` は呼び出し側が事前に用意した `upstreams/<id>/` ディレクトリ。
    pub fn new(dir: PathBuf) -> Self {
        Self {
            path: dir.join("index.json"),
        }
    }

    fn ref_name(repository: &str, tag: &str) -> String {
        format!("{repository}:{tag}")
    }

    pub async fn get(&self, repository: &str, tag: &str) -> std::io::Result<Option<String>> {
        let index = self.read().await?;
        let ref_name = Self::ref_name(repository, tag);
        Ok(index
            .manifests
            .iter()
            .find(|m| m.annotations.get(REF_NAME_ANNOTATION) == Some(&ref_name))
            .map(|m| m.digest.clone()))
    }

    /// `digest` を記録する。既存の記録を置き換えた場合は、その旧ダイジェスト
    /// を返す（REQ-0025 の GC 契機の判定に使う）。
    pub async fn put(
        &self,
        repository: &str,
        tag: &str,
        digest: &str,
    ) -> std::io::Result<Option<String>> {
        let mut index = self.read().await?;
        let ref_name = Self::ref_name(repository, tag);
        let position = index
            .manifests
            .iter()
            .position(|m| m.annotations.get(REF_NAME_ANNOTATION) == Some(&ref_name));
        let previous_digest = position.map(|i| index.manifests[i].digest.clone());

        let descriptor = ManifestDescriptor {
            media_type: MANIFEST_MEDIA_TYPE.to_string(),
            digest: digest.to_string(),
            size: 0,
            annotations: HashMap::from([(REF_NAME_ANNOTATION.to_string(), ref_name)]),
        };
        match position {
            Some(i) => index.manifests[i] = descriptor,
            None => index.manifests.push(descriptor),
        }
        self.write(&index).await?;
        Ok(previous_digest.filter(|previous| previous != digest))
    }

    /// 記録を削除する。実際に何かを削除できたかを返す。
    pub async fn remove(&self, repository: &str, tag: &str) -> std::io::Result<bool> {
        let mut index = self.read().await?;
        let ref_name = Self::ref_name(repository, tag);
        let before = index.manifests.len();
        index
            .manifests
            .retain(|m| m.annotations.get(REF_NAME_ANNOTATION) != Some(&ref_name));
        let removed = index.manifests.len() != before;
        if removed {
            self.write(&index).await?;
        }
        Ok(removed)
    }

    /// 記録済みのすべてのマニフェストダイジェスト（GC の到達判定の起点）。
    pub async fn all_digests(&self) -> std::io::Result<Vec<String>> {
        Ok(self
            .read()
            .await?
            .manifests
            .into_iter()
            .map(|m| m.digest)
            .collect())
    }

    async fn read(&self) -> std::io::Result<IndexFile> {
        match tokio::fs::read(&self.path).await {
            // 解析に失敗した内容を空の索引として扱うと、以降の put/remove が
            // それを「正しく読めた空の索引」として上書き保存し、既存の
            // タグ記録を黙って失う。GC（`compute_live_set`）もこの索引を
            // 空と誤認し、まだ参照されている blob を回収してしまう
            // （CodeRabbit レビュー指摘）。壊れている場合は呼び出し元に
            // エラーとして伝える。
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(IndexFile::default()),
            Err(err) => Err(err),
        }
    }

    async fn write(&self, index: &IndexFile) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let content =
            serde_json::to_vec_pretty(index).expect("IndexFile のシリアライズに失敗することはない");
        let tmp_path = self.path.with_extension("json.tmp");
        tokio::fs::write(&tmp_path, &content).await?;
        tokio::fs::rename(&tmp_path, &self.path).await?;
        Ok(())
    }
}
