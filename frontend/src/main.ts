import { invoke } from "@tauri-apps/api/core";
import "./style.css";
import {
  assertCompatibleContract,
  type BackgroundStatus,
  type ContractInfo,
  type ExclusionConfig,
  type IndexResponse,
  type ScannerMode,
  type SearchHit,
  type SearchRequest,
  type SearchCoreBackendStatus,
  type Settings,
  type SnippetBatchResult,
  type SnippetRequest,
} from "./app_contract_v1";
import {
  canLoadMore,
  contentQueryDebounceMs,
  highlightText,
  loadMoreCount,
  nextSearchGeneration,
  RESULT_WINDOW_SIZE,
  shouldApplySearchResult,
  visibleResultWindow,
} from "./search_state";
import {
  backgroundStatusSummary,
  rebuildPhaseLabel,
  rebuildProgressMetrics,
} from "./background_state";

const app = document.querySelector<HTMLDivElement>("#app");
if (!app) throw new Error("app root is missing");

app.innerHTML = `
  <header class="topbar"><h1>PersonalRag</h1><span id="mode">キーワード検索</span></header>
  <main>
    <section class="search-panel">
      <label>ファイル名 / パス<input id="file-query" autocomplete="off" /></label>
      <label>ファイル内文字列<input id="content-query" autocomplete="off" /></label>
      <div class="options">
        <label><input id="include-path" type="checkbox" /> パスを含めて検索</label>
        <label><input id="match-case" type="checkbox" /> Match Case</label>
        <label><input id="whole-words" type="checkbox" /> Whole Words</label>
        <label><input id="regex" type="checkbox" /> Regex</label>
        <label>拡張子<input id="extensions" placeholder="txt,md" /></label>
        <label>Scope<input id="scope" placeholder="C:\\work" /></label>
        <label>Sort<select id="sort-field"><option value="name">名前</option><option value="path" selected>パス</option><option value="size">サイズ</option><option value="modified">更新日時</option><option value="extension">種類</option></select></label>
        <label><input id="descending" type="checkbox" /> 降順</label>
        <label>検索backend<select id="search-backend"><option value="v2" selected>Search v2（ready時）</option><option value="v1">Search v1</option></select></label>
      </div>
    </section>
    <section class="tabs"><button id="simple-tab" class="active">シンプル</button><button id="hit-tab">ヒット表示</button><span id="result-count">0件</span><span id="search-status" aria-live="polite">待機中</span></section>
    <section id="error" class="error" hidden></section>
    <section id="results" class="results"></section>
    <section id="hits" class="hits" hidden></section>
    <section class="index-panel">
      <h2>Index設定</h2>
      <label>対象root<input id="index-root" placeholder="C:\\work" /></label>
      <label>最大bytes<input id="max-bytes" type="number" value="33554432" min="1" /></label>
      <div class="index-options">
        <label>Scanner<select id="scanner-mode"><option value="auto" selected>Auto</option><option value="walk_dir">WalkDir</option><option value="windows_native">Windows Native</option></select></label>
        <label>Search Core<select id="search-core-backend"><option value="perf12" selected>Perf12（安定）</option><option value="shadow">Shadow比較</option><option value="vnext">vNext（候補）</option></select></label>
        <span id="search-core-status">Perf12</span>
        <span>除外:</span>
        <label><input id="exclude-dev-caches" type="checkbox" /> cache</label>
        <label><input id="exclude-virtual-envs" type="checkbox" /> venv</label>
        <label><input id="exclude-node-modules" type="checkbox" /> node_modules</label>
        <label><input id="exclude-build-artifacts" type="checkbox" /> build</label>
        <label><input id="exclude-vcs" type="checkbox" /> VCS</label>
        <label><input id="respect-gitignore" type="checkbox" /> .gitignore</label>
        <label class="custom-excludes">カスタムglob<input id="custom-globs" placeholder="*.tmp,private/**" /></label>
      </div>
      <button id="reindex">再index</button><button id="reindex-cancel" disabled>キャンセル</button>
      <span id="index-status">未実行</span>
      <div id="index-progress-panel" class="index-progress" hidden aria-live="polite">
        <div class="index-progress-heading"><strong>インデックス作成の進捗</strong><span id="index-progress-percent">0%</span></div>
        <progress id="index-progress-bar" max="100" value="0">0%</progress>
        <div class="index-progress-grid">
          <div><span>経過時間</span><strong id="index-elapsed">—</strong></div>
          <div><span>残り時間</span><strong id="index-remaining-time">—</strong></div>
          <div><span>予想完了時間</span><strong id="index-eta">—</strong></div>
          <div><span>処理ファイル数</span><strong id="index-processed-files">0</strong></div>
          <div><span>残ファイル数</span><strong id="index-remaining-files">—</strong></div>
          <div><span>総ファイル数</span><strong id="index-total-files">—</strong></div>
          <div><span>フェーズ</span><strong id="index-phase">scanning</strong></div>
          <div><span>処理速度</span><strong id="index-files-rate">—</strong></div>
          <div><span>読込速度</span><strong id="index-mib-rate">—</strong></div>
          <div><span>キュー</span><strong id="index-queue-files">0</strong></div>
          <div><span>準備済みメモリ</span><strong id="index-prepared-bytes">0 B</strong></div>
          <div><span>除外</span><strong id="index-pruned-files">0</strong></div>
        </div>
      </div>
    </section>
    <section class="background-panel">
      <h2>Background indexing</h2>
      <p id="background-status">Background indexing: OFF</p>
      <div class="background-actions">
        <button id="background-enable">有効化</button>
        <button id="background-disable">無効化</button>
        <button id="background-sync">今すぐ同期</button>
        <button id="background-rebuild">再構築</button>
      </div>
    </section>
  </main>
`;

