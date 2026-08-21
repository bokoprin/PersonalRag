# PersonalRag SIMD Query Fast Path — 2026-08-18

## Scope

This wave starts from `PersonalRag_GUI_PortableCore_Win32Batch2FastPath_2026-08-18.zip` and changes only Search Core query execution plus one benchmark example. Segment writing, durable generation format, scanner behavior, and on-disk `.prseg2` layout are unchanged.

## Implemented fast paths

1. **Adaptive AVX2 `ExactMatcher::find_from`**
   - Existing long-literal BMH remains the default.
   - On x86_64, AVX2 is used only for needles up to 8 bytes with at least 64 bytes remaining.
   - The first BMH-safe candidate is checked before SIMD; high-hit matches at the current document start therefore keep the cheap old behavior.
   - AVX2 compares 32 candidate starts at once using first-byte and last-byte filters, then performs a full exact compare only for surviving lanes.
   - Non-x86_64 targets keep the scalar path.

2. **Encoding-aware q3 posting intersection**
   - Single-block content segments can intersect q3 postings directly because block ID and document ID have a one-to-one ordering and no cross-block alignment is required.
   - Multi-block content retains the old alignment-aware path unchanged.
   - Path search also uses direct posting intersections before exact verification.
   - Implemented combinations:
     - RawU16 × RawU16: monotonic intersection with an AVX2 16-lane membership kernel on x86_64.
     - RawU16 × DenseBitmap: direct bitmap membership, no repeated binary search.
     - DenseBitmap × DenseBitmap: AVX2 256-bit AND, zero-vector skip, and ordered 4×u64 set-bit enumeration on x86_64.
     - Singleton/Empty special cases.
     - A second extra anchor filters the already-sorted result without returning to generic posting binary searches.

3. **Opt-in query sub-profiler**
   - Enable with `PR_PROFILE_QUERY=1`.
   - `kind=content` reports anchor selection, extra-anchor sampling, intersection, verification, total time, candidate counts, and posting encoding mix.
   - `kind=dense_blob` reports `find_from_calls`, candidate positions examined, hits, and total time.
   - `kind=path` reports anchor/intersection/verification phases and posting encoding mix.
   - Normal execution avoids the detailed timers/counters.

## Correctness gates added

- SIMD-boundary/start-offset `find_from` versus a naive exact oracle.
- Random binary haystack/needle `find_from` versus the naive oracle.
- RawU16/RawU16 SIMD dispatch versus scalar merge and naive membership.
- DenseBitmap/DenseBitmap dispatch versus naive membership.
- Raw/Dense plus third-anchor direct intersection versus naive membership.
- All SIMD needle lengths 2..=8 across 32-byte boundaries, including profiled/non-profiled equivalence.
- RawU16 intersection over the upper half of the full u16 domain.
- DenseBitmap intersection including block ID 65535.

Existing vNext query and generation tests remain the semantic oracle for multi-block alignment, newest-wins generation semantics, ASCII folding, and high-hit execution.

## Release A/B results

All compared binaries were built from the same Rust 1.97.1 toolchain with `CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16`.

### Multi-anchor q3 benchmark

Existing `vnext_common_q3_bench`, 33,000 single-block documents, query `abcde`, 3 selected anchors, 11,000 primary blocks reduced to 1,000 candidates. Five alternating process runs, 101 in-process query rounds each:

- Baseline median p50: **0.234157 ms**
- SIMD/intersection median p50: **0.035532 ms**
- Speedup: **6.590x**
- Reduction: **84.83%**

The final DenseBitmap×DenseBitmap kernel skips all-zero 256-bit intersections before any store and enumerates non-zero vectors as four `u64` words. This was retained only after a staged A/B showed an additional ~7–8% improvement over the first SIMD dense/dense implementation.

### Scan-heavy `find_from` benchmark

