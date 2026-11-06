#!/usr/bin/env bash
#
# OCI distribution-spec の conformance スイートを oci-cache に対して実行する。
#
# CI と手元で同じ手順を踏めるようにスクリプトへ寄せてある。ワークフローに
# 直接書くと手元で再現できず、Red が実装の不足なのか CI 固有の事情なのかを
# 切り分けられなくなるため。
#
# 取得系のみを対象にする。書き込み系は REQ-0031 により実装しない方針なので、
# 上流スイートの push / content management / content discovery は無効にする。
# 書き込み系エンドポイントが HTTP 405 を返すこと（REQ-0036）の確認は
# tests/conformance.rs::push_request_is_rejected_as_not_allowed が受け持つ。
#
# 実装が済むまでこのスクリプトは失敗する。それが CLAUDE.md の言う
# 「Red の状態から始める」であって、異常ではない。
set -euo pipefail

cd "$(dirname "$0")/.."
readonly ROOT_DIR="$PWD"

# 上流スイートは仕様のタグに追随させる。追随先を固定しないと、スイート側の
# 変更で Red の理由が変わってしまい実装の進捗を測れなくなる
readonly CONFORMANCE_REF="${CONFORMANCE_REF:-v1.1.1}"

readonly WORK_DIR="${ROOT_DIR}/.conformance"
readonly REPORT_DIR="${OCI_REPORT_DIR:-${WORK_DIR}/report}"
readonly LISTEN="${CONFORMANCE_LISTEN:-127.0.0.1:15000}"
readonly ROOT_URL="http://${LISTEN}"

# 上流のパス要素を付けない参照であること自体が要求（REQ-0006）なので、
# 名前空間も上流を含まない形で与える。
#
# 既定の対象は単一プラットフォームのマニフェストを持つ OCI アーティファクト
# （Helm チャート）にしてある。コンテナイメージの公開タグはほぼ例外なく
# マルチアーキ（Image Index）であり、上流スイートの「タグ参照がマニフェスト
# 1件そのものの digest と一致する」という検証は、要求が無い限り
# Accept ヘッダでは絞り込まれない（実際に docker.io の library/hello-world
# で検証: Accept を単一マニフェストの型だけに絞っても Image Index が返る）。
# システムは要求されたマニフェストがマニフェストリストならプラットフォーム
# 選択を行わずそのまま返す（REQ-0034）ため、これはシステム側の不備ではなく、
# 検証対象の選び方の問題。単一プラットフォームの対象に変えることで解消する。
#
# 対象は docker.io 上の Helm チャート（OCI アーティファクト）にしてある。
# ghcr.io でも同種の対象（stefanprodan/charts/podinfo）を一度試したが、
# blob の実体が redirect 先の CDN（pkg-containers.githubusercontent.com）
# にあり、このスクリプトを動かすサンドボックス環境のプロキシでは当該ホストへ
# の接続が通らなかった（docker.io の CDN redirect 先である
# production.cloudfront.docker.com は通る）。入れ替えは実装の問題ではなく
# 実行環境のネットワーク事情によるもの。
readonly NAMESPACE="${CONFORMANCE_NAMESPACE:-bitnamicharts/nginx}"
readonly TAG="${CONFORMANCE_TAG:-25.2.14}"

for cmd in go curl jq; do
  command -v "$cmd" >/dev/null || { echo "必要なコマンドがありません: $cmd" >&2; exit 1; }
done

mkdir -p "$WORK_DIR" "$REPORT_DIR"

# --- スイートの取得と構築 --------------------------------------------------
readonly SPEC_DIR="${WORK_DIR}/distribution-spec"
if [ ! -d "$SPEC_DIR/.git" ]; then
  git clone --depth 1 --branch "$CONFORMANCE_REF" \
    https://github.com/opencontainers/distribution-spec.git "$SPEC_DIR"
fi
readonly SUITE="${WORK_DIR}/conformance.test"
if [ ! -x "$SUITE" ]; then
  ( cd "$SPEC_DIR/conformance" && go test -c -o "$SUITE" )
fi

# --- 被試験サーバの起動 ----------------------------------------------------
BIN="${OCI_CACHE_BIN:-}"
if [ -z "$BIN" ]; then
  cargo build --release --locked
  BIN="${ROOT_DIR}/target/release/oci-cache"
fi

readonly DATA_DIR="${WORK_DIR}/data"
rm -rf "$DATA_DIR"
mkdir -p "$DATA_DIR"

cat > "${WORK_DIR}/config.toml" <<TOML
[server]
listen = "${LISTEN}"

