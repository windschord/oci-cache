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
# 名前空間も上流を含まない形で与える
readonly NAMESPACE="${CONFORMANCE_NAMESPACE:-library/hello-world}"
readonly TAG="${CONFORMANCE_TAG:-latest}"

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
cleanup() { kill "$SERVER_PID" 2>/dev/null || true; }
trap cleanup EXIT

for _ in $(seq 1 50); do
  if curl -fsS -o /dev/null "${ROOT_URL}/v2/"; then
    break
  fi
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    echo "oci-cache が起動しませんでした。実装が未着手であればこれが想定の Red です。" >&2
    sed 's/^/  | /' "${WORK_DIR}/server.log" >&2
    exit 1
  fi
  sleep 0.2
done

if ! curl -fsS -o /dev/null "${ROOT_URL}/v2/"; then
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

fetch_manifest() { # $1: 参照（タグまたはダイジェスト）, $2: 保存先
  curl -fsS -H "Accept: ${SINGLE_TYPES},${INDEX_TYPES}" -o "$2" \
    "${ROOT_URL}/v2/${NAMESPACE}/manifests/$1"
}

# ダイジェストは受信したバイト列そのものから計算する。応答の
# Docker-Content-Digest ヘッダを使うと、検証したい相手（REQ-0032）の
# 出力を前提に検証対象を組み立てることになるため
digest_of() { echo "sha256:$(sha256sum "$1" | cut -d' ' -f1)"; }

readonly MANIFEST_FILE="${WORK_DIR}/manifest.json"
fetch_manifest "$TAG" "$MANIFEST_FILE"
manifest_digest="$(digest_of "$MANIFEST_FILE")"

# マニフェストリストが返った場合は blob を持つ子マニフェストまで降りる。
# ここで降りるのは検証用の blob ダイジェストを得るためだけで、
# リストをそのまま返すこと自体は REQ-0034 でスイートが確認する。
child_file="$MANIFEST_FILE"
media_type="$(jq -r '.mediaType // ""' "$MANIFEST_FILE")"
case "$media_type" in
  *manifest.list.v2+json|*image.index.v1+json)
    child_file="${WORK_DIR}/child-manifest.json"
    fetch_manifest "$(jq -r '.manifests[0].digest' "$MANIFEST_FILE")" "$child_file"
    ;;
esac
blob_digest="$(jq -r '.config.digest' "$child_file")"

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
