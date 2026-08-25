# トレーサビリティマトリクス（自動生成 / 手で編集しないこと）

ユーザーストーリー → 要求 → 検証手段。テストの合格条件はこの表がすべて埋まっていること。
取下げ（dropped）のストーリーは達成対象外のため掲載しない。

| ストーリー | 要求 | 要求文 | 検証手段 |
|------------|------|--------|----------|
| US-001 | REQ-0002 | 名前空間ヒントが無くルーティングメモにも記録が無いリポジトリ参照を要求された時、システムは設定された順序で上流レジストリに問い合わせ、最初に成功応答を返した上流の結果をクライアントへ返さなければならない | test:tests/routing.rs::ordered_fallback_returns_first_success |
| US-001 | REQ-0003 | 順序フォールバックによって上流レジストリが確定した時、システムはリポジトリ参照と上流レジストリの対応を記録し、以降の同一リポジトリ参照への要求では探索を行わず記録された上流へ問い合わせなければならない | test:tests/routing.rs::routing_memo_skips_probe_on_second_request |
| US-001 | REQ-0006 | システムはクライアントが指定するイメージ参照に、上流レジストリを識別するための追加のパス要素を要求してはならない | test:tests/routing.rs::pull_path_has_no_upstream_prefix |
| US-001 | REQ-0007 | 同一のリポジトリ参照が複数の上流レジストリに存在する時、システムは設定順序が最も先である上流レジストリの応答を返さなければならない | test:tests/routing.rs::name_collision_resolved_by_configured_order |
| US-001 | REQ-0009 | 運用者が上流レジストリの識別子を順序付きで設定した時、システムは設定された順序を上流レジストリへの問い合わせ順序として使用しなければならない | test:tests/routing.rs::configured_upstream_order_is_used |
| US-001 | REQ-0054 | 運用者が証明書と秘密鍵を設定した時、システムは TLS による接続を受け付けなければならない | test:tests/platform.rs::tls_listener_accepts_configured_certificate |
| US-001 | REQ-0056 | 記録された上流レジストリがリポジトリ参照に対して不存在を返した時、システムはその記録を破棄して順序フォールバックを再実行しなければならない | test:tests/routing.rs::routing_memo_discarded_when_recorded_upstream_misses |
| US-001 | REQ-0057 | 上流レジストリへの問い合わせが設定された待ち時間を超えて応答しない時、システムはその上流レジストリを失敗として扱い、順序フォールバックを次の上流レジストリへ進めなければならない | test:tests/routing.rs::unresponsive_upstream_advances_to_next |
| US-002 | REQ-0001 | クライアントが名前空間ヒントを付与して要求を送信した時、システムは名前空間ヒントが示す上流レジストリのみに問い合わせなければならない | test:tests/routing.rs::ns_hint_selects_single_upstream |
| US-002 | REQ-0006 | システムはクライアントが指定するイメージ参照に、上流レジストリを識別するための追加のパス要素を要求してはならない | test:tests/routing.rs::pull_path_has_no_upstream_prefix |
| US-003 | REQ-0009 | 運用者が上流レジストリの識別子を順序付きで設定した時、システムは設定された順序を上流レジストリへの問い合わせ順序として使用しなければならない | test:tests/routing.rs::configured_upstream_order_is_used |
| US-003 | REQ-0017 | 利用者が Web UI から再取得を指示した時、システムは有効期間の残りにかかわらず上流レジストリへ問い合わせ、保存内容を更新しなければならない | test:tests/ui.rs::force_refresh_bypasses_remaining_ttl |
| US-003 | REQ-0018 | 運用者がタグ参照マニフェストまたはネガティブキャッシュの有効期間を設定した時、システムは既定値ではなく設定された値を有効期間として使用しなければならない | test:tests/cache.rs::configured_ttl_overrides_default |
| US-003 | REQ-0040 | 利用者が Web UI を開いた時、システムは保存済みのリポジトリ参照とタグの一覧を表示しなければならない | test:tests/ui.rs::catalog_lists_stored_repositories |
| US-003 | REQ-0041 | Web UI が保存済みのイメージを表示する時、システムはその取得元である上流レジストリを併せて表示しなければならない | test:tests/ui.rs::image_entry_shows_source_upstream |
| US-003 | REQ-0042 | 利用者が Web UI を開いた時、システムはキャッシュの命中回数と不命中回数、および保存領域の使用量を表示しなければならない | test:tests/ui.rs::stats_endpoint_reports_hit_and_miss |
| US-003 | REQ-0044 | 監視系がメトリクスの取得を要求した時、システムは Prometheus 形式でキャッシュ統計を返さなければならない | test:tests/ui.rs::metrics_endpoint_exposes_prometheus_format |
| US-004 | REQ-0003 | 順序フォールバックによって上流レジストリが確定した時、システムはリポジトリ参照と上流レジストリの対応を記録し、以降の同一リポジトリ参照への要求では探索を行わず記録された上流へ問い合わせなければならない | test:tests/routing.rs::routing_memo_skips_probe_on_second_request |
| US-004 | REQ-0004 | すべての上流レジストリが対象のリポジトリ参照に対して不存在を返した時、システムはその結果を有効期間付きで記録し、有効期間内は上流レジストリへ再問い合わせせずに不存在を返さなければならない | test:tests/routing.rs::negative_cache_suppresses_reprobe |
| US-004 | REQ-0005 | システムはネガティブキャッシュの既定の有効期間を30分としなければならない | test:tests/routing.rs::negative_cache_default_ttl_is_30_minutes |
| US-004 | REQ-0010 | 上流レジストリから blob を取得した時、システムはその blob をダイジェストを鍵として保存し、以降の同一ダイジェストへの要求に対して上流レジストリへ問い合わせずに応答しなければならない | test:tests/cache.rs::blob_served_from_store_without_upstream_call |
| US-004 | REQ-0011 | クライアントがタグを指定してマニフェストを要求し、かつ保存済みマニフェストの有効期間が経過している時、システムは上流レジストリへ再問い合わせしなければならない | test:tests/cache.rs::expired_tag_manifest_triggers_revalidation |
| US-004 | REQ-0012 | クライアントがダイジェストを指定してマニフェストを要求した時、システムは保存済みマニフェストが存在すれば有効期間を確認せずに応答しなければならない | test:tests/cache.rs::digest_manifest_never_revalidated |
| US-004 | REQ-0014 | 上流レジストリへの問い合わせが失敗した時、システムは保存済みの内容が存在すればそれを返し、その応答が再検証されていないことをクライアントへ示さなければならない | test:tests/cache.rs::stale_content_served_when_upstream_unreachable |
| US-004 | REQ-0015 | システムはタグを指定して取得したマニフェストの既定の有効期間を30分としなければならない | test:tests/cache.rs::tag_manifest_default_ttl_is_30_minutes |
| US-004 | REQ-0016 | 運用者が上流レジストリの認証情報を設定した時、システムは当該上流レジストリへの問い合わせにその認証情報を使用しなければならない | test:tests/cache.rs::configured_credentials_used_for_upstream_request |
| US-004 | REQ-0017 | 利用者が Web UI から再取得を指示した時、システムは有効期間の残りにかかわらず上流レジストリへ問い合わせ、保存内容を更新しなければならない | test:tests/ui.rs::force_refresh_bypasses_remaining_ttl |
| US-004 | REQ-0018 | 運用者がタグ参照マニフェストまたはネガティブキャッシュの有効期間を設定した時、システムは既定値ではなく設定された値を有効期間として使用しなければならない | test:tests/cache.rs::configured_ttl_overrides_default |
| US-005 | REQ-0023 | 保存済みデータの合計サイズが設定された上限に達した時、システムは最終参照時刻が古い blob から順に削除しなければならない | test:tests/storage.rs::eviction_removes_least_recently_used_blob |
| US-005 | REQ-0025 | マニフェストが削除または置換された時、システムはどのマニフェストからも参照されなくなった blob を削除しなければならない | test:tests/storage.rs::unreferenced_blob_is_collected |
| US-005 | REQ-0026 | システムは保存領域の使用量の既定の上限を、保存先ファイルシステムの全容量の50パーセントとしなければならない | test:tests/storage.rs::default_capacity_is_half_of_filesystem |
| US-005 | REQ-0027 | 運用者が保存領域の上限を設定した時、システムは既定値ではなく設定された値を上限として使用しなければならない | test:tests/storage.rs::configured_capacity_overrides_default |
| US-005 | REQ-0028 | 退避によって削除された blob が以降の要求で必要になった時、システムは記録された上流レジストリからその blob を再取得しなければならない | test:tests/storage.rs::evicted_blob_is_refetched_on_demand |
| US-006 | REQ-0050 | システムは外部のデータベースと追加の常駐プロセスを必要とせず、単一の実行ファイルで動作しなければならない | review:配布物が単一実行ファイルであることの確認手順 |
| US-006 | REQ-0051 | システムは blob を上流レジストリから取得してクライアントへ中継する時、その blob の全体を主記憶上に保持してはならない | test:tests/platform.rs::large_blob_relay_does_not_buffer_whole_body |
| US-006 | REQ-0053 | システムは linux/arm64 と linux/amd64 の双方で動作する実行ファイルを提供しなければならない | review:双方のアーキテクチャ向け成果物が生成されることの確認手順 |
| US-006 | REQ-0055 | システムは常駐メモリの使用量に自身で上限を設けず、実行環境が課す制限に従って動作しなければならない | review:独自のメモリ上限機構を持たないことを確認するコードレビュー手順 |
| US-007 | REQ-0013 | 上流レジストリから取得したイメージが認証情報なしでは取得できないものである時、システムはその内容を保存してはならない | test:tests/cache.rs::authenticated_content_is_not_persisted |
| US-007 | REQ-0019 | システムは認証情報を用いない問い合わせで取得できることを確認できたイメージに限り、その内容を保存しなければならない | test:tests/cache.rs::persistence_requires_anonymous_pull_success |