[storage]
path = "${DATA_DIR}"
TOML

"$BIN" --config "${WORK_DIR}/config.toml" > "${WORK_DIR}/server.log" 2>&1 &
readonly SERVER_PID=$!

# 被試験サーバを確実に止める。スイートが途中で落ちても待ち受けが残ると、
# 次の実行が前回のプロセスを相手に走ってしまうため
cleanup() { kill "$SERVER_PID" 2>/dev/null || true; }
trap cleanup EXIT

# 接続はできるが応答が返らない状態で無限に待たないよう、要求ごとと全体の双方に
# 上限を置く。CI のジョブ上限まで居座られると、Red の理由が分からなくなるため
ready=0
readonly READY_DEADLINE=$((SECONDS + 30))
while [ "$SECONDS" -lt "$READY_DEADLINE" ]; do
  if curl -fsS --connect-timeout 2 --max-time 5 -o /dev/null "${ROOT_URL}/v2/"; then
    ready=1
    break
  fi
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "oci-cache が起動しませんでした。実装が未着手であればこれが想定の Red です。" >&2
    sed 's/^/  | /' "${WORK_DIR}/server.log" >&2
    exit 1
  fi
  sleep 0.2
done

if [ "$ready" != 1 ]; then
  echo "${ROOT_URL}/v2/ が応答しません（REQ-0030）。" >&2
  sed 's/^/  | /' "${WORK_DIR}/server.log" >&2
  exit 1
fi

# --- 取得対象の用意 --------------------------------------------------------
# 上流スイートは既定では push でテスト対象を作る。oci-cache は書き込み系を
# 持たない（REQ-0031）ので、代わりに実際に1つ取得してキャッシュを温め、
# その結果のダイジェストを OCI_*_DIGEST として渡す。こうするとスイートは
# push による準備を省き、取得系だけを検証する。
readonly SINGLE_TYPES='application/vnd.oci.image.manifest.v1+json,application/vnd.docker.distribution.manifest.v2+json'
readonly INDEX_TYPES='application/vnd.oci.image.index.v1+json,application/vnd.docker.distribution.manifest.list.v2+json'

# マニフェストを1つ取得してファイルへ落とす。
#   $1: 参照（タグまたはダイジェスト）
#   $2: 保存先のパス
#
# `continue-on-error` を外した今、この取得が Docker Hub の匿名要求回数
# 制限（429）に当たると、無関係な変更の PR まで赤くなる。`set -euo
# pipefail` の下で失敗すると即座にスクリプトを止めるため、一時的な失敗を
# 再試行で吸収する（CodeRabbit レビュー指摘）。実装の不備はこの後も動く
# スイート本体の検証が引き続き検出する。
#
# 再試行は 429・5xx・通信断（接続不能・応答本文の転送失敗を含む）に限る。
# 存在しないタグ等の恒久的な 4xx まで5回再試行すると、最大50秒待って
# ようやく失敗するだけで無意味に遅くなる（CodeRabbit レビュー指摘）。
#
# 上流の認証拒否は本システムでは 502 に変換され、一時的な上流障害の 502
# と応答だけでは区別できない。このスクリプトが生成する設定ファイルには
# `[upstreams]` の認証情報を書かないが、`src/config.rs` の
# `env_credentials` は設定ファイルの内容と無関係に
# `OCI_CACHE_AUTH_DOCKER_IO_USERNAME` 等の環境変数を読むため、実行環境に
# それが設定されていれば認証拒否による 502 も起こりうる（CodeRabbit
# レビュー指摘: 前回の「起こり得ない」という説明は言い過ぎだった）。
# この区別は HTTP ステータスだけでは付けられないため、ここでは一時的な
# 上流障害として再試行する。
fetch_manifest() {
  local attempt status curl_rc
  for attempt in 1 2 3 4 5; do
    # `set -euo pipefail` の下でも `curl` 自体の失敗でスクリプトを止めず
    # 再試行させるため、`if` の条件式として実行する。応答本文の転送が
    # 途中で失敗した場合、ステータス行は既に受信済みで `-w` は 200 を
    # 表示しうるが、curl の終了コードは非 0 になる。終了コードも合わせて
    # 見ないと、不完全な本文を成功と誤認する（CodeRabbit レビュー指摘）。
    if status="$(curl -sS --connect-timeout 5 --max-time 120 \
      -H "Accept: ${SINGLE_TYPES},${INDEX_TYPES}" -o "$2" \
      -w '%{http_code}' "${ROOT_URL}/v2/${NAMESPACE}/manifests/$1")"; then
      curl_rc=0
    else
      curl_rc=$?
    fi
    if [ "$curl_rc" -eq 0 ] && [ "$status" = 200 ]; then
      return 0
    fi
    if [ "$curl_rc" -ne 0 ]; then
      : # 通信断・応答本文の転送失敗は再試行対象
    else
      case "$status" in
        429 | 5??) ;; # レート制限・上流の一時的障害（認証拒否の502含む）は再試行対象
        *)
          echo "対象の取得に失敗した（恒久的な HTTP ${status:-不明}）: ${NAMESPACE}:$1" >&2
          return 1
          ;;
      esac
    fi
    if [ "$attempt" -lt 5 ]; then
      echo "対象の取得に失敗した（試行 ${attempt}/5、HTTP ${status:-不明}・curl終了コード ${curl_rc}、$((attempt * 5))秒後に再試行）。Docker Hub の要求回数制限等、一時的な失敗の可能性がある。" >&2
      sleep $((attempt * 5))
    fi
  done
  echo "対象の取得に5回失敗した: ${NAMESPACE}:$1" >&2
  return 1
}

