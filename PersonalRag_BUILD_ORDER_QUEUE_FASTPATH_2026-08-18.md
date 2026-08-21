# PersonalRag Build-Order + Native Directory Queue Fast Path

Date: 2026-08-18

Baseline: `PersonalRag_GUI_PortableCore_Win32ScannerHotPath_2026-08-18.zip`

Baseline SHA-256:

`0b716dbf79d9a9079896395b00a21598631c55525c5177ccb674ebb4b1949ac7`

## Scope

This wave continues the Windows full-build acceleration work after the Win32 batch scanner and scanner hot-path passes.

Two changes are adopted:

1. deterministic full-build path ordering uses a bounded stable parallel sort/merge for large scan results;
2. the Win32 native scanner claims/completes directory work in adaptive batches when the shared queue is deep.

An experimental Search Core hydration work-claim/preallocation path was also implemented and measured, but it did not improve full-build wall clock reliably. It was completely reverted. Search Core source is byte-identical to the baseline source in the final candidate.

No persistent index format, query semantics, generation semantics, USN journal semantics, exclusion semantics, or WalkDir fallback semantics are changed.

## 1. Deterministic build-order parallel sort

Before a full build, scanner results are sorted by `ScannedFile.path` so document order is deterministic. The previous implementation used a single-threaded stable `Vec::sort_by` regardless of file count.

The new `bridge-core/src/build_order.rs` keeps the old path for small sets and uses a bounded stable parallel implementation for large sets:

- fewer than 50,000 files: original single-thread stable sort;
- large inputs: up to 8 workers, also limited by `available_parallelism` and work size;
- input records are moved into chunks; `PathBuf` / `String` payloads are not cloned for the sort;
- each chunk is stable-sorted independently;
- chunks are pairwise stable-merged;
- equality chooses the left input, preserving the same duplicate-path stability as `Vec::sort_by`;
- worker panic is converted to a normal build error instead of escaping as an unwind.

Optional profile output:

`PR_PROFILE_BUILD_ORDER=1`

prints:

`BUILD_ORDER_SORT files=<N> workers=<N> elapsed_ms=<ms>`

### Synthetic CPU-side A/B

Exact production helper, randomized Windows-like paths, alternating baseline/candidate order, 7 runs each, median:

| Files | Workers | Stable single-thread | Parallel stable | Speedup |
|---:|---:|---:|---:|---:|
| 50,000 | 1 | 36.139 ms | 34.593 ms | 1.045x |
| 100,000 | 2 | 82.500 ms | 58.551 ms | 1.409x |
| 300,000 | 4 | 388.393 ms | 184.354 ms | 2.107x |

Every run asserts that the candidate output is exactly equal to the standard stable sort result.

These are Linux synthetic CPU-side measurements of the deterministic sort stage, not a claim about Windows end-to-end full-build speed.

## 2. Adaptive native directory queue batching

The Win32 scanner previously acquired the shared directory queue once to pop one directory and once again to complete that directory. On very large trees this makes queue synchronization proportional to the directory count.

The new queue API can claim multiple directories per lock acquisition:

- maximum claim: 4 directories;
- when the queue is shallow, claim remains 1 to preserve load balance;
- when the queue is deep, claim grows based on `queued / (workers * 2)`, capped at 4;
- claimed work is completed in one `complete_many` operation;
- pending-directory accounting and child-directory enqueue accounting remain unchanged;
- cancellation/fatal-stop behavior intentionally discards unprocessed claimed tasks because the scan is already terminating.

The adaptive policy replaced an earlier fixed-4 experiment after review identified a load-balance risk when one worker could retain multiple expensive directories while another worker became idle.

### Synthetic queue-only A/B

5 worker threads, alternating order, 9 runs, median; this measures only queue synchronization/claim overhead and does not call Win32 filesystem APIs:

| Directory tasks | One-at-a-time | Adaptive 1..4 | Speedup |
|---:|---:|---:|---:|
| 100,000 | 2.096 ms | 1.039 ms | 2.017x |
| 2,000,000 | 36.506 ms | 13.301 ms | 2.745x |

These numbers are mechanism-level evidence only. Native Windows end-to-end scanner improvement still requires a Windows filesystem A/B.

## Rejected experiment: Search Core hydration batching

This wave also prototyped batching of hydration work claims and known-size file-read allocation changes. The local hydration kernel could improve in some cases, but full-build wall-clock results were not stable across 512-byte and 4-KiB corpora. The experiment was therefore removed from the final source rather than increasing complexity for a local-only win.

Final Search Core source comparison against the baseline, excluding build artifacts:

`SEARCH_CORE_SOURCE_BYTE_IDENTICAL=YES`

## Correctness and regression

Pre-change baseline gate:

- Search Core: 138/138 PASS;
- fmt: PASS;
- clippy all targets with `-D warnings`: PASS;
- CGU16 release examples/bins: PASS;
- `pr_portable self-test`: `SELF_TEST_PASS`.

Final candidate gate:

- Search Core: 138/138 PASS;
- fmt: PASS;
- clippy all targets with `-D warnings`: PASS;
- CGU16 release examples/bins: PASS;
- `pr_portable self-test`: `SELF_TEST_PASS`;
- build-order exact-source tests: 2/2 PASS;
- native scanner parser/semantic/queue tests: 5/5 PASS;
- Windows scanner cfg-lift `rustc -D warnings`: PASS;
- Windows scanner cfg-lift clippy `-D warnings`: PASS;
- changed Bridge Rust files `rustfmt --check`: PASS.

Bridge Cargo as a whole is still blocked in this Linux offline environment before source compilation because the local crates.io cache does not contain `ignore`. The same block existed before this wave and is not caused by these changes.

## Review

Review loop 1 found that a fixed four-directory queue claim could reduce load balance on irregular/deep directory trees. The queue claim was changed to the final adaptive 1..4 policy and the scanner source-lift/cfg-lift gates were rerun.

No additional functional issue was found in the final review.

## Changed production source

- `bridge-core/src/build_order.rs` (new)
- `bridge-core/src/lib.rs`
- `bridge-core/src/engine.rs`
- `bridge-core/src/windows_native_scanner.rs`

Search Core production source is unchanged.

## Remaining hard gate

The Linux environment cannot execute the actual Win32 directory enumerator. The existing `scripts/benchmark-windows-native-scanner.ps1` remains the Windows native scanner A/B gate. A real large-root full-build run with `PR_PROFILE_BUILD_ORDER=1` should also be used to quantify the build-order stage on the target laptop.

## Fresh distribution validation

The completed distribution ZIP was extracted into a new directory and validated again:

- packaged changed source files are byte-identical to the already reviewed/validated candidate source;
- Search Core: 138/138 PASS;
- Search Core fmt: PASS;
- Search Core clippy all targets with `-D warnings`: PASS;
- CGU16 release examples/bins: PASS;
- release `pr_portable self-test`: `SELF_TEST_PASS`;
- changed Bridge Rust `rustfmt --check`: PASS;
- Bridge offline Cargo check reproduces the same pre-source missing-`ignore` block;
- distribution ZIP contains no `target/` directory and no validation `.cargo/` directory.
