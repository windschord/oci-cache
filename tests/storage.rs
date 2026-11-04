//! OCI Image Layout による永続化のテスト。
//!
//! 対象要求: REQ-0020 〜 REQ-0029（`CLAUDE.md` の実装順序 3.）。
//! テスト名は `docs/requirements/storage.yaml` の `verification.ref` と一致させてある。

use oci_cache::cache::{ManifestCache, ManifestReference, OciBlobSource};
use oci_cache::routing::Upstream;
use oci_cache::storage::{filesystem_capacity, Layout, TagIndex};
use sha2::{Digest, Sha256};
use tempfile::tempdir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn digest_of(content: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(content)))
}

async fn spawn_upstream(id: &str) -> (Upstream, MockServer) {
    let server = MockServer::start().await;
    (Upstream::new(id, server.uri()), server)
}

/// 認証方式の確認（`GET /v2/`）に challenge の無い成功応答を返すよう登録する。
async fn register_anonymous_v2(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/v2/"))
        .respond_with(ResponseTemplate::new(200))
        .mount(server)
        .await;
}

async fn register_blob(server: &MockServer, repository: &str, digest: &str, content: &[u8]) {
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repository}/blobs/{digest}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
        .mount(server)
        .await;
}

async fn register_manifest(server: &MockServer, repository: &str, reference: &str, content: &[u8]) {
    Mock::given(method("GET"))
        .and(path(format!("/v2/{repository}/manifests/{reference}")))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(content.to_vec()))
        .mount(server)
        .await;
}

