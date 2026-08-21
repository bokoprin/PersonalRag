export type SearchGeneration = {
  id: number;
  fileQuery: string;
  contentQuery: string;
};

export function shouldApplySearchResult(
  currentGeneration: number,
  responseGeneration: number,
): boolean {
  return currentGeneration === responseGeneration;
}

export function nextSearchGeneration(current: number): number {
  return current + 1;
}

/**
 * Short content queries are intentionally delayed longer so that a user
 * typing a 1- or 2-character prefix does not start repeated direct scans.
 * Unicode scalar values, rather than UTF-16 code units, define the length.
 */
export function contentQueryDebounceMs(
  value: string,
  normalMs = 150,
  shortQueryMs = 600,
): number {
  const length = Array.from(value.trim()).length;
  return length > 0 && length < 3 ? shortQueryMs : normalMs;
}

export const RESULT_WINDOW_SIZE = 250;

export function visibleResultWindow<T>(items: readonly T[], visibleCount: number): T[] {
  return items.slice(0, Math.max(0, visibleCount));
}

export function canLoadMore(total: number, visibleCount: number): boolean {
  return visibleCount < total;
}

export function loadMoreCount(visibleCount: number, pageSize = RESULT_WINDOW_SIZE): number {
  return visibleCount + Math.max(1, pageSize);
}

function escapeHtml(value: string): string {
  return value.replace(/[&<>"']/g, (character) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[character] ?? character);
}

function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

export function highlightText(
  line: string,
  query: string,
  regexMode = false,
  matchCase = false,
): string {
  const pattern = query.trim();
  if (!pattern) return escapeHtml(line);
  let matcher: RegExp;
  try {
    matcher = new RegExp(regexMode ? pattern : escapeRegExp(pattern), `g${matchCase ? "" : "i"}u`);
  } catch {
    return escapeHtml(line);
  }
  let output = "";
  let cursor = 0;
  for (const match of line.matchAll(matcher)) {
    const start = match.index ?? 0;
    const matched = match[0];
    output += escapeHtml(line.slice(cursor, start));
    output += `<mark>${escapeHtml(matched)}</mark>`;
    cursor = start + matched.length;
    if (!matched) cursor += 1;
  }
  return output + escapeHtml(line.slice(cursor));
}