const $ = <T extends HTMLElement>(id: string): T => {
  const element = document.getElementById(id);
  if (!element) throw new Error(`missing element: ${id}`);
  return element as T;
};

const fileQuery = $("file-query") as HTMLInputElement;
const contentQuery = $("content-query") as HTMLInputElement;
const simpleTab = $("simple-tab") as HTMLButtonElement;
const hitTab = $("hit-tab") as HTMLButtonElement;
const results = $("results");
const hits = $("hits");
const error = $("error");
const searchStatus = $("search-status");
let visibleHits: SearchHit[] = [];
let visibleCount = RESULT_WINDOW_SIZE;
let generation = 0;
let debounceTimer: number | undefined;
let rebuildPollTimer: number | undefined;

function requestFromUi(): SearchRequest {
  const extensions = ($("extensions") as HTMLInputElement).value
    .split(",")
    .map((value) => value.trim())
    .filter(Boolean);
  return {
    file_query: fileQuery.value.trim() || null,
    include_path: ($( "include-path") as HTMLInputElement).checked,
    content_query: contentQuery.value.trim() || null,
    extensions,
    path_scope: ($( "scope") as HTMLInputElement).value.trim() || null,
    match_case: ($( "match-case") as HTMLInputElement).checked,
    whole_words: ($( "whole-words") as HTMLInputElement).checked,
    regex: ($( "regex") as HTMLInputElement).checked,
    sort_field: ($( "sort-field") as HTMLSelectElement).value,
    sort_direction: ($( "descending") as HTMLInputElement).checked ? "descending" : "ascending",
    // Keep the backend result cap high enough for incremental display, while
    // the DOM renderer below materializes only RESULT_WINDOW_SIZE rows.
    limit: 2_000,
  };
}