/// REQ-0020: 保存するデータの配置は OCI Image Layout 仕様に準拠する。
/// `upstreams/<registry>/` 配下に `oci-layout` と `index.json`、共有
/// `blobs/` へのシンボリックリンクが揃い、そのディレクトリ単体が検査可能
/// になっていることを確認する。
#[tokio::test]
async fn layout_conforms_to_oci_image_layout() {
    let root = tempdir().unwrap();
    let layout = Layout::new(root.path(), Some(u64::MAX)).unwrap();

    let content = b"manifest content for layout conformance test".to_vec();
    let digest = digest_of(&content);
    layout.store_blob(&digest, &content).await.unwrap();
    layout
        .put_tag("docker.io", "library/nginx", "latest", &digest)
        .await
        .unwrap();

    let upstream_dir = root.path().join("upstreams").join("docker.io");

    let layout_marker: serde_json::Value = serde_json::from_slice(
        &tokio::fs::read(upstream_dir.join("oci-layout"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(layout_marker["imageLayoutVersion"], "1.0.0");

    let index: serde_json::Value = serde_json::from_slice(
        &tokio::fs::read(upstream_dir.join("index.json"))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(index["schemaVersion"], 2);
    let manifests = index["manifests"].as_array().unwrap();
    assert_eq!(manifests.len(), 1);
    assert_eq!(manifests[0]["digest"], digest);
    assert_eq!(
        manifests[0]["annotations"]["org.opencontainers.image.ref.name"],
        "library/nginx:latest"
    );

    // blobs はシンボリックリンクで共有ストアを指し、実体をそのまま読める
    let blobs_link = upstream_dir.join("blobs");
    let metadata = tokio::fs::symlink_metadata(&blobs_link).await.unwrap();
    assert!(metadata.file_type().is_symlink());
    let hex = digest.strip_prefix("sha256:").unwrap();
    let via_symlink = tokio::fs::read(blobs_link.join("sha256").join(hex))
        .await
        .unwrap();
    assert_eq!(via_symlink, content);
}

/// REQ-0021: 複数の上流レジストリから取得したイメージが同一ダイジェストの
/// blob を含む時、その blob は1つだけ保存する。
#[tokio::test]
async fn identical_blob_stored_once_across_upstreams() {
    let (docker, docker_server) = spawn_upstream("docker.io").await;
    let (ghcr, ghcr_server) = spawn_upstream("ghcr.io").await;
    let repo = "library/nginx";
    let content = b"shared base layer".to_vec();
    let digest = digest_of(&content);
    register_anonymous_v2(&docker_server).await;
    register_anonymous_v2(&ghcr_server).await;
    register_blob(&docker_server, repo, &digest, &content).await;
    register_blob(&ghcr_server, repo, &digest, &content).await;

    let root = tempdir().unwrap();
    let layout = Layout::new(root.path(), Some(u64::MAX)).unwrap();

    let path_a = layout.get_blob(&docker, repo, &digest).await.unwrap();
    let path_b = layout.get_blob(&ghcr, repo, &digest).await.unwrap();

    assert_eq!(path_a, path_b);
    // 2回目（ghcr.io 経由）は保存済みのため、ghcr.io へは blob 要求が
    // 発生しない
    let blob_path = format!("/v2/{repo}/blobs/{digest}");
    assert!(ghcr_server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|request| request.url.path() != blob_path));
}

/// REQ-0022: 保存するタグの情報は上流レジストリごとに分離して管理する。
#[tokio::test]
async fn tag_namespace_is_separated_per_upstream() {
    let root = tempdir().unwrap();
    let layout = Layout::new(root.path(), Some(u64::MAX)).unwrap();
    let repo = "library/nginx";
    let tag = "latest";
    let digest_a = digest_of(b"docker.io content");
    let digest_b = digest_of(b"ghcr.io content");

    layout
        .put_tag("docker.io", repo, tag, &digest_a)
        .await
        .unwrap();
    layout
        .put_tag("ghcr.io", repo, tag, &digest_b)
        .await
        .unwrap();

    assert_eq!(
        layout.get_tag("docker.io", repo, tag).await.unwrap(),
        Some(digest_a)
    );
    assert_eq!(
        layout.get_tag("ghcr.io", repo, tag).await.unwrap(),
        Some(digest_b)
    );
    assert!(root.path().join("upstreams/docker.io/index.json").exists());
    assert!(root.path().join("upstreams/ghcr.io/index.json").exists());
}

/// REQ-0023: 保存済みデータの合計サイズが設定された上限に達した時、最終
/// 参照時刻が古い blob から順に削除する。
#[tokio::test]
async fn eviction_removes_least_recently_used_blob() {
    let root = tempdir().unwrap();
    let content_a = vec![0xAAu8; 100];
    let content_b = vec![0xBBu8; 100];
    let digest_a = digest_of(&content_a);
    let digest_b = digest_of(&content_b);

    // 片方しか収まらない上限にする
    let layout = Layout::new(root.path(), Some(150)).unwrap();

    layout.store_blob(&digest_a, &content_a).await.unwrap();
    layout.store_blob(&digest_b, &content_b).await.unwrap();

    let blobs_dir = root.path().join("blobs/sha256");
    let hex_a = digest_a.strip_prefix("sha256:").unwrap();
    let hex_b = digest_b.strip_prefix("sha256:").unwrap();
    assert!(
        !blobs_dir.join(hex_a).exists(),
        "先に保存した blob が退避されていない"
    );
    assert!(
        blobs_dir.join(hex_b).exists(),
        "後から保存した blob まで消えている"
    );
}

/// REQ-0023（続き）: 単体で容量上限を超える blob を保存した直後、退避処理が
/// その blob 自身を削除してしまわないことを確認する（CodeRabbit レビュー
/// 指摘: `touch` した直後の digest は通常もっとも新しく退避されにくいが、
/// 他に削れる blob が無くなった末尾では自分自身が最古の候補として選ばれ
/// うる。削除すると呼び出し元に返すパスの実体が消える）。
#[tokio::test]
async fn freshly_stored_blob_exceeding_capacity_alone_is_not_evicted() {
    let root = tempdir().unwrap();
    let content = vec![0xCCu8; 100];
    let digest = digest_of(&content);

    // どの blob 単体の容量（100バイト）よりも小さい上限にする
    let layout = Layout::new(root.path(), Some(10)).unwrap();

    let path = layout.store_blob(&digest, &content).await.unwrap();

    assert_eq!(
        tokio::fs::read(&path).await.unwrap(),
        content,
        "保存した直後の blob が、自分自身の退避によって消えている"
    );
}

/// REQ-0024: 上流レジストリから blob を取得した時、受信した内容から計算
/// したダイジェストが要求したダイジェストと一致しない場合、その内容を
/// 保存せず誤りを返す。
#[tokio::test]
async fn digest_mismatch_rejects_and_does_not_store() {
    let server = MockServer::start().await;
    let repo = "library/nginx";
    let actual_content = b"actual content".to_vec();
    let wrong_digest = digest_of(b"completely different content");

    register_anonymous_v2(&server).await;
    // 要求されたダイジェストに対して、一致しない内容を返す上流を再現する
    register_blob(&server, repo, &wrong_digest, &actual_content).await;

    let upstream = Upstream::new("docker.io", server.uri());
    let root = tempdir().unwrap();
    let layout = Layout::new(root.path(), Some(u64::MAX)).unwrap();

    let result = layout.get_blob(&upstream, repo, &wrong_digest).await;

    assert!(result.is_err());
    let hex = wrong_digest.strip_prefix("sha256:").unwrap();
    assert!(!root.path().join("blobs/sha256").join(hex).exists());
}

/// REQ-0025: マニフェストが置換された時、どのマニフェストからも参照され
/// なくなった blob を削除する。
#[tokio::test]
async fn unreferenced_blob_is_collected() {
    let root = tempdir().unwrap();
    let layout = Layout::new(root.path(), Some(u64::MAX)).unwrap();
    let repo = "library/nginx";
    let tag = "latest";

    let old_config = b"old config".to_vec();
    let old_config_digest = digest_of(&old_config);
    let old_layer = b"old layer content".to_vec();
    let old_layer_digest = digest_of(&old_layer);
    let old_manifest_bytes = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "config": {"digest": old_config_digest, "mediaType": "application/vnd.oci.image.config.v1+json", "size": 0},
        "layers": [{"digest": old_layer_digest, "mediaType": "application/vnd.oci.image.layer.v1.tar", "size": 0}],
    }))
    .unwrap();
    let old_manifest_digest = digest_of(&old_manifest_bytes);

    layout
        .store_blob(&old_config_digest, &old_config)
        .await
        .unwrap();
    layout
        .store_blob(&old_layer_digest, &old_layer)
        .await
        .unwrap();
    layout
        .store_blob(&old_manifest_digest, &old_manifest_bytes)
        .await
        .unwrap();
    layout
        .put_tag("docker.io", repo, tag, &old_manifest_digest)
        .await
        .unwrap();

    // 新しいマニフェストへ置き換える
    let new_config = b"new config".to_vec();
    let new_config_digest = digest_of(&new_config);
    let new_layer = b"new layer content".to_vec();
    let new_layer_digest = digest_of(&new_layer);
    let new_manifest_bytes = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "config": {"digest": new_config_digest, "mediaType": "application/vnd.oci.image.config.v1+json", "size": 0},
        "layers": [{"digest": new_layer_digest, "mediaType": "application/vnd.oci.image.layer.v1.tar", "size": 0}],
    }))
    .unwrap();
    let new_manifest_digest = digest_of(&new_manifest_bytes);

    layout
        .store_blob(&new_config_digest, &new_config)
        .await
        .unwrap();
    layout
        .store_blob(&new_layer_digest, &new_layer)
        .await
        .unwrap();
    layout
        .store_blob(&new_manifest_digest, &new_manifest_bytes)
        .await
        .unwrap();
    // 同じタグを新しいダイジェストで上書きする。既存の記録と異なるため GC
    // が走る
    layout
        .put_tag("docker.io", repo, tag, &new_manifest_digest)
        .await
        .unwrap();

    let blobs_dir = root.path().join("blobs/sha256");
    let exists = |digest: &str| {
        blobs_dir
            .join(digest.strip_prefix("sha256:").unwrap())
            .exists()
    };

    assert!(
        !exists(&old_manifest_digest),
        "置き換えられた旧マニフェストが残っている"
    );
    assert!(
        !exists(&old_config_digest),
        "どこからも参照されない旧 config が残っている"
    );
    assert!(
        !exists(&old_layer_digest),
        "どこからも参照されない旧レイヤーが残っている"
    );
    assert!(exists(&new_manifest_digest));
    assert!(exists(&new_config_digest));
    assert!(exists(&new_layer_digest));
}