## ストーリー未紐付けの有効要求

- REQ-0008: システムは docker.io、ghcr.io、quay.io の3つの上流レジストリへの問い合わせに対応しなければならない
- REQ-0020: システムは保存するデータの配置を OCI Image Layout 仕様に準拠させなければならない
- REQ-0021: 複数の上流レジストリから取得したイメージが同一ダイジェストの blob を含む時、システムはその blob を1つだけ保存しなければならない
- REQ-0022: システムは保存するタグの情報を上流レジストリごとに分離して管理しなければならない
- REQ-0024: 上流レジストリから blob を取得した時、システムは受信した内容から計算したダイジェストが要求したダイジェストと一致しない場合、その内容を保存せず誤りを返さなければならない
- REQ-0029: ダイジェストを指定してマニフェストを上流レジストリから取得した時、システムは受信した内容から計算したダイジェストが要求したダイジェストと一致しない場合、その内容を保存せず誤りを返さなければならない
- REQ-0030: システムは OCI Distribution Specification が定める取得系エンドポイントに準拠して応答しなければならない
- REQ-0031: システムは OCI Distribution Specification が定める書き込み系エンドポイントを実装してはならない
- REQ-0032: システムはマニフェストの応答に Docker-Content-Digest ヘッダを含め、その値を応答本文から計算したダイジェストと一致させなければならない
- REQ-0033: クライアントが Accept ヘッダで受け入れ可能なメディアタイプを指定した時、システムは指定された範囲に含まれるメディアタイプでマニフェストを返さなければならない
- REQ-0034: 要求されたマニフェストがマニフェストリストである時、システムはプラットフォームの選択を行わずマニフェストリストのまま返さなければならない
- REQ-0035: クライアントが Range ヘッダを指定して blob を要求した時、システムは指定された範囲のみを返さなければならない
- REQ-0036: クライアントが書き込み系エンドポイントへ要求を送信した時、システムは要求が許可されていないことを示す応答を返さなければならない
- REQ-0043: システムは Web UI の静的資産を実行ファイルに埋め込み、追加のファイル配置を伴わずに配信しなければならない

## カバレッジ

- 検証手段が定義された有効要求: 48/48（100%）
