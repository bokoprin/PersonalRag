# PersonalRag High-Hit Generation Fast Path Report — 2026-08-18

## 1. Scope

Baseline: `PersonalRag_GUI_PortableCore_SIMDQueryFastPath_2026-08-18.zip`

Goal: profile and accelerate high-hit query result materialization, multi-generation merge, and newest-wins filtering without changing `.prseg2` format, logical result ordering, tombstone semantics, or newest-wins semantics.

The production changes are limited to:

- `search-core/src/vnext_generation.rs`
- `search-core/src/vnext_query.rs`
- `search-core/tests/vnext_generation.rs`

Reproducible benchmark examples were added:

- `search-core/examples/generation_high_hit_bench.rs`
- `search-core/examples/generation_high_hit_long_bench.rs`

No writer/on-disk-format code was changed.

## 2. Pre-change gate

Before implementation:

- `cargo test --locked --offline`: **146/146 PASS**
- `cargo fmt --check`: PASS
- `cargo clippy --locked --offline --all-targets -- -D warnings`: PASS

## 3. Sub-profiler

Added an opt-in generation-level profiler:

```text
PR_PROFILE_GENERATION_QUERY=1
```

It reports:

- segment count / query workers / planned high-hit jobs
- `scan_all`
- physical matches
- visible/newest-wins matches
- result hits
- submission time
- worker search sum
- newest-wins filter sum
- coordinator wait time
- bitmap OR time
- generation merge time
- final materialization time
- total high-hit generation time

The generation profiler is intentionally separate from the lower-level `PR_PROFILE_QUERY` SIMD profiler so profiling the coordinator does not turn on expensive per-search inner instrumentation.

### Main profiling finding

At the start of the wave, a representative 60k-live-document / 3-segment / 3k-change 100%-hit query showed that segment searching was already parallel, while the coordinator still paid meaningful merge and result materialization cost. Newest-wins visibility was already mostly resolved at generation open time through the live mapping, so the profitable work was to avoid re-materializing local hit vectors and to reduce coordinator work.

After the final implementation, warm profiler medians for the compact one-block-per-document corpus were:

| query | jobs | worker search sum | wait | bitmap OR | materialize | high-hit phase total |
|---|---:|---:|---:|---:|---:|---:|
| 100% hit | 5 | 1.296 ms | 0.417 ms | **0.0022 ms** | **0.0135 ms** | 0.463 ms |
| 98% hit | 5 | 1.387 ms | 0.460 ms | **0.0022 ms** | **0.0621 ms** | 0.569 ms |

For the larger-document (`scan_all=false`) corpus:

| query | jobs | worker search sum | newest filter | wait | bitmap OR | materialize | high-hit phase total |
|---|---:|---:|---:|---:|---:|---:|---:|
| 100% hit | 3 | 0.564 ms | 0.036 ms | 0.299 ms | **0.0010 ms** | **0.0070 ms** | 0.331 ms |
| 98% hit | 3 | 0.850 ms | 0.053 ms | 0.448 ms | **0.0008 ms** | **0.0293 ms** | 0.499 ms |

The coordinator's actual bitmap merge is therefore effectively at the floor; the remaining merge-labelled time is overwhelmingly worker completion wait.

## 4. Final implementation

### 4.1 Worker-local generation live bitmap

Previously each segment worker returned a `Vec<u32>` of local document IDs and the coordinator converted hits one by one into generation-visible results.

The high-hit path now returns a bitmap indexed by the generation's stable live ordinal. The coordinator combines worker results with word-wise `u64` OR.

This changes coordinator work from per-hit merging to per-word merging while retaining final logical-ID order through `live_order`.

### 4.2 Dense scan → newest-wins filtering → bitmap in one pass

For one-block-per-document dense high-hit searches, `VNextSegmentReader` now exposes a range visitor:

```text
scan_content_blob_single_block_range_visit(...)
```

Exact verification and document-boundary validation still happen in the segment reader, but instead of first building a local `Vec<u16>` of hits, each exact hit is immediately mapped through `live_ordinals` and written into the generation bitmap.

This removes an intermediate hit vector and a second pass over dense hits.

### 4.3 Exact-capacity result materialization

`materialize_live_hit_bitmap` now:

- validates bitmap shape/cardinality fail-closed;
- directly returns `live_order.to_vec()` for an all-live-documents hit;
- preallocates exact result capacity for partial hits;
- uses set-bit iteration for sparse results;
- uses linear live-order bitmap testing for dense results.

This preserves result order exactly.

### 4.4 Bounded range-job scheduling

Compact high-hit segments can be split into document-aligned block ranges. Jobs are weighted by block count, sorted largest-first, and greedily assigned to the least-loaded worker in the existing bounded generation query pool.