/// REQ-0025（続き）: タグが Image Index（マニフェストリスト）を指す場合、
/// その子マニフェストが参照する config・layers（2階層目）も GC で回収
/// されない（CodeRabbit レビュー指摘: 到達判定が1階層しか辿っていないと、
/// 子マニフェスト自身は live のまま、その config・layers だけ誤って
/// 回収してしまう）。
#[tokio::test]
async fn gc_preserves_layers_referenced_through_image_index() {
    let root = tempdir().unwrap();
    let layout = Layout::new(root.path(), Some(u64::MAX)).unwrap();
    let repo = "library/nginx";
    let tag = "latest";

    let config = b"amd64 config".to_vec();
    let config_digest = digest_of(&config);
    let layer = b"amd64 layer content".to_vec();
    let layer_digest = digest_of(&layer);
    let child_manifest_bytes = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "config": {"digest": config_digest, "mediaType": "application/vnd.oci.image.config.v1+json", "size": 0},
        "layers": [{"digest": layer_digest, "mediaType": "application/vnd.oci.image.layer.v1.tar", "size": 0}],
    }))
    .unwrap();
    let child_manifest_digest = digest_of(&child_manifest_bytes);

    let index_bytes = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{"digest": child_manifest_digest, "mediaType": "application/vnd.oci.image.manifest.v1+json", "size": 0, "platform": {"architecture": "amd64", "os": "linux"}}],
    }))
    .unwrap();
    let index_digest = digest_of(&index_bytes);

    layout.store_blob(&config_digest, &config).await.unwrap();
    layout.store_blob(&layer_digest, &layer).await.unwrap();
    layout
        .store_blob(&child_manifest_digest, &child_manifest_bytes)
        .await
        .unwrap();
    layout
        .store_blob(&index_digest, &index_bytes)
        .await
        .unwrap();
    layout
        .put_tag("docker.io", repo, tag, &index_digest)
        .await
        .unwrap();

    // put_tag 自身は置き換え前が無い（初回）ため GC を起こさない。到達
    // 判定そのものを確認したいので、ここで明示的に呼ぶ。
    layout.collect_garbage().await.unwrap();

    let blobs_dir = root.path().join("blobs/sha256");
    let exists = |digest: &str| {
        blobs_dir
            .join(digest.strip_prefix("sha256:").unwrap())
            .exists()
    };

    assert!(exists(&index_digest));
    assert!(
        exists(&child_manifest_digest),
        "Image Index の子マニフェストが回収されている"
    );
    assert!(
        exists(&config_digest),
        "子マニフェストが参照する config が誤って回収されている"
    );
    assert!(
        exists(&layer_digest),
        "子マニフェストが参照する layer が誤って回収されている"
    );
}

