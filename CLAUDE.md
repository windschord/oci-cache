# oci-cache — Claude Code 向けプロジェクト規約

複数の上流レジストリに対応する、パスを変えないプルスルーキャッシュ。Rust 製の単一実行ファイル。

**現状: 要求定義は完了、実装は未着手。** `src/main.rs` はスタブ。

## 最初に読むもの

1. `README.md` — 何を解こうとしているか、方式の全体像
2. `docs/requirements/generated/traceability.md` — ストーリー → 要求 → 検証手段の対応
3. `docs/requirements/generated/index.md` — 全要求一覧

## 要求管理のルール（重要）

要求は `docs/requirements/` の YAML レジストリが唯一の正。**設計ドキュメントは作らない。**

| すること | しないこと |
|---|---|
| 要求の追加・変更は YAML を編集して `validate` を通す | `generated/` を手で編集する（必ず `generate` で再生成） |
| 設計は PR 本文に書き、URL だけ `design_refs` に残す | `docs/design/` のような設計書を作る |
| 廃止は `status` を `deprecated` / `superseded` にして `retired_why` を書く | 要求の行を削除する |
| 実装したテスト名を要求の `verification` に登録する | 検証手段のない `active` 要求を残す |

```bash
python3 scripts/reqctl.py validate --strict   # 検査（エラーがあればコミットしない）
python3 scripts/reqctl.py generate            # generated/ を再生成
python3 scripts/reqctl.py impact REQ-0002     # 変更前に影響範囲を確認
python3 scripts/reqctl.py next-id req         # 採番
```

**要求文・数値の変更、廃止・置換、優先度の変更はユーザー承認が必須。** 追加と表現修正は差分提示で足りる。

要求の意味が変わる変更は「変更」ではなく「置換」。旧 ID を `superseded` にして新 ID に `supersedes` を書き、**新旧を同じコミットで**入れる。

## 実装の進め方

### conformance テストを先に置く

[OCI distribution-spec の conformance スイート](https://github.com/opencontainers/distribution-spec/tree/main/conformance) を CI に組み込み、**Red の状態から始める**。フルスクラッチで実際に刺さるのは認証でも保存でもなく、以下の仕様の細部であるため。

- `Docker-Content-Digest` ヘッダの欠落・不一致（REQ-0032）
- Accept ヘッダによるメディアタイプのネゴシエーション（REQ-0033）
- マニフェストリストをそのまま返すこと（REQ-0034）
- Range による範囲指定取得（REQ-0035）

push 系のテストは REQ-0031 により意図的に未実装なので skip 設定で落とす。skip するのは上流の conformance スイートが持つ push 系テストだけで、書き込み系エンドポイントが返す応答（REQ-0036 の HTTP 405）は自前の `tests/conformance.rs::push_request_is_rejected_as_not_allowed` で検証する。

### 順序

1. **ルーティング解決** — REQ-0001 / 0002 / 0003 / 0007 / 0009 / 0056 〜 0059
2. **上流からの取得と中継** — REQ-0010 / 0051（`oci-client` の `pull_blob_stream` を使う）
3. **OCI Image Layout による保存** — REQ-0020 / 0021 / 0022
4. **仕様準拠** — REQ-0030 〜 0036
5. **UI と観測** — REQ-0040 〜 0044 / 0017

### TDD

Red-Green-Refactor で進める。要求の `verification.ref` が指すテスト名をそのまま実装する。

```
tests/routing.rs      REQ-0001 〜 0009 / 0056 〜 0059
tests/cache.rs        REQ-0010 〜 0019
tests/storage.rs      REQ-0020 〜 0029
tests/conformance.rs  REQ-0030 〜 0036
tests/ui.rs           REQ-0040 〜 0044
tests/platform.rs     REQ-0050 〜 0055
```

テストを実装したら `validate --check-tests` が通るようになる。全テストが揃った時点で `.github/workflows/req-lint.yml` の `--check-tests` を有効化する。

## 設計上の要点

### ルーティングは3段

```
① ?ns= あり     → containerd 経由。上流が確定するので探索しない  (REQ-0001)
② メモに記録あり → 過去に解決済み。記録された上流へ直接           (REQ-0003)
③ どちらも無い   → 順序フォールバック docker.io → ghcr.io → quay.io (REQ-0002)
```

③ で解決したら ①② のために必ず記録する。③ の途中で応答しない上流があれば待ち時間（既定 5 秒）で打ち切って次へ進む（REQ-0057 / 0059）。記録した上流が不存在を返したら記録を破棄して ③ からやり直し（REQ-0056）、設定順序を変えたときも記録を破棄する（REQ-0058）。名前衝突は設定順序で決定的に解決し（REQ-0007）、取得元は UI に表示する（REQ-0041）。

### 保存は blob 共有・タグ空間分離

```
data/
  blobs/sha256/<digest>            全上流で共有（重複排除, REQ-0021）
  upstreams/<registry>/index.json  タグ空間は上流ごと（provenance, REQ-0022）
  index.redb                       ルーティングメモ・統計・退避順序
```

### 越えてはいけない制約

- **書き込み系エンドポイントを実装しない**（REQ-0031）
- **認証情報なしで取得できないイメージを保存しない**（REQ-0013）— 認証情報は Docker Hub の要求回数制限を緩和する目的に限る（REQ-0016）
- **blob 全体を主記憶に載せない**（REQ-0051）— `pull_blob_stream` / `pull_blob_stream_partial` を使う
- **メモリ上限を自前で持たない**（REQ-0055）— OS またはコンテナ実行環境に委ねる
- **イメージ参照に上流を識別するパス要素を要求しない**（REQ-0006）— これがプロジェクトの存在理由

## 技術スタック

依存解決と `cargo check --all-targets` は確認済み。

| 用途 | crate | 備考 |
|---|---|---|
| HTTP サーバ | axum 0.8 | |
| 上流通信 | oci-client 0.17 | oras-project 配下。`pull_blob_stream_partial` でレジューム可 |
| HTTP クライアント | reqwest 0.13 | **oci-client が 0.13 に依存**。0.12 に固定すると TLS スタックが二重になる。feature は `rustls`（0.12 の `rustls-tls` から改名） |
| 索引 | redb 4 | 単一ファイル。sled は 2024-10 以降停滞しているため不採用 |
| UI 同梱 | rust-embed 8 | `include_dir` は更新が止まっているため不採用 |
| 設定 | figment 0.10 | TOML パースは figment 経由。`toml` を直接依存に足すと版が二重になる |

## 規約

- ドキュメント・コミットメッセージ・コードコメントは日本語
- コミットメッセージは何を変えたかではなく**なぜ変えたか**を書く
- 要求に紐づく変更は本文に要求 ID を含める
- `cargo clippy -- -D warnings` と `cargo fmt --check` を通す