A minimum target chunk size of 8,192 blocks avoids creating tiny jobs.

Example for the 60k benchmark:

```text
30k base + 30k base + 3k delta
→ ~15k + 15k + 15k + 15k + 3k jobs
→ balanced across four query workers
```

The range scanner requires one block per document and validates document/block alignment. Other segments fall back to the normal exact search path.

### 4.5 Avoid nested query oversubscription

For generation-level high-hit fallback work where `scan_all=false`, generation workers already run concurrently. Allowing each segment query to spawn its own inner verification threads created nested parallelism and oversubscribed the 5-CPU test environment.

A generation-worker-specific exact search entry point therefore disables segment-internal q3 verification parallelism while generation-level parallelism owns the CPU budget.

Ordinary single-segment searches still retain the existing inner parallel path.

This was the dominant improvement for larger-document high-hit queries.

## 5. Standard release A/B

Both baseline and candidate were compiled from their respective sources with the normal Cargo release profile. Each line below is the median of seven AB/BA-alternated process runs, with 301 measured queries per process.

### 5.1 Compact 60k live docs, 3 segments, one block per document

| query | metric | SIMD baseline | candidate | improvement |
|---|---|---:|---:|---:|
| 100% hit | p50 | 0.510256 ms | **0.434875 ms** | **14.77%** |
| 100% hit | p95 | 0.779615 ms | **0.612277 ms** | **21.46%** |
| 98% hit | p50 | 0.565367 ms | **0.554962 ms** | **1.84%** |
| 98% hit | p95 | 0.879914 ms | **0.721649 ms** | **17.99%** |

The 98%-hit p50 is noisy on the shared 5-CPU execution environment, so the modest 1.84% median improvement should not be overstated. Its p95 improvement is much clearer.

### 5.2 Larger-document 30k live docs, `scan_all=false`

| query | metric | SIMD baseline | candidate | improvement |
|---|---|---:|---:|---:|
| 100% hit | p50 | 0.580541 ms | **0.276309 ms** | **52.40%** |
| 100% hit | p95 | 0.899372 ms | **0.442806 ms** | **50.76%** |
| 98% hit | p50 | 0.570255 ms | **0.339964 ms** | **40.38%** |
| 98% hit | p95 | 0.841276 ms | **0.459911 ms** | **45.33%** |

The large gain here is primarily removal of nested generation-level + segment-level verification parallelism.

## 6. Rejected / reverted experiments

### 6.1 Bitmap-only coordinator change

Worker bitmap + word-wise OR alone improved high-hit queries only a few percent. It was kept because it enabled the more important one-pass fusion and result materialization changes, not because the isolated gain was large.

### 6.2 Per-worker grouping of multiple range jobs

A late experiment reused one bitmap/channel for all jobs assigned to a worker. Some short-query runs improved substantially, but repeated AB/BA measurements were unstable and other cases regressed due to code-layout/scheduling effects. The additional complexity was therefore **fully reverted**.

### 6.3 Unbounded/nested internal parallelism

Keeping segment-internal parallel verification inside already-parallel generation workers was measured and rejected for the large-document high-hit path because it materially increased latency through oversubscription.

## 7. Correctness and regression

Final tests:

- **148/148 PASS**
- `cargo fmt --check`: PASS
- `cargo clippy --locked --offline --all-targets -- -D warnings`: PASS
- standard `cargo build --release --locked --offline --bins --examples`: PASS
- release `pr_portable self-test`: **SELF_TEST_PASS**

Two relevant tests were added beyond the prior 146-test baseline:

1. high-hit bitmap materialization: all/dense/sparse behavior and exact order;
2. multi-generation high-hit range split with hidden old versions: replaced/tombstoned physical matches cannot leak through newest-wins filtering.

Existing generation tests also continue to cover newest upsert visibility, tombstones, multiple layers, first-N order, and oracle equivalence.

## 8. On-disk byte identity

No writer code or on-disk format changed.

A fresh 5k code-like segment generated by the final source has SHA-256:

```text
5cb9a9ca8df364b51ced6c6353f56d1b168896aaa434b06036d97f6d1069e43d
```

This is identical to the established pre-query-optimization segment oracle.

Result: **BYTE_IDENTITY_PASS**.

## 9. Outcome

The original requested targets are substantially reduced:

- newest-wins filtering for dense scan is fused into exact hit production;
- coordinator merge is reduced to ~microseconds of bitmap OR plus unavoidable worker wait;
- all-hit materialization is a direct stable `live_order` copy;
- partial dense materialization uses exact capacity;
- skewed compact segments are range-balanced across the bounded generation pool;
- large-document high-hit generation avoids nested verification pools.

The remaining high-hit wall time is now primarily actual per-worker exact search/verification and scheduler wait rather than generation merge or materialization.