`simd_query_fastpath_bench`, 20,000 live documents in four generation segments, 128-byte repeated-prefix documents, 4-byte high-hit query `aaab`. Five alternating process runs, 101 in-process query rounds each:

- Baseline median p50: **2.396741 ms**
- Adaptive AVX2 median p50: **0.230382 ms**
- Speedup: **10.403x**
- Reduction: **90.39%**
- Baseline median p95: **4.352779 ms**
- Adaptive AVX2 median p95: **0.373734 ms**

This corpus intentionally stresses the case where BMH has a one-byte skip distance. It is evidence for the SIMD kernel, not a claim that every query improves by 10x.

### 60k query-only suite

To remove build and filesystem-generation noise, baseline and candidate query binaries opened the **same prebuilt 60k-document `.prseg2`** and executed each query for 501 rounds. Seven alternating baseline/candidate processes were used; the table reports the median of each process p50.

| Query | Baseline p50 median | Candidate p50 median | Change |
|---|---:|---:|---:|
| q1 | 0.407244 ms | **0.374966 ms** | **-7.93%** |
| q2 | 0.409907 ms | **0.376097 ms** | **-8.25%** |
| common | 2.724076 ms | 2.720410 ms | -0.13% |
| medium | 0.071035 ms | 0.071105 ms | +0.10% |
| rare | 0.000851 ms | 0.000851 ms | unchanged |
| zero | 0.001042 ms | 0.001041 ms | -0.10% |
| Japanese | 0.017896 ms | 0.017656 ms | -1.34% |
| long | 0.001933 ms | 0.001933 ms | unchanged |
| filename/path | 0.010626 ms | **0.008152 ms** | **-23.28%** |

The 501-round medium-query p95 varied noticeably across processes. A dedicated **10,001-round × 7-process** confirmation removed that ambiguity:

- baseline medium p50/p95 medians: **0.071035 / 0.094229 ms**
- candidate medium p50/p95 medians: **0.069112 / 0.091695 ms**
- p50 reduction: **2.71%**
- p95 reduction: **2.69%**

The earlier profiler-scaffolding version caused a reproducible Japanese-query regression and a small q1 regression. That implementation was rejected. The final design uses const-generic profiled/non-profiled q3 paths and a dedicated q1/q2 short-query hot path, so normal queries do not pay the detailed profiler branch/timer cost.

### Long-needle generation high-hit guard

The first AVX2 implementation routed every `find_from` through the profiled SIMD dispatcher and regressed the 14-byte `timeout common` generation query by about 5%. That version was rejected. The final adaptive design keeps long needles on the original BMH path. Seven alternating process runs after the fix measured a median **0.445380 ms → 0.426962 ms**; the important acceptance result is that the earlier regression was removed.

## Safety and format compatibility

- No on-disk format changes.
- No writer/build algorithm changes.
- AVX2 functions are runtime-dispatched after `is_x86_feature_detected!("avx2")`.
- Unaligned SIMD loads are guarded by explicit range proofs.
- Multi-block q3 alignment retains the previous implementation.
- Non-x86_64 targets use scalar query paths in this wave; no unverified NEON implementation was added.

## Final validation

- Pre-change regression: **138/138 PASS**.
- Final regression: **146/146 PASS** (the original 138 plus 8 SIMD/query-specific tests).
- `cargo fmt --check`: PASS.
- `cargo clippy --locked --offline --all-targets -- -D warnings`: PASS.
- `CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 cargo build --release --locked --offline --examples --bins`: PASS.
- Release `pr_portable self-test`: `SELF_TEST_PASS`.
- Deterministic 5k q3 build remains byte-identical to the pre-change writer: SHA-256 `5cb9a9ca8df364b51ced6c6353f56d1b168896aaa434b06036d97f6d1069e43d`.
- No git commit was created.

The production fast path is AVX2 on x86_64 with runtime feature detection. Non-x86_64 targets keep the scalar query implementation in this wave; an unverified NEON path was deliberately not added.
