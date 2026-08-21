# PersonalRag System Bottleneck Fast Path Report

Date: 2026-08-18

Baseline source:
`PersonalRag_GUI_PortableCore_HighHitGenerationFastPath_2026-08-18.zip`

Baseline SHA-256:
`8b056a4099fde7404c0ef445d78b40b7aa69c229c41bfa2fce85f2ef70c3ef43`

## 1. Goal

Measure the current PersonalRag system end-to-end rather than choosing another local optimization in advance, identify the largest wall-time bottleneck, and retain only an optimization that improves whole-system wall time while preserving exact search semantics.

Measured pipeline coverage:

- filesystem content hydration;
- Office Open XML extraction/cache path;
- Perf12 base/index construction and accelerators;
- vNext segment construction;
- generation finalize/open;
- exact query latency.

The Linux validation host cannot execute the Win32 native scanner kernel APIs. Windows scanner CPU-side work was already optimized in the preceding waves, so the current system-wide profiling treats Win32 enumeration as a separate Windows acceptance gate rather than inventing a Linux number for it.

## 2. Pre-change safety gate

Before optimization:

- Search Core canonical regression: 148/148 PASS
- `cargo fmt --check`: PASS
- Clippy `-D warnings`: PASS
- release bins/examples: PASS
- `pr_portable self-test`: `SELF_TEST_PASS`

Bridge Cargo is not fully buildable in this container because the offline crates.io cache does not contain `ignore`, and online access cannot resolve `index.crates.io`. This block occurs before Bridge source compilation and is unchanged from prior waves.

## 3. Whole-system bottleneck measurement

### 3.1 20k normal text/code files

Current Full-acceleration baseline, 20,000 files:

| Payload | Hydration wall | Perf12 build | vNext build | Perf12+vNext kernel |
|---|---:|---:|---:|---:|
| 512 B | 34.308 ms | 377.616 ms | 100.576 ms | 478.192 ms |
| 4096 B | 52.845 ms | 2664.526 ms | 392.559 ms | 3057.084 ms |

At 4 KiB per file, the four 5k-document Perf12 segments each spent approximately 1.92-1.99 seconds in the shared PRPOS frontier. Accelerator wall per segment was approximately 2.26-2.27 seconds.

Representative per-segment PRPOS frontier times:

- 1918.634 ms
- 1932.603 ms
- 1993.064 ms
- 1932.800 ms

Therefore the dominant initial-build bottleneck was not hydration, Office extraction, or vNext construction. It was production Perf12 `AccelerationProfile::Full`, specifically the PRPOS001/002/003 positional frontier.

### 3.2 Office path

Representative current Office cold-cache measurements for 200 files showed roughly 61-80 ms for common DOCX/XLSX/PPTX cases on this host, with multipart cases roughly 68-80 ms. This can still dominate an Office-heavy root, but it was materially below the 2.66-second Perf12 Full build wall for the representative 20k×4KiB text/code corpus.

### 3.3 Query path

The previous SIMD and high-hit waves have already reduced query-side merge/materialization costs to sub-millisecond territory for the representative suites. Consequently it was not rational to spend another wave on query micro-kernels before eliminating the multi-second initial-build accelerator tax.

## 4. PRPOS micro-optimization experiments rejected

Before changing the production acceleration policy, several direct PRPOS optimizations were implemented and A/B measured.

### 4.1 Posting generation + child redistribution fusion

Rejected.

5k×4KiB, 7 alternating runs:

- frontier finish: about 1.1% worse;
- Perf12 wall: about 1.5% worse.

### 4.2 Dense-q3 rank + direct q4 slot table

Rejected.

- q4 seed: about 2.2% worse;
- whole Perf12 improvement only about 1.3% and not robust.

### 4.3 u32-only frontier occurrence storage

Rejected.

- seed: about 4.4% worse;
- finish: about 2.2% better;
- whole Perf12 improvement only about 0.8%.

### 4.4 Separate single-child-chain detection scan

