# PersonalRag Segment vNext — Gate 5 Retry Final A/B

Date: 2026-08-17

## Executive decision

**Linux Search Core Gate 5: PASS / Production Candidate.**

The four Gate-5 HOLD blockers were reworked and re-benchmarked. Full-build/open/durable-delta/fixed-index overhead are no longer production blockers. The remaining known regression is the pathological high-true-hit common content query: vNext block-level q3 must exact-verify almost every matching document, while Perf12's positional acceleration can prove the same query more cheaply. Even there, measured absolute full-result latency remains 0.58 ms at 20k and 5.17 ms at 100k.

**Actual Windows production default switch remains a controlled/conditional GO, not an unconditional PASS**, because Windows-native crash/power-loss durability and native A/B were not executed in this Linux environment. Keep Perf12 available as rollback/correctness oracle for the first production rollout.

## Correctness gate

Final Search Core regression:

- unit: 5/5 PASS
- Perf12 production oracle: 35/35 PASS
- durable compaction: 6/6 PASS
- durable GC: 5/5 PASS
- durable generation: 12/12 PASS
- vNext generation: 8/8 PASS
- persistent index: 5/5 PASS
- vNext query: 9/9 PASS
- vNext segment: 17/17 PASS
- total: **102 tests PASS**
- `cargo fmt --check`: PASS
- Clippy `-D warnings`: PASS
- release build: PASS
- `pr_portable self-test`: `SELF_TEST_PASS`

Perf12 production code was not modified.

## Changes since Gate5 HOLD

### 1. Generation-level high-hit scheduler

The 8192 high-hit threshold is now decided across the whole generation, not independently per segment. This fixes the 4x5k/10x5k case where every individual segment was below the threshold while the global query was extremely dense.

- Global distinct q3 cardinalities are summed across all generation segments.
- If the rarest global q3 still has >=8192 candidates, immutable segments are distributed across available workers.
- Local segment planners remain concurrent; the rejected design that serially materialized all segment candidates before verification was measured slower and removed.
- For >=75% global q3 density, one-block segments bypass redundant posting enumeration and perform contiguous exact verification.
- Dedicated test: four 2050-candidate segments (each <8192) produce 8200 generation-wide hits and remain exact.

Result: common-query regression materially reduced, but not eliminated because high-true-hit exact verification remains intrinsic to the current block-q3 format.

### 2. Lazy path-q3 metadata + published-fast open

- Path q3 `(key, cardinality)` planning cache changed from eager open-time expansion to `OnceLock` lazy construction.
- Immutable segments within a generation are opened/validated concurrently.
- Public `VNextSegmentReader::open` remains strict and still performs full structural/posting validation.
- Durable `CURRENT` reopen uses a separate published-fast path only for bytes that were already strict-validated before CURRENT publication:
  - validates header/footer/version/ranges;
  - verifies whole-file checksum every reopen;
  - trusts prior strict structural validation of unchanged immutable bytes;
  - skips the second redundant per-section checksum scan because the whole-file checksum covers all section bytes and descriptors.
- New test bit-flips a published `.prseg2` and verifies restart/open fails closed on checksum mismatch.

Result: 20k open fell from the previous ~234 ms HOLD baseline to ~19 ms, effectively parity with Perf12.

### 3. Localized durable incremental validation

New-format `CURRENT` stores checksum-protected `live_docs`.

For a new delta publish:

- Existing manifest layers must match the old immutable layer prefix exactly.
- Existing base/delta components are not reopened.
- Only the newly written delta `.prseg2` files and tombstones are strict-validated.
- Persisted delta logical ID/path/content/tombstones are compared with the `UpdatePlan`.
- `live_docs_after` is recomputed from old `live_docs`, inserts and pure deletions.
- Old CURRENT format remains readable and falls back to one legacy full-open when needed.

Result: 1/10/100/1000-document durable publishes are now materially faster than Perf12.

### 4. 5k-segment fixed-index overhead reduction

q1 remains `PRFIX001` dense because its domain is only 256 keys.

q2 now uses `PRFIX002` sparse sorted metadata:

- only present q2 keys allocate persistent metadata;
- old `PRFIX001` q2 remains reader-compatible;
- postings retain Singleton / RawU16 / DenseBitmap;
- builder uses naturally ordered per-key posting lists rather than a global occurrence sort, restoring filename-heavy build throughput;
- corruption test repairs section/file checksums after corrupting sparse metadata and still fails closed.

This removes the old ~1 MiB empty 65,536-entry q2 metadata table per q2 index per segment.

## Final 20k paired Gate 5

Three paired runs, median values, 5k-doc vNext segments.

| Metric | Perf12 | vNext | vNext result |
|---|---:|---:|---:|
| Full build + publish | 670.937 ms | 728.916 ms | 1.086x time |
| Open p50 | 18.673 ms | 19.039 ms | 1.020x time |
| Store bytes | 78,002,393 | 53,935,691 | **30.9% smaller** |
| q1 p50 | 0.209 ms | **0.170 ms** | 1.23x faster |
| q2 p50 | 0.204 ms | **0.169 ms** | 1.21x faster |
| common p50 | **0.208 ms** | 0.581 ms | 2.79x slower |
| medium p50 | 0.045 ms | **0.027 ms** | 1.68x faster |
| rare p50 | 0.0178 ms | **0.00464 ms** | 3.84x faster |
| zero p50 | 0.0193 ms | **0.00726 ms** | 2.66x faster |
| Japanese p50 | 0.0239 ms | **0.0113 ms** | 2.12x faster |
| long p50 | 0.0191 ms | **0.0112 ms** | 1.71x faster |
| block-boundary p50 | 0.0196 ms | **0.00844 ms** | 2.32x faster |
| filename p50 | 0.106 ms | **0.00555 ms** | 19.2x faster |
| path zero p50 | 0.0692 ms | **0.000401 ms** | 172x faster |
| delta 1 | 42.893 ms | **14.992 ms** | 2.86x faster |
| delta 10 | 41.460 ms | **15.118 ms** | 2.74x faster |
| delta 100 | 35.494 ms | **13.209 ms** | 2.69x faster |
| delta 1000 | 50.018 ms | **18.335 ms** | 2.73x faster |
| Compaction | 2798.867 ms | **726.019 ms** | 3.85x faster |

