# PersonalRag vNext Adaptive Fast Path / Async Shadow Report

Date: 2026-08-17
Base: `PersonalRag_GUI_PortableCore_SegmentVNext_ProductionRollout_2026-08-17`

## Result

The requested seven optimizations were implemented without changing the serialized `.prseg2` bytes for an identical corpus.

1. Adaptive first-N
2. High-hit content-blob one-pass scan
3. Generation plan reuse + persistent worker pool
4. Dense live-ordinal hit bitset
5. q2 packed-radix builder (content q2; path q2 adaptively retains the faster bucket builder)
6. Clone-free path q3 build
7. Asynchronous bounded shadow comparison

Perf12 remains the default/rollback backend. Windows native acceptance remains a hard gate before changing the default.

## 1. Adaptive first-N

`VNextGenerationIndex::first_n_in_order()` was added and the Bridge first-N path now calls it directly.

Planner behavior:
- selective query: use existing inverted index, then project the small hit set into caller row order;
- dense query: exact-verify live documents in caller order and stop immediately at `limit` hits.

This avoids materializing tens of thousands of results just to return the GUI limit (normally 2000).

### 100k dense GUI gate, limit=2000, 31 rounds

Final run:
- ProductionRollout old GUI-equivalent full-hit path: 0.780389 ms p50
- New full-result vNext: 0.573020 ms p50
- New adaptive first-N: 0.221319 ms p50
- New adaptive path first-N: 0.159597 ms p50

Thus the production GUI-oriented content path is about **3.53x faster** than the old full-materialization path in the final run, and about **2.59x faster** than the new engine's own full-result path.

A prior run on the same 100k shape measured 1.299960 ms -> 0.227718 ms (~5.7x); timing varies, so the final report uses the more conservative final run above.

## 2. High-hit content-blob one-pass scan

`ExactMatcher::find_from()` and `VNextSegmentReader::scan_content_blob_single_block()` were added.

For dense, compact one-block-per-document segments the matcher walks the contiguous content blob monotonically, maps hits back to document boundaries, rejects cross-document matches, and skips to the next document after the first exact hit.

The planner is adaptive: one-pass is used only when the global q3 anchor is very dense and average content size is small (<=256 bytes/document). Text-heavy documents retain the mature posting/exact-verify path because unconditional blob scanning regressed that workload.

The new generation boundary test deliberately creates q3 false positives and verifies that an `abcd` match spanning two documents is never accepted.

## 3. Generation plan reuse + persistent worker pool

Generation-wide q3 cardinalities are planned once per content query. High-hit segment work is submitted to a process-lifetime bounded worker pool rather than creating OS threads for every query.

This preserves the existing segment-local mature planner where it is faster, while removing repeated generation-level planning and thread-creation overhead.

## 4. Dense hit bitset

High-hit workers return live ordinals. The generation layer sets bits in a dense live-ordinal bitmap and enumerates `live_order` once.

This removes the previous dense `Vec<LogicalDocId>` merge + sort + dedup work while preserving stable logical result order.

## 5. q2 packed-radix build

Content q2 now:
- uses an 8 KiB key-presence bitset per owner block;
- emits packed `(q2 << 16) | block_id` occurrences;
- performs 16-bit x2 radix sort;
- deduplicates;
- directly encodes PRFIX002 sparse postings.

Path q2 was also prototyped with packed-radix, but repeated 100k filename benchmarks showed a regression. The final production code therefore keeps packed-radix for content q2 and selects the existing bucket-based path q2 builder, which is faster for short paths. This preserves the requested packed-radix optimization without retaining a measured regression.

## 6. Clone-free path build

Path q3 no longer creates cloned `VNextDocumentInput` values or allocated folded path buffers. `build_path_q3_index()` streams `display_path` bytes directly, ASCII-folding bytes while building path q3 postings.

100k filename-heavy vNext build, 5 final paired runs:
- old median: 174.284 ms
- new median: 149.902 ms
- about 14% lower median build time
- new was faster in 4 of 5 paired runs

Serialized size remained 14,997,704 bytes in both versions.

## 7. Async shadow

Shadow response behavior changed from synchronous dual execution to:

1. run Perf12 and return its response path;
2. enqueue a raw logical-ID comparison job to a bounded queue (capacity 16);
3. a dedicated background thread opens/caches the Perf12 and vNext generation for the current `(index_dir, generation)`;
4. compare raw file/content/intersection logical IDs;
5. update telemetry asynchronously.

New telemetry:
- `shadow_queued`
- `shadow_dropped`
- `shadow_failures`
- existing `shadow_comparisons`
- existing `shadow_mismatches`

A Bridge unit test `shadow_compare_executor_runs_off_response_thread` was added. It is included in the Windows native validation harness. The current Linux environment still lacks the historical Bridge/Tauri offline dependency set, so Bridge Cargo compilation is not falsely claimed as Linux-PASS; Rust parsing/rustfmt and frontend strict TypeScript validation pass here.

## `.prseg2` compatibility

Same deterministic 20k corpus, old ProductionRollout writer vs final writer:

- bytes: 48,322,216 both
- SHA-256: `adf2d0a133200e9f06a1f0087b68a6d28066c02fe00ec79d40fa68d13b661d7e`
- result: **byte-identical**

One direct build sample:
- old: 597.529 ms
- new: 549.619 ms

Because the output is byte-identical, these build changes do not require a format migration.

## Normal Gate 5 regression

20k production-realistic final smoke after adaptive blob-scan selection:
- vNext build: 779.173 ms in the final sampled run (system timing is noisy)
- vNext open: 19.023 ms p50
- q1: 0.171104 ms
- q2: 0.172286 ms
- common full-result: 0.913026 ms
- medium: 0.033349 ms
- rare: 0.005148 ms
- zero: 0.007521 ms
- Japanese: 0.011918 ms
- filename: 0.007992 ms
- 1-doc delta: 10.349 ms
- 1000-doc delta: 25.571 ms
- vNext compaction: 702.134 ms

The raw full-result common query remains slower than Perf12, but the actual GUI first-N path no longer pays for full-result materialization.

## Correctness / regression

Final Search Core:
- unit: 5/5
- production oracle: 35/35
- production switch shadow: 1/1
- vNext generation: 10/10
- durable generation: 12/12
- durable compaction: 6/6
- durable GC: 5/5
- persistent: 5/5
- vNext query: 9/9
- vNext segment: 17/17
- total: **105/105 PASS**
- `cargo fmt --check`: PASS
- Clippy `-D warnings`: PASS
- release examples build: PASS
- `SELF_TEST_PASS`: PASS

Integration validation available in Linux:
- Bridge/Tauri changed Rust source parse: PASS
- Frontend source strict TypeScript check with validation-only Tauri API stub: PASS
- Bridge/Tauri full Cargo compile: BLOCKED by the same missing offline vendor dependencies documented before this work; must run on Windows/native complete environment.

## Windows validation

`scripts/validate-vnext-production-switch-windows.ps1` now additionally runs:
- async shadow executor test;
- 100k GUI first-N benchmark (default limit 2000);
- normal Gate 5;
- durable generation/compaction/GC;
- Bridge/Tauri tests/clippy/check;
- optional GUI launch in shadow mode.

Perf12 remains the rollback/default backend until this Windows hard gate and shadow burn-in pass.