Rejected.

- frontier finish: 6.8% worse;
- Perf12 wall: 5.4% worse.

The extra read pass cost more than the avoided copies.

### 4.5 Uniform-child detection fused into posting scan

Rejected.

- frontier finish: 7.2% worse;
- Perf12 wall: about 1.0% worse.

### 4.6 Nested frontier oversubscription hypothesis

Rejected as a diagnosis after source audit. Frontier workers are already budgeted as approximately `logical_cpus / build_workers`; the 20k / four-segment case uses one frontier worker per segment on the 5-vCPU host.

### 4.7 Q2 + PRPOS001-only Balanced candidate

Rejected.

Removing only PRPOS002/003 still left PRPOS001 expensive enough that the 20k×4KiB default Perf12 pipeline improved by about 61%, versus 83% with the final Q2-only Balanced profile. PRPOS001 alone added roughly 0.8 seconds on the measured workload.

## 5. Adopted optimization: production Balanced acceleration

A new Search Core acceleration mode was added:

`AccelerationProfile::Balanced`

Balanced builds:

- exact PRSEG base: yes;
- compact Q2 accelerator: yes;
- PRPOS001: no;
- PRPOS002: no;
- PRPOS003: no.

This is a performance-policy change, not a search-semantics change. Missing positional sidecars are already an exact supported fallback path.

`AccelerationProfile::Full` remains available and unchanged for explicit benchmarking, compatibility tests, or future opt-in use.

Production wiring:

- Bridge initial full build: Balanced for Perf12, Shadow, and vNext fallback base;
- unified base compaction: Balanced;
- incremental delta path: existing `AdaptiveDelta` remains unchanged;
- Full PRPOS verification guard remains in Bridge if the production profile is ever switched back to Full.

The production profile is centralized as one Bridge constant to avoid future drift between the three initial-build branches.

## 6. Whole-system A/B

### 6.1 Current GUI-default Perf12 path

5 alternating/rotating rounds, 20,000 files.

| Payload | Full total | Balanced total | Speedup | Wall reduction |
|---|---:|---:|---:|---:|
| 512 B | 323.666 ms | 141.942 ms | 2.28x | **56.15%** |
| 4096 B | 2638.718 ms | 445.915 ms | 5.92x | **83.10%** |

4KiB stage medians:

- Full Perf12 build: 2516.254 ms
- Balanced Perf12 build: 319.717 ms
- generation finalize: 127.730 -> 118.631 ms
- open/query: 3.585 -> 6.999 ms

The extra few milliseconds at open/query are insignificant compared with the ~2.2-second build reduction and are separately covered by the repeated query benchmark below.

### 6.2 Pipeline including vNext construction

This models the Shadow/vNext path where the exact Perf12 base is followed by vNext materialization/build.

| Payload | Full total | Balanced total | Speedup | Wall reduction |
|---|---:|---:|---:|---:|
| 512 B | 502.139 ms | 284.547 ms | 1.77x | **43.33%** |
| 4096 B | 2898.152 ms | 928.275 ms | 3.12x | **67.97%** |

After this optimization, vNext construction (~409 ms in the 4KiB run) is now comparable to or larger than the Balanced Perf12 build (~321 ms), so the bottleneck has genuinely moved.

### 6.3 Index footprint

For the same 20k×4KiB query benchmark corpus:

- Full index directory: ~202.7 MB
- Balanced: ~108.5 MB

Reduction: approximately **46.5%**.

## 7. Query trade-off check

The exact result vectors were asserted identical between Full and Balanced.

Five query-only processes were run with Full/Balanced execution order alternated. Median p50 differences on the 20k×4KiB corpus:

