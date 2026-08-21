import { describe, expect, it } from "vitest";
import {
  backgroundStatusLabel,
  backgroundStatusSummary,
  rebuildPhaseLabel,
  rebuildProgressMetrics,
} from "../src/background_state";

describe("background status rendering", () => {
  it("renders catch-up and pending state", () => {
    expect(backgroundStatusLabel({ running: true, mode: "usn", sync_state: "catching_up", pending_changes: 3 })).toBe("Catching up");
    expect(backgroundStatusSummary({ running: true, mode: "usn", sync_state: "catching_up", pending_changes: 3 })).toContain("Pending: 3");
  });

  it("prioritizes a degraded error", () => {
    expect(backgroundStatusLabel({ running: true, mode: "reconcile", sync_state: "degraded", pending_changes: 0, last_error: "access denied" })).toContain("access denied");
  });

  it("includes rebuild progress without hiding the background state", () => {
    const summary = backgroundStatusSummary({
      running: true,
      mode: "rebuild",
      sync_state: "rebuilding",
      pending_changes: 0,
      rebuild: {
        job_id: "rebuild-test",
        state: "scanning",
        progress: {
          total_files: 20,
          processed_files: 12,
          indexed_files: 10,
          unchanged_files: 2,
          skipped_files: 0,
          error_files: 0,
          bytes_read: 100,
          elapsed_ms: 42,
        },
      },
    });
    expect(summary).toContain("Rebuild: scanning 12 files");
  });

  it("calculates percent, remaining files, and ETA from live rebuild progress", () => {
    const metrics = rebuildProgressMetrics({
      job_id: "rebuild-test",
      state: "scanning",
      started_at: "1000",
      progress: {
        total_files: 100,
        processed_files: 25,
        indexed_files: 25,
        unchanged_files: 0,
        skipped_files: 0,
        error_files: 0,
        bytes_read: 0,
        elapsed_ms: 2_000,
        eta_ms: 6_000,
        estimated_completion_at_ms: 9_000,
      },
    }, 3_000);
    expect(metrics.percent).toBe(25);
    expect(metrics.remainingFiles).toBe(75);
    expect(metrics.elapsedMs).toBe(2_000);
    expect(metrics.remainingMs).toBe(6_000);
    expect(metrics.estimatedCompletionAt).toBe(9_000);
  });

  it("does not invent an ETA before the first file is processed", () => {
    const metrics = rebuildProgressMetrics({
      job_id: "rebuild-test",
      state: "scanning",
      started_at: "1000",
      progress: {
        total_files: 100,
        processed_files: 0,
        indexed_files: 0,
        unchanged_files: 0,
        skipped_files: 0,
        error_files: 0,
        bytes_read: 0,
        elapsed_ms: 0,
      },
    }, 3_000);
    expect(metrics.percent).toBe(0);
    expect(metrics.remainingFiles).toBe(100);
    expect(metrics.remainingMs).toBeNull();
    expect(metrics.estimatedCompletionAt).toBeNull();
  });
  it("does not fall back to a lifetime-average ETA when rolling ETA is unavailable", () => {
    const metrics = rebuildProgressMetrics({
      job_id: "rebuild-no-rolling-eta",
      state: "reconciling",
      started_at: "1000",
      progress: {
        phase: "building",
        total_files: 100,
        processed_files: 50,
        indexed_files: 50,
        unchanged_files: 0,
        skipped_files: 0,
        error_files: 0,
        bytes_read: 100,
        elapsed_ms: 10_000,
        eta_ms: null,
      },
    }, 11_000);
    expect(metrics.remainingMs).toBeNull();
    expect(metrics.estimatedCompletionAt).toBeNull();
  });

  it("keeps active finalization below 100 percent", () => {
    const metrics = rebuildProgressMetrics({
      job_id: "rebuild-finalizing",
      state: "reconciling",
      started_at: "1000",
      progress: {
        phase: "building",
        total_files: 100,
        processed_files: 100,
        indexed_files: 100,
        unchanged_files: 0,
        skipped_files: 0,
        error_files: 0,
        bytes_read: 1000,
        elapsed_ms: 2_000,
      },
    }, 3_000);
    expect(metrics.percent).toBe(99);
    expect(metrics.remainingFiles).toBe(0);
    expect(metrics.remainingMs).toBeNull();
  });

  it("uses terminal rebuild state as the phase label", () => {
    expect(rebuildPhaseLabel({
      job_id: "rebuild-completed",
      state: "completed",
      progress: {
        phase: "publishing",
        total_files: 10,
        processed_files: 10,
        indexed_files: 10,
        unchanged_files: 0,
        skipped_files: 0,
        error_files: 0,
        bytes_read: 100,
        elapsed_ms: 10,
      },
    })).toBe("completed");
  });

});
