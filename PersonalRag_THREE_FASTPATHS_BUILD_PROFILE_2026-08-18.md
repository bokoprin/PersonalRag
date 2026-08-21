# PersonalRag Three Fast Paths + Build Stage Profile — 2026-08-18

## Scope

This change set starts from `PersonalRag_GUI_PortableCore_OfficeParallelCache_2026-08-17` and implements the three requested acceleration tracks before profiling the completed build pipeline:

1. Sort-aware adaptive first-N.
2. Regex mandatory-literal prefilter plus Office extraction-cache verification.
3. vNext parallel segment build plus adaptive shared hydration.

No `.prseg` / `.prseg2` format version was changed.

## 1. Sort-aware first-N

The generation search boundary now accepts an ordered logical-ID view plus row ranks. `PortableEngine` lazily caches sort orders by generation/document count and `(field, direction)` for:

- path (including descending),
- name,
- size,
- modified,
- extension.

Ascending/descending searches reuse the generation catalog and use vNext `first_n_in_order` / conjunctive first-N to stop when the GUI limit is satisfied. Perf12 uses the same ordering for exact-scan first-N, and rank-based Top-K when it chooses index-driven candidates.

The fast path is deliberately disabled when later filters could invalidate early hits: regex, match-case, whole-word, extension filters, or path scope.

### A/B

50k synthetic dense-hit corpus, `limit=2000`:

- legacy all-hit + Top-K: **2.719 ms**
- sort-aware first-N: **0.118 ms**
- speedup: **23.08x**

The benchmark exercises the same ordered-logical-ID search-core primitive used by the Bridge route.

## 2. Regex mandatory-literal prefilter + Office cache integration

Regex remains exact: the final result is still verified with the existing regex engine against the original search text.

A conservative `required_regex_literal_prefix` extracts only a prefix that can be proven mandatory. Examples:

- `error.*timeout` -> `error`
- `^Report_[0-9]+\.txt$` -> `Report_`
- alternation, leading wildcard, semantic escapes, and ambiguous optional prefixes -> no prefilter.

If a safe prefix exists, existing Perf12/vNext indexes generate candidates first. If no safe prefix can be proven, the old full-candidate path remains the fallback. Therefore the optimization must not introduce false negatives.

Office Open XML final verification now reads through `OfficeExtractionService`, so a valid Office extraction-cache object is reused instead of re-running ZIP/DEFLATE/XML extraction for regex/match-case/whole-word verification.

### A/B

50k synthetic regex-like corpus:

- full scan verification: **3.568 ms**
- mandatory literal prefilter + verification: **0.091 ms**
- speedup: **39.33x**
- candidates: **50,000 -> 500**
- final matches: **500**

This gain applies to regexes with a provably mandatory literal. Regexes without one intentionally remain on the correctness-first fallback.

## 3. vNext parallel segment build

Durable vNext base-component publication can write independent `.prseg2` segments concurrently while preserving deterministic segment numbering and manifest order.

Parallelism is adaptive because each segment writer already internally parallelizes content/path q1/q2/q3 generation:

- very small average documents (`<=256 B`) cap segment-level concurrency at 2,
- larger documents cap at 4,
- both remain bounded by available CPUs and segment count.

The durable byte-identical hard gate compares:

- normal slice initializer,
- streaming initializer with 1 worker,
- streaming initializer with 4 workers,

and requires every generated store file to be byte-identical.

Representative isolated segment-build measurements:

- 20k docs × 512 B: 1 worker 273.8 ms, 2 workers 165.0 ms, 4 workers 130.1 ms.
- 20k docs × 4 KiB: 1 worker 2037 ms, 2 workers 1053 ms, 4 workers 738 ms.

## 4. Shared hydration: final adaptive design

Several variants were measured rather than retaining the first implementation:

### Rejected: concurrent Perf12 + vNext streaming

Running Perf12 and vNext construction concurrently caused CPU/memory-bandwidth contention. At 20k × 4 KiB it was substantially slower than sequential construction, so it is **not** used by production.

### Rejected as universal path: observer clone capture

Cloning normalized content during hydration sometimes improved small corpora but was not stable and retained a second full byte copy.

### Adopted: no-copy retained hydration for small/medium corpora

Search Core now has `build_disk_path_inputs_index_unified_retained`. A completed Perf12 segment can return ownership of its already-normalized `DocumentInput`s in deterministic document-ID order. Bridge then moves `display_path` and `normalized_content` directly into vNext inputs; there is no post-build source reread, MergedIndex materialization, or full-content clone.