| Query | Full p50 | Balanced p50 | Balanced delta |
|---|---:|---:|---:|
| q1 | 0.135350 ms | 0.137254 ms | +1.41% |
| q2 | 0.744593 ms | 0.713888 ms | -4.12% |
| common | 0.885442 ms | 0.888733 ms | +0.37% |
| medium | 0.044286 ms | 0.043835 ms | -1.02% |
| rare | 0.007190 ms | 0.007180 ms | -0.14% |
| zero | 0.007721 ms | 0.007711 ms | -0.13% |
| Japanese | 0.013230 ms | 0.013149 ms | -0.61% |
| long | 0.007932 ms | 0.007852 ms | -1.01% |

p50 is effectively unchanged. Some p95 samples are noisier without PRPOS; the largest observed relative increases were q1/common/medium, but the absolute changes were approximately 0.03 ms, 0.19 ms, and 0.019 ms respectively. This trade-off is explicitly retained rather than hidden: Balanced prioritizes the multi-second full-build bottleneck while keeping exact query results and sub-millisecond representative p50 latency.

## 8. Profiling/tooling retained

New/retained benchmark tooling:

- `search-core/examples/system_full_build_bench.rs`
  - Full/Balanced/None whole-pipeline comparison;
  - optional vNext stage through `PR_BENCH_VNEXT=0|1`.
- `search-core/examples/acceleration_profile_tradeoff_bench.rs`
  - Full/Balanced/AdaptiveDelta/None build/query Pareto comparison;
  - payload sizing and built-index reuse for query-only A/B.
- `PR_PROFILE_FRONTIER=1`
  - exposes q4 seed vs frontier finish wall without changing normal execution semantics.

## 9. Correctness and final validation

Final source:

- canonical Search Core regression: **149/149 PASS**;
- all-target including repeated extractor tests embedded in benchmark examples: **163/163 PASS**;
- `cargo fmt --check`: PASS;
- Clippy all-targets `-D warnings`: PASS;
- release bins/examples: PASS;
- release `pr_portable self-test`: `SELF_TEST_PASS`;
- explicit Balanced-vs-Full exact query equivalence test: PASS;
- Balanced Q2 sidecar exists: PASS;
- Balanced PRPOS sidecars absent: PASS;
- unified AdaptiveDelta + Balanced compaction generation semantics: PASS.

vNext writer/on-disk format was not modified. A freshly generated 5k code-like `.prseg2` remains byte-identical to the established oracle:

`5cb9a9ca8df364b51ced6c6353f56d1b168896aaa434b06036d97f6d1069e43d`

Bridge validation in this Linux environment:

- `rustfmt --check bridge-core/src/engine.rs`: PASS;
- source contract: production profile centralized to Balanced, used by all three initial full-build branches, Full verification guard preserved, unified compaction Balanced: PASS;
- `cargo test --locked --offline`: BLOCKED before source compilation because `ignore` is missing from the offline crates.io cache;
- online retry: BLOCKED by DNS/network access to crates.io.

## 10. New bottleneck after the optimization

The original bottleneck has moved.

For the GUI-default Perf12 path at 20k×4KiB:

- Balanced Perf12 build: ~320 ms;
- generation finalize: ~119 ms;
- query open/sample query: ~7 ms.

For a pipeline that also builds vNext:

- Balanced Perf12 build: ~321 ms;
- vNext build: ~409 ms.

Therefore the next system-wide optimization target should be selected by production mode:

- Perf12 default: base/Q2 build and generation finalize;
- Shadow/vNext: vNext build is now the largest measured stage;
- Office-heavy roots: Office extraction/cache-miss work can become comparable after PRPOS is removed;
- Windows scan-heavy roots: real Win32 scanner wall must still be measured on Windows hardware.

## 11. Outcome

The request was to measure first and optimize the measured bottleneck rather than continue local micro-optimization. The measurement identified Full PRPOS acceleration as the dominant initial-build tax. Direct PRPOS micro-optimizations did not win. Replacing the production acceleration policy with exact Q2-only Balanced mode produced the largest stable whole-system improvement observed in this wave while preserving exact results and the existing vNext on-disk oracle.

Phase: System-wide bottleneck optimization / Balanced acceleration

Progress: 100%
