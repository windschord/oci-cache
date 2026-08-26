# oci-cache

複数の上流レジストリに対応する、**パスを変えない**プルスルーキャッシュ。Web UI を同梱した単一実行ファイルとして動作する。

> **状態: 要求定義フェーズ。実装は未着手。**

## 解こうとしている問題

ホームラボでコンテナレジストリのキャッシュを立てようとすると、既存の選択肢はいずれかの点で外れる。

| | 複数上流 | パス無変更 | UI | 構成要素 |
|---|---|---|---|---|
| distribution (registry:2) | ✗ 1上流のみ | ○ | ✗ | 1 |
| zot (sync 拡張) | ○ | ✗ destination プレフィックス | ○ | 1 |
| Harbor proxy cache | ○ | ✗ プロジェクト名が必要 | ○ | 多数 + PostgreSQL |
| Nexus OSS group repo | ○ | ○ | △ 汎用ブラウザ | JVM |
| rpardini/docker-registry-proxy | ○ | ◎ 完全透過 | ✗ | 1 |
| **oci-cache** | **○** | **○** | **○** | **1** |

「パスを変えない」と「UI がある」を同時に満たす既製品が見当たらなかったことが、このプロジェクトの出発点。

## 方式

パスにプレフィックスを付けずに複数の上流を捌くために、**取得元の判別をパスではなく別の2つの手掛かりで行う**。

```
GET /v2/library/nginx/manifests/latest
  │
  ├─ ① 名前空間ヒント (?ns=docker.io) がある
  │     → containerd 経由。上流が確定するので探索しない
  │
  ├─ ② ルーティングメモに記録がある
  │     → 過去に解決済み。記録された上流へ直接
  │
  └─ ③ どちらも無い
        → 順序フォールバック: docker.io → ghcr.io → quay.io
           最初に成功した上流を採用し、①②のために記録する
           応答しない上流は待ち時間で打ち切って次へ進む（REQ-0057）
           記録した上流が不存在を返したら記録を破棄して③をやり直す（REQ-0056）
           設定順序を変えたときも記録を破棄する（REQ-0058）
```

- **①** は containerd が `hosts.toml` 経由のミラー要求に付与する `ns` クエリパラメータを使う。Kubernetes からの取得はこれで決定的に解決する
- **③** は Nexus の group repository と同じ考え方。Docker Engine は名前空間ヒントを送らないため、ワークステーションからの取得はこの経路になる
- 名前衝突は設定順序で決定的に解決する（REQ-0007）。どの上流から取得したかは UI に表示する（REQ-0041）

### 保存

OCI Image Layout 仕様に準拠する。blob は全上流で共有して重複排除し、タグの情報だけを上流ごとに分離する。

```
data/
  blobs/sha256/<digest>            全上流で共有（重複排除）
  upstreams/docker.io/index.json   タグ空間は上流ごと（取得元を追跡可能）
  upstreams/ghcr.io/index.json
  upstreams/quay.io/index.json
  index.redb                       ルーティングメモ・統計・退避順序
```

標準仕様に従うので、保存内容は `skopeo` や `crane` で直接検査できる。

## 範囲

- **やること**: 取得系エンドポイントのみ。公開イメージのキャッシュ。Web UI と Prometheus 形式のメトリクス
- **やらないこと**: 書き込み系エンドポイント（REQ-0031）。非公開イメージの保持（REQ-0013）

非公開イメージを保持しないのは意図的な判断で、認証を経て取得した内容を保存すると認証を持たない下流へ配信され、上流のアクセス制御を迂回する経路になるため。認証情報は Docker Hub の要求回数制限を緩和する目的に限って用い（REQ-0016）、保存するのは認証情報なしで取得できることを確認できたイメージに限る（REQ-0019）。

## 要求管理

要求は `docs/requirements/` の YAML レジストリで管理し、機械検証で整合性を保つ。設計は永続化せず PR 本文に書く。

```bash
python3 scripts/reqctl.py validate --strict   # 検査
python3 scripts/reqctl.py generate            # generated/ を再生成
python3 scripts/reqctl.py impact REQ-0002     # 変更前の影響範囲確認
python3 scripts/reqctl.py stats               # 件数サマリ
```

| ドキュメント | 内容 |
|---|---|
| [generated/index.md](docs/requirements/generated/index.md) | 全要求一覧 |
| [generated/traceability.md](docs/requirements/generated/traceability.md) | ストーリー → 要求 → 検証手段 |
| [generated/graph.md](docs/requirements/generated/graph.md) | 要求間の関係グラフ |

PR では `.github/workflows/req-lint.yml` が同じ検査（`validate --strict` と `generated/` の再生成漏れ）を実行する。

現状: 要求 50 件（active 49 / superseded 1）、ストーリー 7 件、用語 16 件。

主な既定値:

| 項目 | 既定 | 要求 |
|---|---|---|
| タグ参照マニフェストの有効期間 | 30分 | REQ-0015 |
| ネガティブキャッシュの有効期間 | 30分 | REQ-0005 |
| 保存領域の上限 | ファイルシステム全容量の 50% | REQ-0026 |
| 常駐メモリの上限 | 設けない（OS / コンテナに委ねる） | REQ-0055 |

有効期間は UI からの強制再取得で待たずに更新できる（REQ-0017）。保存領域の上限は設定で上書きできる（REQ-0027）。

## 技術選定

| 領域 | 採用 | 理由 |
|---|---|---|
| 言語 | Rust | サーバ側は Go でも自作が必要（ggcr の `pkg/registry` はテスト用途と明記されている）ため、言語選択の決め手が「既存の Rust 製ツールとの収束」になった |
| HTTP | axum 0.8 | |
| 上流通信 | oci-client 0.17 | `pull_blob_stream_partial` によるレジューム可能なストリーム取得（REQ-0051 の前提） |
| 保存 | OCI Image Layout + redb 4 | 標準仕様 + 単一ファイルで完結する索引 |
| UI 同梱 | rust-embed 8 | 単一実行ファイル要件（REQ-0043 / REQ-0050） |

## 実装の進め方

[OCI distribution-spec の conformance テストスイート](https://github.com/opencontainers/distribution-spec/tree/main/conformance) を **最初に CI へ組み込み、Red の状態から始める**。フルスクラッチで実際に刺さるのは認証でも保存でもなく、`Docker-Content-Digest` ヘッダ、メディアタイプのネゴシエーション、マニフェストリストの扱い、範囲指定取得といった仕様の細部であるため。

想定順序:

1. ルーティング解決（REQ-0001 / 0002 / 0003 / 0007 / 0009 / 0056 / 0057 / 0058）
2. 上流からの取得と中継（REQ-0010 / 0051）
3. OCI Image Layout による保存（REQ-0020 / 0021 / 0022）
4. 仕様準拠（REQ-0030 〜 0036）
5. UI と観測（REQ-0040 〜 0044）

## ライセンス

Apache-2.0
