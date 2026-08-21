import type { BackgroundStatus, RebuildState, RebuildStatus } from "./app_contract_v1";
export type { BackgroundStatus, RebuildState, RebuildStatus } from "./app_contract_v1";

export type RebuildProgressMetrics = {
  percent: number;
  elapsedMs: number;
  remainingFiles: number;
  remainingMs: number | null;
  estimatedCompletionAt: number | null;
  totalFiles: number;
  processedFiles: number;
};

const ACTIVE_REBUILD_STATES: RebuildState[] = [
  "starting",
  "scanning",
  "reconciling",
  "catching_up",
  "cancelling",
];

function epochMillis(value: string | null | undefined): number | null {
  if (!value) return null;
  const parsed = Number(value);
  return Number.isFinite(parsed) && parsed >= 0 ? parsed : null;
}

/**
 * Calculate display-only progress values from the persisted rebuild snapshot.
 * The backend reports the file counters and elapsed time; the frontend adds
 * the live clock and ETA without changing the indexer's single-writer state.
 */
export function rebuildProgressMetrics(
  rebuild: RebuildStatus,
  now = Date.now(),
): RebuildProgressMetrics {
  const progress = rebuild.progress;
  const totalFiles = Math.max(0, progress.total_files ?? 0);
  const processedFiles = Math.max(0, progress.processed_files);
  const countedProcessed = Math.min(processedFiles, totalFiles);
  const remainingFiles = Math.max(0, totalFiles - processedFiles);
  const startedAt = epochMillis(rebuild.started_at);
  const finishedAt = epochMillis(rebuild.finished_at);
  const active = ACTIVE_REBUILD_STATES.includes(rebuild.state);
  let elapsedMs = Math.max(0, progress.elapsed_ms);
  if (active && startedAt !== null) {
    elapsedMs = Math.max(elapsedMs, now - startedAt);
  } else if (!active && finishedAt !== null && startedAt !== null) {
    elapsedMs = Math.max(elapsedMs, finishedAt - startedAt);
  }

  let remainingMs: number | null = null;
  if (rebuild.state === "completed") {
    remainingMs = 0;
  } else if (active && Number.isFinite(progress.eta_ms ?? NaN)) {
    remainingMs = Math.max(0, progress.eta_ms ?? 0);
  }

  const rawPercent = totalFiles > 0
    ? Math.min(100, (countedProcessed / totalFiles) * 100)
    : rebuild.state === "completed" ? 100 : 0;
  // Reaching the end of file hydration does not mean the rebuild is finished: sidecars,
  // verification, and publication can still be running. Keep active jobs below 100% so the
  // progress bar never claims completion before the backend does.
  const percent = active && rawPercent >= 100 ? 99 : rawPercent;
  const backendCompletionAt = Number.isFinite(progress.estimated_completion_at_ms ?? NaN)
    ? Math.max(0, progress.estimated_completion_at_ms ?? 0)
    : null;
  const estimatedCompletionAt = remainingMs === null
    ? null
    : backendCompletionAt ?? (active ? now : finishedAt ?? now) + remainingMs;

  return {
    percent,
    elapsedMs,
    remainingFiles,
    remainingMs,
    estimatedCompletionAt,
    totalFiles,
    processedFiles,
  };
}


export function rebuildPhaseLabel(rebuild: RebuildStatus): string {
  if (["completed", "cancelled", "failed"].includes(rebuild.state)) return rebuild.state;
  return rebuild.progress.phase ?? rebuild.state;
}

export function backgroundStatusLabel(status: BackgroundStatus): string {
  if (status.last_error) return `Degraded: ${status.last_error}`;
  switch (status.sync_state) {
    case "starting": return "Starting";
    case "catching_up": return "Catching up";
    case "up_to_date": return "Up to date";
    case "rebuilding": return "Rebuilding index";
    case "degraded": return "Degraded";
    case "error": return "Error";
    default: return "Stopped";
  }
}

export function backgroundStatusSummary(status: BackgroundStatus): string {
  const rebuild = status.rebuild;
  const suffix = rebuild
    ? ` | Rebuild: ${rebuild.state} ${rebuild.progress.processed_files} files`
    : "";
  return `Background indexing: ${status.running ? "ON" : "OFF"} | ${backgroundStatusLabel(status)} | Pending: ${status.pending_changes}${suffix}`;
}