/// REQ-0022（続き）: タグ索引（`index.json`）が壊れている（解析できない）
/// 場合、空の索引として黙って読み替えず、エラーとして伝える（CodeRabbit
/// レビュー指摘: 黙って読み替えると、以降の書き込みが既存のタグ記録を
/// 上書きして失い、GC がまだ参照されている blob を誤って回収しうる）。
#[tokio::test]
async fn corrupted_tag_index_is_reported_as_error_not_silently_reset() {
    let root = tempdir().unwrap();
    let layout = Layout::new(root.path(), Some(u64::MAX)).unwrap();
    let repo = "library/nginx";
    let tag = "latest";
    let digest = digest_of(b"some manifest content");

    layout
        .put_tag("docker.io", repo, tag, &digest)
        .await
        .unwrap();

    let index_path = root.path().join("upstreams/docker.io/index.json");
    tokio::fs::write(&index_path, b"not valid json")
        .await
        .unwrap();

    let result = layout.get_tag("docker.io", repo, tag).await;

    assert!(
        result.is_err(),
        "壊れた index.json を空の索引として読み替えてしまっている"
    );
}

/// REQ-0026: 保存領域の使用量の既定の上限は、保存先ファイルシステムの
/// 全容量の50パーセント。
#[test]
fn default_capacity_is_half_of_filesystem() {
    let root = tempdir().unwrap();
    let layout = Layout::new(root.path(), None).unwrap();
    let total = filesystem_capacity(root.path()).unwrap();
    assert_eq!(layout.capacity_bytes(), total / 2);
}

/// REQ-0027: 運用者が保存領域の上限を設定した時、既定値ではなく設定された
/// 値を上限として使う。
#[test]
fn configured_capacity_overrides_default() {
    let root = tempdir().unwrap();
    let layout = Layout::new(root.path(), Some(12345)).unwrap();
    assert_eq!(layout.capacity_bytes(), 12345);
}

