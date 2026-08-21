import { describe, expect, it } from "vitest";
import {
  canLoadMore,
  contentQueryDebounceMs,
  highlightText,
  loadMoreCount,
  nextSearchGeneration,
  RESULT_WINDOW_SIZE,
  shouldApplySearchResult,
  visibleResultWindow,
} from "../src/search_state";

describe("adaptive content-query debounce", () => {
  it("uses the short-query delay for one or two Unicode scalar values", () => {
    expect(contentQueryDebounceMs("a")).toBe(600);
    expect(contentQueryDebounceMs("あ😀")).toBe(600);
  });

  it("uses the normal delay for empty and three-or-more-character queries", () => {
    expect(contentQueryDebounceMs("  ")).toBe(150);
    expect(contentQueryDebounceMs("abc")).toBe(150);
    expect(contentQueryDebounceMs("あいう")).toBe(150);
  });
});

describe("search request generation", () => {
  it("rejects an older response after a newer request", () => {
    const current = nextSearchGeneration(4);
    expect(shouldApplySearchResult(current, 4)).toBe(false);
    expect(shouldApplySearchResult(current, 5)).toBe(true);
  });

  it("highlights a literal hit without allowing HTML injection", () => {
    expect(highlightText("timeout <safe>", "timeout")).toBe("<mark>timeout</mark> &lt;safe&gt;");
  });

  it("keeps the initial DOM window bounded and supports incremental loading", () => {
    const hits = Array.from({ length: 2_000 }, (_, index) => index);
    expect(visibleResultWindow(hits, RESULT_WINDOW_SIZE)).toHaveLength(RESULT_WINDOW_SIZE);
    expect(canLoadMore(hits.length, RESULT_WINDOW_SIZE)).toBe(true);
    expect(loadMoreCount(RESULT_WINDOW_SIZE)).toBe(RESULT_WINDOW_SIZE * 2);
    expect(visibleResultWindow(hits, loadMoreCount(RESULT_WINDOW_SIZE))).toHaveLength(500);
    expect(canLoadMore(500, 500)).toBe(false);
  });
});
