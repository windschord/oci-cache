// oci-cache の Web UI。依存を持たない（単一実行ファイルへ埋め込むため
// / REQ-0043）。/api/catalog・/api/stats を定期的に取得して表示し、
// 強制再取得（REQ-0017）を指示する。抑止時間中は操作を実行できない
// 状態として表示する（REQ-0045 / REQ-0047）。

const POLL_INTERVAL_MS = 5000;

// 直前の強制再取得が失敗したエントリのキー集合。`refreshCatalog` は
// ポーリングのたびに行を丸ごと再構築するため、ボタンの文字列だけに
// 失敗を記録すると次の定期更新で即座に消えてしまう（CodeRabbit
// レビュー指摘）。再構築後も `renderRow` がここを見て表示を復元する。
const failedRefreshes = new Set();

function entryKey(entry) {
  return `${entry.upstream}\u0000${entry.repository}\u0000${entry.tag}`;
}

function formatBytes(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KiB", "MiB", "GiB", "TiB"];
  let value = bytes / 1024;
  let unitIndex = 0;
  while (value >= 1024 && unitIndex < units.length - 1) {
    value /= 1024;
    unitIndex += 1;
  }
  return `${value.toFixed(1)} ${units[unitIndex]}`;
}

function shortDigest(digest) {
  const [algorithm, hex] = digest.split(":");
  if (!hex) return digest;
  return `${algorithm}:${hex.slice(0, 12)}`;
}

async function refreshStats() {
  const response = await fetch("/api/stats");
  if (!response.ok) return;
  const stats = await response.json();
  for (const [field, value] of Object.entries(stats)) {
    const cell = document.querySelector(`[data-field="${field}"]`);
    if (!cell) continue;
    cell.textContent = field === "storage_bytes_used" ? formatBytes(value) : value;
  }
}

async function requestRefresh(entry, button) {
  const key = entryKey(entry);
  failedRefreshes.delete(key);
  button.disabled = true;
  button.textContent = "再取得中...";
  try {
    const response = await fetch("/api/refresh", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        upstream: entry.upstream,
        repository: entry.repository,
        tag: entry.tag,
      }),
    });
    if (!response.ok && response.status !== 429) {
      failedRefreshes.add(key);
      button.textContent = "失敗しました";
    }
  } catch {
    failedRefreshes.add(key);
    button.textContent = "失敗しました";
  } finally {
    // カタログの再取得自体が失敗しても、上で記録した失敗表示は
    // `failedRefreshes` 経由で次回の再構築まで残る。
    try {
      await refreshCatalog();
    } catch {
      // 無視: 次のポーリングで再試行される。
    }
  }
}

function renderRow(entry) {
  const row = document.createElement("tr");

  const cells = [entry.repository, entry.tag, entry.upstream, shortDigest(entry.digest)];
  for (const text of cells) {
    const cell = document.createElement("td");
    cell.textContent = text;
    row.appendChild(cell);
  }

  const actionCell = document.createElement("td");
  const button = document.createElement("button");
  const remaining = entry.refresh_cooldown_remaining_seconds;
  // REQ-0047: 抑止時間が経過していない間は操作を実行できない状態にする。
  if (remaining > 0) {
    button.disabled = true;
    button.textContent = `再取得（あと${remaining}秒）`;
  } else {
    button.disabled = false;
    button.textContent = failedRefreshes.has(entryKey(entry)) ? "失敗しました" : "再取得";
    button.addEventListener("click", () => requestRefresh(entry, button));
  }
  actionCell.appendChild(button);
  row.appendChild(actionCell);

  return row;
}

async function refreshCatalog() {
  const response = await fetch("/api/catalog");
  if (!response.ok) return;
  const entries = await response.json();

  const table = document.querySelector('[data-role="catalog-table"]');
  const empty = document.querySelector('[data-role="catalog-empty"]');
  const rows = document.querySelector('[data-role="catalog-rows"]');

  if (entries.length === 0) {
    table.hidden = true;
    empty.hidden = false;
    return;
  }

  table.hidden = false;
  empty.hidden = true;
  rows.replaceChildren(...entries.map(renderRow));
}

async function tick() {
  await Promise.all([refreshCatalog(), refreshStats()]);
}

tick();
setInterval(tick, POLL_INTERVAL_MS);