/// REQ-0028: 退避によって削除された blob が以降の要求で必要になった時、
/// 記録された上流レジストリからその blob を再取得する。
#[tokio::test]
async fn evicted_blob_is_refetched_on_demand() {
    let server = MockServer::start().await;
    let repo = "library/nginx";
    let content_a = vec![0xAAu8; 100];
    let content_b = vec![0xBBu8; 100];
    let digest_a = digest_of(&content_a);
    let digest_b = digest_of(&content_b);

    register_anonymous_v2(&server).await;
    register_blob(&server, repo, &digest_a, &content_a).await;
    register_blob(&server, repo, &digest_b, &content_b).await;

    let upstream = Upstream::new("docker.io", server.uri());
    let root = tempdir().unwrap();
    let layout = Layout::new(root.path(), Some(150)).unwrap();

    layout.get_blob(&upstream, repo, &digest_a).await.unwrap();
    layout.get_blob(&upstream, repo, &digest_b).await.unwrap(); // digest_a が退避される

    let hex_a = digest_a.strip_prefix("sha256:").unwrap();
    assert!(!root.path().join("blobs/sha256").join(hex_a).exists());

    let requests_before = server.received_requests().await.unwrap().len();

    let path = layout.get_blob(&upstream, repo, &digest_a).await.unwrap();

    assert_eq!(tokio::fs::read(&path).await.unwrap(), content_a);
    let requests_after = server.received_requests().await.unwrap().len();
    assert!(
        requests_after > requests_before,
        "退避された blob が上流へ再取得されていない"
    );
}

/// REQ-0029: ダイジェストを指定してマニフェストを上流レジストリから取得
/// した時、受信した内容から計算したダイジェストが要求したダイジェストと
/// 一致しない場合、その内容を保存せず誤りを返す。
#[tokio::test]
async fn manifest_digest_mismatch_rejects_and_does_not_store() {
    let server = MockServer::start().await;
    let repo = "library/nginx";
    let actual_content = b"actual manifest content".to_vec();
    let wrong_digest = digest_of(b"completely different manifest content");

    register_anonymous_v2(&server).await;
    register_manifest(&server, repo, &wrong_digest, &actual_content).await;

    let upstream = Upstream::new("docker.io", server.uri());
    let store_dir = tempdir().unwrap();
    let cache = ManifestCache::new(store_dir.path(), OciBlobSource::new());

    let result = cache
        .get(
            &upstream,
            repo,
            &ManifestReference::Digest(wrong_digest.clone()),
        )
        .await;

    assert!(result.is_err());
    let hex = wrong_digest.strip_prefix("sha256:").unwrap();
    assert!(!store_dir.path().join("blobs/sha256").join(hex).exists());
}

/// タグが指すダイジェストが変わっていない時、`TagIndex::put` は
/// `index.json` への書き込みを行わない（CodeRabbit レビュー指摘:
/// `crate::server` はタグ参照が解決できるたびに（キャッシュ命中かどうかに
/// かかわらず）`put` を呼ぶため、これが無いと命中時にも毎回 index.json
/// の読み書きが発生する）。書き込みが実際に起きたかどうかを、ファイルの
/// 変更時刻が変わらないことで確認する。
#[tokio::test]
async fn tag_index_put_skips_write_when_digest_unchanged() {
    let dir = tempdir().unwrap();
    let index = TagIndex::new(dir.path().to_path_buf());
    let digest = digest_of(b"unchanged content");

    index.put("library/nginx", "latest", &digest).await.unwrap();
    let index_path = dir.path().join("index.json");
    let mtime_after_first_put = std::fs::metadata(&index_path).unwrap().modified().unwrap();

    // ファイルの変更時刻の分解能より確実に大きい間隔を空ける。
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let result = index.put("library/nginx", "latest", &digest).await.unwrap();
    assert_eq!(
        result, None,
        "ダイジェストが変わっていないので置き換えは発生していないはず"
    );
    let mtime_after_second_put = std::fs::metadata(&index_path).unwrap().modified().unwrap();
    assert_eq!(
        mtime_after_first_put, mtime_after_second_put,
        "ダイジェストが変わらない put で index.json が書き直されている"
    );
}