The build runs were somewhat noisy; isolated 50k build-only medians below are more stable for scale comparison.

## 50k build/resource scale

Three isolated build-only runs:

- Perf12 median: **1581.050 ms**
- vNext durable median: **1706.985 ms**
- vNext: ~8.0% slower
- Perf12 index: 191,441,235 bytes
- vNext durable store: 132,924,638 bytes (**30.6% smaller**)

Peak RSS sampled on the same 50k synthetic corpus:

- Perf12: 372,296 KiB
- vNext: 229,084 KiB
- vNext: **38.5% lower peak RSS**

A full 50k Gate-5 run also showed open 51.1 ms vs 51.6 ms after published-fast open and vNext deltas/compaction substantially ahead. Exact build timings in long mixed runs were more scheduler-noisy than isolated build-only runs.

## 100k text-heavy scale smoke

One single-run scale smoke completed through the delta phase before the execution-time cap stopped the later 100k compaction phase. This is not presented as a repeated median.

- full build: Perf12 3792.505 ms / vNext 3190.372 ms
- open: Perf12 89.777 ms / vNext 86.933 ms
- store: Perf12 389,441,369 bytes / vNext 269,284,229 bytes
- common, 98,435 hits: Perf12 0.672 ms / vNext 5.166 ms
- q2: 0.988 / 0.905 ms
- medium: 0.284 / 0.246 ms
- rare: 0.0267 / 0.0229 ms
- Japanese: 0.0941 / 0.0759 ms
- filename: 0.195 / 0.0896 ms
- delta 1: 118.9 / 8.86 ms
- delta 10: 126.0 / 10.27 ms
- delta 100: 130.5 / 19.28 ms
- delta 1000: 133.6 / 13.76 ms

The common query remains the sole material content-query regression. It returns ~98% of all documents, so vNext's exact block/document verification has real work that Perf12's positional acceleration can avoid. Absolute latency remained ~5.2 ms at 100k documents.

## 100k filename-heavy

Three runs, build median:

- Perf12: 125.958 ms
- vNext: **110.240 ms** (1.14x faster)
- Perf12 index: 58,777,904 bytes
- vNext index: **14,997,704 bytes** (74.5% smaller)

Query medians:

- `component_00042`: 0.0484 / **0.0321 ms**
- `group_0042`: **0.0336** / 0.0390 ms
- one-hit long name: **0.0106** / 0.0164 ms
- common `png` 100k hits: 0.5027 / **0.2784 ms**
- zero-hit: 0.000190 / **0.000110 ms**

There is no broad filename-heavy regression; the long 1-hit case remains slower by only ~5.7 microseconds absolute.

## Production switch judgement

### Gate status

- correctness mismatch: **0 / PASS**
- Unicode/Japanese: **PASS**
- block-boundary: **PASS**
- malformed strict open fail-closed: **PASS**
- durable random-corruption checksum fail-closed: **PASS**
- generation/newest-wins/tombstone: **PASS**
- durable publish/restart: **PASS**
- compaction/GC: **PASS**
- filename-heavy: **PASS**
- open: **PASS after retry**
- durable small delta: **PASS after retry**
- build: **acceptable by aggregate criterion** (near parity/slightly slower at 20k/50k, lower RSS/index; 100k smoke faster)
- high-true-hit common full-result content query: **known residual regression**

### Decision

**Linux Search Core Gate 5 is promoted from HOLD to Production Candidate / controlled GO.**

Reason: the previous structural blockers are resolved, all correctness gates pass, open is now near parity, durable deltas/compaction are substantially faster, index/RSS are substantially lower, filename-heavy is stronger, and nearly all query classes are faster. The remaining common-query regression is large as a ratio but remains low single-digit milliseconds even at 100k documents while returning ~98k IDs.

Do **not** delete Perf12 yet. Recommended rollout:

1. keep Perf12 as rollback/correctness oracle;
2. switch the Search Core default behind a feature/config flag;
3. run Windows-native full regression and Gate-5 smoke, including crash/restart behavior;
4. collect real-corpus common-query telemetry;
5. remove or demote Perf12 only after one stable production observation window.

Because Windows-native durability/A-B was not executed here, the actual Windows product default switch is **conditional on that final native validation**.

## Rejected optimizations during this retry

The following were implemented/benchmarked and removed because they regressed performance or correctness:

- serially materializing all generation candidate vectors before worker redistribution: slower common query;
- whole-generation dense exact scan for all segments: a first version incorrectly dropped a segment containing a multi-block document; oracle mismatch caught it immediately;
- generic full-document dense scan: exact but slower than the hybrid one-block fast path;
- global packed-occurrence sort for sparse q2: reduced persistent metadata but regressed 100k filename build; replaced by naturally ordered per-key posting lists.

## Windows caveat

Linux validates file and directory `fsync` behavior used by the durable store. Windows-native power-loss/crash semantics were not measured in this environment and must not be called PASS until run on the actual Windows target.
