# PersonalRag Segment Format vNext — Further Acceleration Report

Date: 2026-08-17

## Summary

This pass builds on the `AdaptiveFastPath` production-rollout candidate and targets remaining real-GUI hot paths without changing `.prseg2` serialized semantics.

Kept optimizations:

1. Prepared matcher reuse and dense logical-ID → physical-location table for first-N.
2. Conjunctive path+content adaptive first-N with early stop.
3. Persistent generation worker pool and dense live-ordinal hit bitset from the prior pass.
4. Parallel path q1/q2/q3 build workers.
5. Content q2 packed-radix builder while retaining the faster bucket path-q2 builder.
6. Clone-free path q3 build.
7. Async shadow duplicate-job coalescing and telemetry.

Two full-result common-query experiments were measured and rejected because they were slower on text-heavy corpora:
- scan every live document when global q3 density is very high;
- force one generation-global q3 anchor into every segment and bypass the mature local planner.

The production full-result common path therefore remains on the existing local planner.

## GUI first-N results

Corpus: 100,000 documents, GUI limit=2,000, release build, 31 rounds.

| Case | Previous AdaptiveFastPath p50 | Further Accelerated p50 | Improvement |
|---|---:|---:|---:|
| Content first-N | 0.219316 ms | 0.044166 ms | 4.97x faster |
| Path first-N | 0.158545 ms | 0.082733 ms | 1.92x faster |
| Path + content AND | legacy full/full/intersection 6.223159 ms | 0.126658 ms | 49.13x faster |

Final benchmark output:

```text
FIRST_N_BENCH docs=100000 limit=2000 rounds=31
build_ms=404.184
full_hits=100000
full_p50_ms=0.634802
full_p95_ms=1.650250
adaptive_hits=2000
adaptive_p50_ms=0.044166
adaptive_p95_ms=0.045427
path_adaptive_p50_ms=0.082733
path_adaptive_p95_ms=0.093429
legacy_both_p50_ms=6.223159
legacy_both_p95_ms=7.083231
adaptive_both_p50_ms=0.126658
adaptive_both_p95_ms=0.175811
```

### Why first-N improved

The old first-N hot loop rebuilt `ExactMatcher` and performed logical-ID HashMap lookup repeatedly. The new path:

- folds the query and prepares `ExactMatcher` once;
- uses a dense logical-ID location table when logical IDs are sufficiently compact;
- directly verifies the one-block document content slice when possible;
- stops immediately after the requested row-order limit is satisfied.

For path+content conjunction, the planner estimates each predicate's anchor cardinality. It either searches the selective side and verifies the other predicate, or—when both are dense—walks requested row order and exact-verifies both predicates, stopping at `limit`.

## Build optimization

Path q1/q2/q3 construction is now split into separate scoped workers. Content q2 uses the packed-radix builder; path q2 deliberately stays on the previous bucket builder because packed-radix was slower for the filename-heavy shape.

100k filename-heavy, alternating old/new runs, median:

```text
Previous AdaptiveFastPath: 142.595 ms
Further Accelerated:       133.841 ms
Improvement:                ~6.1%
```

This is an evidence-based hybrid: the slower path-q2 packed-radix experiment was not retained.

## Serialized-format compatibility

The same 100k corpus was built with the previous and new source. All 20 `.prseg2` files matched byte-for-byte, including file size and SHA-256.

```text
BYTE_IDENTICAL=true
segments_compared=20
```

No `.prseg2` migration is required.

## Final Gate 5 smoke

20k production-realistic release run, 31 query rounds:

```text
GATE5_BUILD     Perf12 667.076 ms | vNext 655.686 ms
GATE5_OPEN      Perf12 20.296 ms  | vNext 19.205 ms
base q2         Perf12 0.202440 ms | vNext 0.170433 ms
base common     Perf12 0.172025 ms | vNext 0.729413 ms
base medium     Perf12 0.044165 ms | vNext 0.027911 ms
base rare       Perf12 0.018367 ms | vNext 0.004667 ms
base zero       Perf12 0.019329 ms | vNext 0.007331 ms
base Japanese   Perf12 0.024266 ms | vNext 0.011677 ms
base filename   Perf12 0.098316 ms | vNext 0.005508 ms
base path_zero  Perf12 0.064746 ms | vNext 0.000401 ms

delta 1        Perf12 33.352 ms | vNext 9.213 ms
delta 10       Perf12 62.016 ms | vNext 24.310 ms
delta 100      Perf12 45.199 ms | vNext 16.534 ms
delta 1000     Perf12 52.243 ms | vNext 28.677 ms

compaction      Perf12 2482.103 ms | vNext 753.008 ms
```

The raw full-result high-true-hit `common` query remains the known vNext trade-off. This pass intentionally optimizes the actual GUI limit path rather than reintroducing a large positional index solely to win the pathological all-results benchmark.

## Async shadow coalescing

Shadow comparison remains off the response thread and now coalesces duplicate pending/in-flight work by:

```text
(index_dir, generation, file_query, content_query)
```

New telemetry:

```text
shadow_coalesced
```

This reduces background CPU competition and bounded-queue pressure during shadow burn-in. The Windows production-switch validation script now includes a dedicated coalescing regression and requires the conjunctive first-N benchmark evidence.

## Correctness / regression

Linux Search Core final hard gate:

```text
unit                    5 / 5 PASS
production oracle      35 / 35 PASS
production shadow       1 / 1 PASS
vNext generation       11 / 11 PASS
durable generation     12 / 12 PASS
durable compaction      6 / 6 PASS
durable GC              5 / 5 PASS
persistent              5 / 5 PASS
vNext query              9 / 9 PASS
vNext segment           17 / 17 PASS
---------------------------------
TOTAL                  106 / 106 PASS

cargo fmt --check              PASS
Clippy -D warnings             PASS
```

Frontend source, including the new shadow telemetry field, passes strict TypeScript validation with a validation-only Tauri API stub.

Bridge/Tauri source passes `rustfmt` parsing. Full Bridge Cargo compilation cannot be claimed in this Linux environment because the known offline dependency set is incomplete (`ignore 0.4.33` is unavailable before source compilation). The Windows native validation harness remains the required hard gate for Bridge/Tauri and shadow coalescing.

## Production status

The rollout architecture is unchanged:

- default remains `perf12` until Windows native acceptance and shadow burn-in;
- `shadow` returns Perf12 immediately and compares vNext asynchronously;
- `vnext` falls back automatically to Perf12 if the vNext shadow is stale/unavailable.

This optimization pass materially reduces real-GUI first-N latency without changing `.prseg2` bytes or weakening the rollback strategy.