# ファイルの中身から sha256 のダイジェスト文字列を作る。
#   $1: 対象のファイル
# 受信したバイト列そのものから計算する。応答の Docker-Content-Digest
# ヘッダを使うと、検証したい相手（REQ-0032）の出力を前提に検証対象を
# 組み立てることになるため
digest_of() {
  echo "sha256:$(sha256sum "$1" | cut -d' ' -f1)"
}

readonly MANIFEST_FILE="${WORK_DIR}/manifest.json"
fetch_manifest "$TAG" "$MANIFEST_FILE"

# タグ参照（$TAG）は単一プラットフォームのマニフェストに直接解決できる対象
# でなければならない。マニフェストリスト/Image Index が返った場合、タグ
# 応答の Docker-Content-Digest は常にリスト自身の digest であり（REQ-0034:
# システムは要求されたマニフェストがマニフェストリストならプラットフォーム
# 選択を行わずそのまま返す）、子マニフェストへ降りて得た digest を
# OCI_MANIFEST_DIGEST として渡すと、タグ参照自体への応答と食い違う。
#
# 以前は子マニフェストへ降りて取り繕っていたが、それでも元の不一致は
# 解消しない（降りた先が「さらに」入れ子の Image Index だった場合しか
# 検出できていなかった）。`CONFORMANCE_NAMESPACE` / `CONFORMANCE_TAG` を
# マルチアーキのコンテナイメージへ上書きした場合に備え、降りずにここで
# 明確に停止する（CodeRabbit レビュー指摘: 1回目の対処が不十分だった）。
#
# OCI Image Index の `mediaType` は省略可能なので、それだけを見ると
# mediaType を省略した Image Index を単一マニフェストと誤認する
# （CodeRabbit レビュー指摘）。`manifests` 配列の有無でも判定する。
media_type="$(jq -r 'if (.manifests? | type) == "array" then "application/vnd.oci.image.index.v1+json" else (.mediaType // "") end' "$MANIFEST_FILE")"
case "$media_type" in
  *manifest.list.v2+json|*image.index.v1+json)
    echo "対象 ${NAMESPACE}:${TAG} はマニフェストリスト/Image Index です。タグ応答の digest と OCI_MANIFEST_DIGEST が食い違うため、CONFORMANCE_NAMESPACE / CONFORMANCE_TAG には単一プラットフォームのマニフェストを持つ対象を指定してください。" >&2
    exit 1
    ;;
esac

manifest_digest="$(digest_of "$MANIFEST_FILE")"
blob_digest="$(jq -r '.config.digest' "$MANIFEST_FILE")"

echo "対象: ${NAMESPACE}:${TAG}"
echo "  マニフェスト: ${manifest_digest}"
echo "  blob:         ${blob_digest}"

# --- 実行 ------------------------------------------------------------------
cd "$REPORT_DIR"
OCI_ROOT_URL="$ROOT_URL" \
OCI_NAMESPACE="$NAMESPACE" \
OCI_TAG_NAME="$TAG" \
OCI_MANIFEST_DIGEST="$manifest_digest" \
OCI_BLOB_DIGEST="$blob_digest" \
OCI_TEST_PULL=1 \
OCI_TEST_PUSH=0 \
OCI_TEST_CONTENT_DISCOVERY=0 \
OCI_TEST_CONTENT_MANAGEMENT=0 \
OCI_HIDE_SKIPPED_WORKFLOWS=1 \
OCI_REPORT_DIR="$REPORT_DIR" \
  "$SUITE"