Retaining a large corpus during expensive Perf12 PRPOS construction increases memory/cache pressure, so production enables this path only when the estimated retained normalized corpus is **<=32 MiB**. Larger corpora use the proven Perf12 snapshot -> vNext fallback.

A/B examples:

- 10k × 512 B: retained path about **1.09x** faster than legacy median.
- 20k × 512 B: retained path **1.12x** faster.
- 20k × 4 KiB (~80 MiB text): retained path **0.95x** (regression), therefore production falls back instead of retaining.

A dedicated hard gate requires the normal unified builder and retained builder to generate byte-identical Perf12 output and verifies returned normalized documents are in logical-ID order.

## 5. Build-stage profile

The completed implementation was profiled with `PR_PROFILE_BUILD=1`. Instrumentation is dormant in normal operation and does not change index formats.

The profile separates:

- disk read / normalize,
- Perf12 base content/name gram/posting work,
- base write,
- q2,
- shared positional frontier,
- PRPOS1 / PRPOS2 / PRPOS3,
- positional sidecar writes,
- vNext layout,
- content q1/q2/q3,
- path q1/q2/q3,
- encode,
- checksum,
- durable write.

### 20k docs × 4 KiB

Overall isolated kernels:

- Perf12: **3360.3 ms**
- vNext: **723.1 ms**

Perf12 critical/slow segments:

- segment acceleration: up to **2941 ms**
- shared PRPOS frontier: up to **2539 ms**
- PRPOS1 serialization: up to **301 ms**
- q2: up to **12 ms**
- PRPOS2: up to **23 ms**
- PRPOS3: ~**0.1 ms**

The slowest PRPOS frontier alone is about **75.6% of total Perf12 wall time**, and roughly **86% of that segment's accelerator time**. This is the dominant build bottleneck.

Hydration is not the dominant Search Core cost:

- hydration wall: **61.3 ms**
- cumulative parallel reads: **314.8 ms**
- cumulative normalization: **12.3 ms**

vNext critical segment:

- content q3: up to **457 ms**
- total concurrent index group: up to **458 ms**
- checksum: about **27-29 ms**
- durable write: about **89-108 ms**
- total segment: up to **616 ms**

Because q1/q2/q3 workers overlap, `index_group_ms` is the wall-clock critical group; q3 is effectively the critical worker.

### 20k docs × 512 B

Overall isolated kernels:

- Perf12: **449.9 ms**
- vNext: **126.4 ms**

Perf12 remains frontier-bound:

- shared PRPOS frontier: **249-272 ms/segment**
- PRPOS1: ~**30-31 ms/segment**
- q2: **9-22 ms/segment**

vNext remains q3-bound:

- content q3: **55-69 ms/segment**
- concurrent index group: **57-75 ms/segment**

Thus the bottleneck order is stable across these two content sizes:

1. **Perf12 shared PRPOS frontier**
2. **vNext content q3 construction**
3. Perf12 PRPOS1 serialization / base write (depending corpus)
4. vNext durable write/checksum
5. hydration/normalization

## 6. Validation

Search Core final local gate before packaging:

- **122/122 tests PASS**
- `cargo fmt --check` PASS
- Clippy all targets with `-D warnings` PASS
- release examples/binaries build PASS
- `pr_portable self-test` -> `SELF_TEST_PASS`

New/extended hard gates include:

- streaming slice / 1-worker / 4-worker durable byte identity,
- retained hydration Perf12 byte identity,
- retained normalized-document logical order,
- Bridge sort-order semantics test,
- Bridge conservative regex-prefix test.

The Windows validation harness now explicitly runs the new Bridge sort and regex tests.

### Linux Bridge limitation

Full Bridge Cargo compilation is still blocked in this offline Linux environment before source compilation because the cached dependency set does not contain crate `ignore`. Modified Bridge Rust files pass Rust 2021 `rustfmt --check`/parse. This is not reported as a Bridge compile PASS; Windows native validation remains the hard gate.

## 7. Next optimization target selected by profile

Do **not** spend the next wave on hydration or generic q2 work. The data says the next target should be:

1. **PRPOS shared-frontier construction** — by far the largest Perf12 build cost.
2. **vNext content-q3 construction** — the vNext critical worker.
3. Only after those: sidecar/base durable-write and checksum bandwidth.

Any next optimization should A/B these stages directly with the included build-stage profiler rather than using total build time alone.
