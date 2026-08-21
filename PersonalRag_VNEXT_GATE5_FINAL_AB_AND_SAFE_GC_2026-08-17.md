# PersonalRag Segment vNext — Safe GC + Final Gate 5 A/B

Date: 2026-08-17

## Decision

**Production switch: HOLD**

Correctness and durable generation semantics pass on Linux, but the production-realistic multi-segment A/B still has material regressions in full build, mmap/open, high-hit common content queries, and durable small-delta publish latency.

## Safe GC

Added `gc_vnext_generation_store(root, grace_period)` and `VNextDurableGcReport`.

Safety rules:

- CURRENT is never modified by GC.
- The current manifest and every component reachable from it are retained.
- Only recognized immutable component/manifest names with `generation < CURRENT generation` are candidates.
- Unknown entries, staging names, and future generations are never touched.
- A configurable grace period defers young obsolete files.
- CURRENT is re-verified after candidate discovery and again after deletion.
- The published generation is reopened after GC to ensure restart integrity.
- On Windows, deletion failures consistent with an in-use mapped file are deferred rather than treated as reclaimed.

After the Gate 5 20k sequence (base + 4 delta + compaction), zero-grace GC removed 5 obsolete component directories and 5 obsolete manifests. Median GC time across 3 paired runs was **230.333 ms** and reclaimed **73,002,690 bytes**, reducing total vNext store bytes from **133,202,584** to **60,199,894**.

## Regression

Final Search Core regression:

- unit: 5/5 PASS
- production oracle: 35/35 PASS
- durable compaction: 6/6 PASS
- durable GC: 5/5 PASS
- durable generation: 11/11 PASS
- vNext generation: 7/7 PASS
- persistent index: 3/3 PASS
- vNext query: 9/9 PASS
- vNext segment: 17/17 PASS
- total: **98 tests PASS**
- `cargo fmt --check`: PASS
- `cargo clippy --all-targets -- -D warnings`: PASS
- release build: PASS
- `SELF_TEST_PASS`: PASS

All paired Gate 5 query comparisons had zero correctness mismatch.

## Gate 5: production-realistic 20k generation A/B

Three paired runs, medians where applicable. Both sides use generation stores. vNext uses 4 × 5k `.prseg2` segments.

### Full build + generation publish

| Metric | Perf12 | vNext | Result |
|---|---:|---:|---:|
| build median | 643.944 ms | 911.605 ms | vNext 1.42x slower |
| store bytes | 78,002,393 | 62,187,291 | vNext 20.3% smaller |

Separate isolated 20k full-build 5-run median: Perf12 592.437 ms vs vNext 883.528 ms (**1.49x slower**).

### Open

| Metric | Perf12 | vNext | Result |
|---|---:|---:|---:|
| open p50 | 18.918 ms | 234.197 ms | vNext 12.38x slower |

The dominant vNext cost is production-realistic multi-segment open/validation and per-segment persistent path-q3 cardinality cache construction.

### Base query p50

| Query | Hits | Perf12 | vNext | vNext / Perf12 |
|---|---:|---:|---:|---:|
| q1 | 20,000 | 0.284784 ms | 0.182983 ms | 0.64x |
| q2 | 19,777 | 0.216612 ms | 0.182712 ms | 0.84x |
| common `timeout` | 19,675 | 0.281629 ms | 1.550709 ms | **5.51x slower** |
| medium | 207 | 0.044066 ms | 0.025708 ms | 0.58x |
| rare | 2 | 0.017326 ms | 0.002964 ms | 0.17x |
| zero-hit | 0 | 0.018638 ms | 0.004587 ms | 0.25x |
| Japanese | 94 | 0.023014 ms | 0.009574 ms | 0.42x |
| long substring | 1 | 0.018477 ms | 0.007171 ms | 0.39x |
| block boundary | 1 | 0.019018 ms | 0.005338 ms | 0.28x |
| filename | 67 | 0.089813 ms | 0.005518 ms | 0.06x |
| path zero-hit | 0 | 0.098687 ms | 0.000381 ms | 0.004x |

The multi-segment common regression is important: the earlier single-20k-segment high-hit fast path does not translate to four 5k segments because each segment falls below the per-segment parallel threshold.