function escapeHtml(value: string): string {
  return value.replace(/[&<>"']/g, (character) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[character] ?? character);
}

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MiB`;
}

function renderResults(items: SearchHit[]): void {
  $("result-count").textContent = `${items.length}件`;
  if (items.length === 0) {
    results.innerHTML = `<p class="empty">結果はありません</p>`;
    return;
  }
  const windowItems = visibleResultWindow(items, visibleCount);
  const loadMore = canLoadMore(items.length, visibleCount)
    ? `<button id="load-more-results" class="load-more">さらに表示（${Math.min(RESULT_WINDOW_SIZE, items.length - visibleCount)}件）</button>`
    : "";
  results.innerHTML = `<table><thead><tr><th>名前</th><th>パス</th><th>サイズ</th><th>更新日時</th><th>種類</th><th></th></tr></thead><tbody>${windowItems.map((item) => `
    <tr><td>${escapeHtml(item.name)}</td><td title="${escapeHtml(item.path)}">${escapeHtml(item.path)}</td><td>${formatSize(item.size_bytes)}</td><td>${new Date(item.modified_ns / 1_000_000).toLocaleString()}</td><td>${escapeHtml(item.extension)}</td><td class="actions"><button data-open="${escapeHtml(item.path)}">開く</button><button data-parent="${escapeHtml(item.path)}">親</button><button data-copy="${escapeHtml(item.path)}">コピー</button></td></tr>`).join("")}</tbody></table>`;
  results.innerHTML += loadMore;
  results.querySelector<HTMLButtonElement>("#load-more-results")?.addEventListener("click", () => {
    visibleCount = loadMoreCount(visibleCount);
    renderResults(items);
  });
  results.querySelectorAll<HTMLButtonElement>("button[data-open]").forEach((button) => button.addEventListener("click", () => void invoke("open_file", { path: button.dataset.open })));
  results.querySelectorAll<HTMLButtonElement>("button[data-parent]").forEach((button) => button.addEventListener("click", () => void invoke("open_parent", { path: button.dataset.parent })));
  results.querySelectorAll<HTMLButtonElement>("button[data-copy]").forEach((button) => button.addEventListener("click", () => void navigator.clipboard.writeText(button.dataset.copy ?? "")));
}

async function renderHitTab(requestGeneration: number): Promise<void> {
  if (!contentQuery.value.trim()) {
    hits.innerHTML = `<p class="empty">ファイル内文字列を入力するとヒット周辺を表示できます</p>`;
    return;
  }
  const regexMode = ($( "regex") as HTMLInputElement).checked;
  const matchCase = ($( "match-case") as HTMLInputElement).checked;
  const wholeWords = ($( "whole-words") as HTMLInputElement).checked;
  const windowItems = visibleResultWindow(visibleHits, visibleCount).slice(0, 100);
  const requests: SnippetRequest[] = windowItems.map((item) => ({
    path: item.path,
    query: contentQuery.value,
    context: 1,
    maxHits: 10,
    matchCase,
    wholeWords,
    regex: regexMode,
  }));
  const batches = await invoke<SnippetBatchResult[]>("snippets_batch", { request: { items: requests } });
  if (!shouldApplySearchResult(generation, requestGeneration)) return;
  const itemsByPath = new Map(windowItems.map((item) => [item.path, item]));
  hits.innerHTML = batches.flatMap(({ path, hits: snippetHits }) => {
    const item = itemsByPath.get(path);
    if (!item) return [];
    return snippetHits.map((snippet) => `<article><h3>${escapeHtml(item.name)}:${snippet.line_number}</h3><pre>${escapeHtml(snippet.before.join("\n"))}${snippet.before.length ? "\n" : ""}${highlightText(snippet.hit_line, contentQuery.value, regexMode, matchCase)}${snippet.after.length ? "\n" : ""}${escapeHtml(snippet.after.join("\n"))}</pre></article>`);
  }).join("") || `<p class="empty">ヒット周辺はありません</p>`;
}

function setSearchStatus(value: string): void {
  searchStatus.textContent = value;
}

async function runSearch(requestGeneration?: number): Promise<void> {
  const activeGeneration = requestGeneration ?? (generation = nextSearchGeneration(generation));
  const request = requestFromUi();
  error.hidden = true;
  setSearchStatus("検索中");
  try {
    const items = await invoke<SearchHit[]>("search", { request });
    if (!shouldApplySearchResult(generation, activeGeneration)) return;
    visibleHits = items;
    visibleCount = RESULT_WINDOW_SIZE;
    renderResults(items);
    setSearchStatus(`結果 ${items.length}件`);
    void refreshSearchCoreStatus();
    if (!hits.hidden) await renderHitTab(activeGeneration);
  } catch (cause) {
    if (!shouldApplySearchResult(generation, activeGeneration)) return;
    error.textContent = String(cause);
    error.hidden = false;
    visibleHits = [];
    renderResults([]);
    setSearchStatus("エラー");
  }
}

function scheduleSearch(): void {
  generation = nextSearchGeneration(generation);
  const requestGeneration = generation;
  void invoke("cancel_search").catch(() => undefined);
  if (debounceTimer !== undefined) window.clearTimeout(debounceTimer);
  setSearchStatus("入力待ち");
  debounceTimer = window.setTimeout(
    () => void runSearch(requestGeneration),
    contentQueryDebounceMs(contentQuery.value),
  );
}

function currentRoot(): string | null {
  return ($( "index-root") as HTMLInputElement).value.trim() || null;
}

function csvValues(value: string): string[] {
  return value.split(",").map((item) => item.trim()).filter(Boolean);
}

function exclusionsFromUi(): ExclusionConfig {
  return {
    dev_caches: ($("exclude-dev-caches") as HTMLInputElement).checked,
    virtual_envs: ($("exclude-virtual-envs") as HTMLInputElement).checked,
    node_modules: ($("exclude-node-modules") as HTMLInputElement).checked,
    build_artifacts: ($("exclude-build-artifacts") as HTMLInputElement).checked,
    vcs: ($("exclude-vcs") as HTMLInputElement).checked,
    use_gitignore: ($("respect-gitignore") as HTMLInputElement).checked,
    custom_directory_names: [],
    custom_relative_paths: [],
    custom_globs: csvValues(($('custom-globs') as HTMLInputElement).value),
  };
}

function scannerModeFromUi(): ScannerMode {
  return ($("scanner-mode") as HTMLSelectElement).value as ScannerMode;
}

function indexConfigurationFromUi(): { scannerMode: ScannerMode; exclusions: ExclusionConfig } {
  return { scannerMode: scannerModeFromUi(), exclusions: exclusionsFromUi() };
}

function applySettingsToUi(settings: Settings): void {
  if (settings.roots[0]) ($( "index-root") as HTMLInputElement).value = settings.roots[0];
  ($( "max-bytes") as HTMLInputElement).value = String(settings.max_bytes || 32 * 1024 * 1024);
  ($( "search-backend") as HTMLSelectElement).value = settings.search_backend ?? "v2";
  ($( "search-core-backend") as HTMLSelectElement).value = settings.search_core_backend ?? "perf12";
  ($( "scanner-mode") as HTMLSelectElement).value = settings.scanner_mode ?? "auto";
  const exclusions = settings.exclusions;
  if (!exclusions) return;
  ($("exclude-dev-caches") as HTMLInputElement).checked = exclusions.dev_caches;
  ($("exclude-virtual-envs") as HTMLInputElement).checked = exclusions.virtual_envs;
  ($("exclude-node-modules") as HTMLInputElement).checked = exclusions.node_modules;
  ($("exclude-build-artifacts") as HTMLInputElement).checked = exclusions.build_artifacts;
  ($("exclude-vcs") as HTMLInputElement).checked = exclusions.vcs;
  ($("respect-gitignore") as HTMLInputElement).checked = exclusions.use_gitignore;
  ($("custom-globs") as HTMLInputElement).value = exclusions.custom_globs.join(", ");
}

function renderSearchCoreStatus(status: SearchCoreBackendStatus): void {
  const ready = status.vnext_ready ? "ready" : "not-ready";
  const mismatch = status.shadow_mismatches > 0 ? ` mismatch=${status.shadow_mismatches}` : "";
  const shadow = status.requested === "shadow"
    ? ` / shadow=${status.shadow_comparisons}/${status.shadow_queued} coalesce=${status.shadow_coalesced} drop=${status.shadow_dropped} fail=${status.shadow_failures}`
    : "";
  const commonAvg = status.common_result_searches > 0
    ? Math.round(status.common_result_total_micros / status.common_result_searches)
    : 0;
  const common = status.common_result_searches > 0
    ? ` / common=${status.common_result_searches} avg=${commonAvg}µs max=${status.common_result_max_micros}µs`
    : "";
  $("search-core-status").textContent =
    `${status.requested} → ${status.active} / vNext ${ready} / fallback=${status.fallbacks}${mismatch}${shadow}${common}`;
}

async function refreshSearchCoreStatus(): Promise<void> {
  try {
    renderSearchCoreStatus(await invoke<SearchCoreBackendStatus>("search_core_backend_status"));
  } catch {
    $("search-core-status").textContent = "status unavailable";
  }
}

function renderBackgroundStatus(status: BackgroundStatus): void {
  $("background-status").textContent = backgroundStatusSummary(status);
  renderRebuildStatus(status);
}

function formatDuration(milliseconds: number | null): string {
  if (milliseconds === null || !Number.isFinite(milliseconds)) return "計算中";
  const totalSeconds = Math.max(0, Math.floor(milliseconds / 1_000));
  const hours = Math.floor(totalSeconds / 3_600);
  const minutes = Math.floor((totalSeconds % 3_600) / 60);
  const seconds = totalSeconds % 60;
  if (hours > 0) return `${hours}時間 ${minutes}分 ${seconds}秒`;
  if (minutes > 0) return `${minutes}分 ${seconds}秒`;
  return `${seconds}秒`;
}

function formatCompletionTime(timestamp: number | null): string {
  if (timestamp === null || !Number.isFinite(timestamp)) return "計算中";
  return new Date(timestamp).toLocaleString("ja-JP", {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}

function formatBytes(value: number): string {
  if (!Number.isFinite(value) || value <= 0) return "0 B";
  if (value < 1024) return `${Math.round(value)} B`;
  if (value < 1024 * 1024) return `${(value / 1024).toFixed(1)} KiB`;
  return `${(value / 1024 / 1024).toFixed(1)} MiB`;
}

function renderRebuildStatus(status: BackgroundStatus): void {
  const job = status.rebuild;
  const reindex = $("reindex") as HTMLButtonElement;
  const cancel = $("reindex-cancel") as HTMLButtonElement;
  const statusElement = $("index-status");
  const progressPanel = $("index-progress-panel");
  if (!job) {
    reindex.disabled = false;
    cancel.disabled = true;
    progressPanel.hidden = true;
    return;
  }
  const active = ["starting", "scanning", "reconciling", "catching_up", "cancelling"].includes(job.state);
  reindex.disabled = active;
  cancel.disabled = !active;
  const progress = job.progress;
  const metrics = rebuildProgressMetrics(job);
  const progressBar = $("index-progress-bar") as HTMLProgressElement;
  const percentElement = $("index-progress-percent");
  const scanning = progress.phase === "scanning";
  const totalKnown = !scanning && metrics.totalFiles > 0;
  const percentText = totalKnown || job.state === "completed"
    ? `${metrics.percent.toFixed(1)}%`
    : "走査中";
  progressPanel.hidden = false;
  if (scanning) {
    progressBar.removeAttribute("value");
  } else {
    progressBar.value = metrics.percent;
  }
  progressBar.setAttribute("aria-valuetext", percentText);
  percentElement.textContent = percentText;
  $("index-elapsed").textContent = formatDuration(metrics.elapsedMs);
  $("index-remaining-time").textContent = metrics.remainingMs === 0
    ? job.state === "completed" ? "完了" : "0秒"
    : formatDuration(metrics.remainingMs);
  $("index-eta").textContent = formatCompletionTime(metrics.estimatedCompletionAt);
  $("index-processed-files").textContent = scanning
    ? (progress.discovered_files ?? 0).toLocaleString("ja-JP")
    : metrics.processedFiles.toLocaleString("ja-JP");
  $("index-remaining-files").textContent = totalKnown
    ? metrics.remainingFiles.toLocaleString("ja-JP")
    : "集計中";
  $("index-total-files").textContent = scanning
    ? `候補 ${(progress.total_files ?? 0).toLocaleString("ja-JP")} / 走査 ${(progress.discovered_files ?? 0).toLocaleString("ja-JP")}`
    : totalKnown
      ? metrics.totalFiles.toLocaleString("ja-JP")
      : "集計中";
  $("index-phase").textContent = rebuildPhaseLabel(job);
  $("index-files-rate").textContent = Number.isFinite(progress.files_per_second ?? NaN)
    ? `${(progress.files_per_second ?? 0).toFixed(1)} files/s`
    : "—";
  $("index-mib-rate").textContent = Number.isFinite(progress.mib_per_second ?? NaN)
    ? `${(progress.mib_per_second ?? 0).toFixed(2)} MiB/s`
    : "—";
  $("index-queue-files").textContent = String(progress.queue_files ?? 0);
  $("index-prepared-bytes").textContent = formatBytes(progress.prepared_bytes ?? 0);
  $("index-pruned-files").textContent = (progress.pruned_files ?? 0).toLocaleString("ja-JP");
  const path = progress.current_path ? ` | ${progress.current_path}` : "";
  const statusText = `再構築: ${job.state} indexed=${progress.indexed_files} unchanged=${progress.unchanged_files} errors=${progress.error_files}${path}${job.error ? ` | ${job.error}` : ""}`;
  statusElement.textContent = statusText;
  statusElement.title = statusText;
}

async function refreshBackgroundStatus(): Promise<void> {
  try {
    renderBackgroundStatus(await invoke<BackgroundStatus>("background_status"));
  } catch (cause) {
    $("background-status").textContent = `Background status error: ${String(cause)}`;
  }
}

async function pollRebuildStatus(): Promise<void> {
  try {
    const status = await invoke<BackgroundStatus>("background_status");
    renderBackgroundStatus(status);
    const state = status.rebuild?.state;
    const active = state !== undefined && ["starting", "scanning", "reconciling", "catching_up", "cancelling"].includes(state);
    if (active) {
      rebuildPollTimer = window.setTimeout(() => void pollRebuildStatus(), 350);
    } else {
      rebuildPollTimer = undefined;
      if (state === "completed") await runSearch();
    }
  } catch (cause) {
    $("index-status").textContent = String(cause);
    rebuildPollTimer = window.setTimeout(() => void pollRebuildStatus(), 700);
  }
}

function startRebuildPolling(): void {
  if (rebuildPollTimer !== undefined) window.clearTimeout(rebuildPollTimer);
  rebuildPollTimer = window.setTimeout(() => void pollRebuildStatus(), 50);
}

[fileQuery, contentQuery, $("include-path"), $("match-case"), $("whole-words"), $("regex"), $("extensions"), $("scope"), $("sort-field"), $("descending")].forEach((element) => element.addEventListener("input", scheduleSearch));
$("search-backend").addEventListener("change", () => {
  void invoke("set_search_backend", { backend: ($("search-backend") as HTMLSelectElement).value })
    .then(() => void runSearch())
    .catch((cause) => { error.textContent = String(cause); error.hidden = false; });
});
$("search-core-backend").addEventListener("change", () => {
  void invoke<SearchCoreBackendStatus>("set_search_core_backend", {
    backend: ($("search-core-backend") as HTMLSelectElement).value,
  })
    .then((status) => {
      renderSearchCoreStatus(status);
      void runSearch();
    })
    .catch((cause) => { error.textContent = String(cause); error.hidden = false; });
});
simpleTab.addEventListener("click", () => { simpleTab.classList.add("active"); hitTab.classList.remove("active"); results.hidden = false; hits.hidden = true; });
hitTab.addEventListener("click", () => { hitTab.classList.add("active"); simpleTab.classList.remove("active"); results.hidden = true; hits.hidden = false; void renderHitTab(generation); });
$("reindex").addEventListener("click", async () => {
  const root = ($( "index-root") as HTMLInputElement).value.trim();
  const status = $("index-status");
  if (!root) { status.textContent = "rootを指定してください"; return; }
  const reindex = $("reindex") as HTMLButtonElement;
  reindex.disabled = true;
  status.textContent = "再構築要求を送信中...";
  try {
    const response = await invoke<IndexResponse>("index", {
      request: {
        roots: [root],
        maxBytes: Number(( $("max-bytes") as HTMLInputElement).value),
        ...indexConfigurationFromUi(),
      },
    });
    renderRebuildStatus(response.status);
    status.textContent = response.message;
    startRebuildPolling();
  } catch (cause) {
    reindex.disabled = false;
    status.textContent = String(cause);
  }
});

$("reindex-cancel").addEventListener("click", async () => {
  try {
    renderRebuildStatus(await invoke<BackgroundStatus>("background_cancel"));
    startRebuildPolling();
  } catch (cause) {
    $("index-status").textContent = String(cause);
  }
});

$("background-enable").addEventListener("click", async () => {
  const root = currentRoot();
  if (!root) {
    $("background-status").textContent = "rootを指定してください";
    return;
  }
  try {
    const status = await invoke<BackgroundStatus>("background_enable", {
      request: {
        roots: [root],
        maxBytes: Number(($('max-bytes') as HTMLInputElement).value),
        ...indexConfigurationFromUi(),
      },
    });
    renderBackgroundStatus(status);
  } catch (cause) {
    $("background-status").textContent = String(cause);
  }
});

$("background-disable").addEventListener("click", async () => {
  try {
    renderBackgroundStatus(await invoke<BackgroundStatus>("background_disable"));
  } catch (cause) {
    $("background-status").textContent = String(cause);
  }
});

$("background-sync").addEventListener("click", async () => {
  try {
    renderBackgroundStatus(await invoke<BackgroundStatus>("background_sync_now"));
    startRebuildPolling();
  } catch (cause) {
    $("background-status").textContent = String(cause);
  }
});

$("background-rebuild").addEventListener("click", async () => {
  try {
    renderBackgroundStatus(await invoke<BackgroundStatus>("background_rebuild"));
    startRebuildPolling();
  } catch (cause) {
    $("background-status").textContent = String(cause);
  }
});

async function bootstrap(): Promise<void> {
  const contract = await invoke<ContractInfo>("contract_info");
  assertCompatibleContract(contract);
  const settings = await invoke<Settings>("load_settings");
  applySettingsToUi(settings);
  await refreshSearchCoreStatus();
  await refreshBackgroundStatus();
  window.setInterval(() => void refreshBackgroundStatus(), 1_000);
}

void bootstrap().catch((cause) => {
  error.textContent = String(cause);
  error.hidden = false;
  setSearchStatus("契約エラー");
});