### Incremental durable publish

Median across 3 paired runs:

| Changed docs | Perf12 | vNext | Result |
|---:|---:|---:|---:|
| 1 | 48.527 ms | 260.012 ms | vNext 5.36x slower |
| 10 | 35.559 ms | 270.962 ms | vNext 7.62x slower |
| 100 | 35.466 ms | 275.392 ms | vNext 7.76x slower |
| 1000 | 44.216 ms | 292.868 ms | vNext 6.62x slower |

vNext remains generation-size dependent because pre-CURRENT durable validation reopens the cumulative multi-segment snapshot.

### Compaction

| Metric | Perf12 | vNext | Result |
|---|---:|---:|---:|
| compaction median | 2,305.619 ms | 1,123.376 ms | **vNext 2.05x faster** |

vNext compaction is a clear win and correctness before/after is exact.

## Longer text-heavy 50k

Three-run medians, isolated full-build benchmark:

| Metric | Perf12 | vNext | Result |
|---|---:|---:|---:|
| build median | 1,587.576 ms | 2,870.269 ms | **vNext 1.81x slower** |
| index/store bytes | 191,441,235 | 153,558,366 | vNext 19.8% smaller |
| peak RSS sample | 382,364 KiB | 249,736 KiB | vNext ~34.7% lower |

20k peak RSS sample: Perf12 299,416 KiB vs vNext 124,580 KiB (~58.4% lower).

## Filename-heavy 100k

Five-run build median:

| Metric | Perf12 | vNext | Result |
|---|---:|---:|---:|
| build median | 127.998 ms | 151.862 ms | vNext 18.6% slower |
| index bytes | 58,777,904 | 19,186,856 | **vNext 67.4% smaller** |
| peak RSS sample | 205,096 KiB | 108,396 KiB | **vNext ~47.1% lower** |

Query sample (31 rounds p50):

- `component_00042`: Perf12 0.047992 ms, vNext 0.031707 ms — vNext faster.
- `group_0042`: Perf12 0.032248 ms, vNext 0.038026 ms — vNext ~18% slower.
- one-hit long filename: Perf12 0.010526 ms, vNext 0.016204 ms — vNext ~1.54x slower but ~6 µs absolute difference.
- common `png`: Perf12 0.621736 ms, vNext 0.340096 ms — vNext faster.
- zero-hit: Perf12 0.000200 ms, vNext 0.000110 ms — vNext faster.

## Production criteria evaluation

Correctness:

- oracle mismatch: 0 — PASS
- Unicode/Japanese — PASS
- block-boundary false negatives — 0 / PASS
- malformed durable files fail closed — PASS
- newest-wins/tombstone/generation semantics — PASS
- durable CURRENT publish/restart recovery on Linux — PASS
- compaction before/after equality — PASS
- safe GC restart equality — PASS

Performance/operations:

- text-heavy build target (~1.5x faster desired): **FAIL**; vNext is slower, and 50k is ~1.81x slower.
- query no major regression: **FAIL** due multi-segment high-hit common query (~5.5x slower).
- mmap/open: **FAIL** due ~12.4x slower open at 20k/4 segments.
- incremental delta: **FAIL** due ~5.4–7.8x slower durable publish.
- filename-heavy: mixed; size/RSS/query are strong but build median is ~18.6% slower.
- compaction: PASS and materially faster.
- index size/RSS: PASS and materially smaller/lower.

## Final decision

**HOLD — do not switch production from Perf12 yet.**

The format/query design remains promising and correctness is strong, but Gate 5 exposed multi-segment operational costs not visible in earlier single-segment prototype measurements. The next optimization work should target, in order:

1. generation-level high-hit scheduling across multiple segments (not a per-segment 8192 threshold),
2. lazy/shared path-q3 cardinality metadata so open does not rebuild large per-segment RAM caches,
3. durable delta validation that validates only the new component plus manifest invariants rather than reopening the full cumulative generation,
4. parallel/full-build strategy that avoids repeating fixed-index setup for many 5k segments while preserving bounded local IDs.

Windows-native crash/power-loss durability and in-use-file GC behavior still require native validation before any production switch.
