# PersonalRag Segment vNext Pre-Production Optimization Report

Date: 2026-08-17

## Scope

This pass addresses the three remaining performance blockers identified after the persistent-index and common-q3 planner work:

1. High-true-hit common content queries such as `timeout`
2. Selective filename/path queries
3. Persistent-index build cost

Perf12 remains the production/correctness oracle. No production switch is performed in this pass.

## Baseline

Input source for this pass:

`PersonalRag_GUI_PortableCore_SegmentVNext_CommonQ3Planner_2026-08-17.zip`

The vNext on-disk format remains `PRSEG2A4`. No section layout, magic, or format version change was made.

## 1. High-true-hit common q3

### Problem

For `timeout` on the deterministic text-heavy 20k corpus:

- primary q3 candidates: 19,674 blocks
- exact hits: 19,672 documents

Because almost every candidate is a true hit, 2nd/3rd q3 anchors cannot meaningfully reduce the candidate set.

### Changes

- Keep the multi-anchor planner only when sampling predicts useful candidate reduction.
- Add an `all_docs_single_block` segment-shape property computed once at mmap open.
- Add a single-block verification fast path that borrows the block table/content blob once.
- Avoid repeated document/block metadata retrieval, hit bitmap allocation, and sort/dedup for one-block segments.
- When the candidate count is at least 8,192 blocks, split exact verification across up to four workers.
- Add an explicit 8,200-document high-hit test to exercise the parallel path against the naive oracle.

### A/B

Same environment, 20k corpus, 31 query rounds:

Before this pass:

- vNext `timeout` p50: 1.756312 ms
- Perf12 p50 in that run: 0.794563 ms

After this pass:

- vNext `timeout` p50: 0.716594 ms
- Perf12 p50 in that run: 0.680403 ms

Result:

- vNext itself improved about 2.45x / 59.2% lower latency.
- Remaining p50 gap to Perf12 is about 5.3%.

This blocker is considered resolved to near-parity for the measured high-hit workload.

## 2. Selective filename/path

### Problem

The previous path planner paid repeated q3 dictionary/rank lookup cost and exact-verified hundreds to thousands of paths even for selective queries.

### Changes

- At mmap open, materialize a compact sorted RAM cache of `(q3 key, cardinality)` for path q3.
- Use binary search over that cache to rank every distinct query trigram without decoding every posting.
- Decode only the rarest three path q3 postings.
- Intersect those postings by document ID before exact path verification.
- Keep ASCII-fold semantics identical to Perf12.
- Fast-path already-lowercase path bytes through the shared `ExactMatcher`; only fall back to folded comparison when uppercase ASCII is present.

The on-disk `.prseg2` format is unchanged.

### 100k filename-heavy A/B

51 query rounds:

| Query | Before vNext p50 | Final vNext p50 | Perf12 p50 | Final assessment |
|---|---:|---:|---:|---|
| `component_00042` (12 hits) | 0.194611 ms | 0.037932 ms | 0.059332 ms | vNext ~1.56x faster than Perf12 |
| `group_0042` (25 hits) | 0.183626 ms | 0.047124 ms | 0.042640 ms | within ~10.5% |
| long 1-hit | 0.043512 ms | 0.021626 ms | 0.014191 ms | ~7.4 us absolute gap remains |
| `png` (100k hits) | 0.324505 ms | 0.324436 ms | 0.567537 ms | vNext ~1.75x faster |
| zero-hit | 0.000111 ms | 0.000121 ms | 0.000176 ms | vNext faster |

Selective filename/path is no longer a large regression. The worst measured remaining selective gap is small in absolute latency.

## 3. Persistent-index build cost

### Rejected experiments

The following were implemented and measured, then reverted because they regressed performance:

- Combine content q1 and q2 into one worker/one pass: reduced scan count but lost parallelism.
- Replace the q1/q2 byte loop with a block-loop variant: slightly slower in paired measurements.
- Put q1/q2 stamp work directly into the q3 hot loop: slowed the critical q3 path.

These changes are not present in the final source.

### Adopted optimizations

#### 3.1 Remove redundant release-time fixed-index self-validation

`encode_fixed_index()` previously rebuilt an index and then immediately reread/validated the complete encoded structure in release builds.

Final behavior:

- Debug/tests still assert encoder output validation.
- `VNextSegmentReader::open()` still fully validates persistent indexes for all on-disk input.
- Release writer no longer pays the redundant full reread after encoding.

#### 3.2 Streaming `.prseg2` writer

Previous writer:

1. Build all section Vecs
2. Compute section checksums
3. Allocate a full ~50 MB file Vec
4. Copy every section into it
5. Hash the full assembled file again
6. Write the full Vec
7. `sync_all`
8. Rename

Final writer:

1. Build section Vecs
2. Compute section checksums in parallel
3. Build only the small header/section-directory prefix
4. Stream prefix/sections/alignment padding directly to the temporary file
5. Update whole-file FNV checksum incrementally while writing
6. Emit footer
7. `sync_all`
8. Rename

The full-file assembly allocation and full-file section copy are eliminated.

#### 3.3 Remove per-trigram block-ID division

The q3 builder previously evaluated `start / block_size` for every trigram start.

The final builder iterates by owner block and assigns a fixed local block ID to all valid q3 starts inside that block. This preserves the existing block-boundary look-ahead semantics while removing division from the inner q3 loop.

### Byte compatibility

The old writer/build path and final path generated a deterministic 20k `.prseg2` with exactly the same:

- file size: 50,385,256 bytes
- SHA-256: `05e1ea3775615d56863d5a1a15498f07d315bff7e6b03b898706d214d2efe0b7`

`cmp` result: byte-identical.

Therefore these optimizations do not change the v4 on-disk format or index semantics.

### text-heavy 20k build A/B

Seven alternating Perf12/vNext runs in the same environment:

- Perf12 median: 1,088.548 ms
- final vNext median: 1,120.533 ms

Final vNext is only about 2.9% slower in build wall time.

Compared with the start-of-pass CommonQ3Planner vNext median (~1,359.931 ms in the same environment), the final vNext is about 17.6% faster.

Resources from representative `/usr/bin/time -v` runs:

- Perf12 peak RSS: 301,740 KiB
- vNext peak RSS: 197,600 KiB
- RSS reduction: about 34.5%

Index size:

- Perf12: 76,733,278 bytes
- vNext: 50,385,256 bytes
- reduction: about 34.3%

### filename-heavy 100k build

Final same-run result:

- Perf12: 486.709 ms, 58,777,904 bytes
- vNext: 272.501 ms, 19,186,856 bytes

Result:

- vNext build about 1.79x faster
- index about 67.4% smaller

## 4. Final query snapshot

text-heavy 20k, 31 rounds:

| Query | Perf12 p50 | vNext p50 |
|---|---:|---:|
| q1 | 0.309975 ms | 0.162994 ms |
| q2 | 0.635501 ms | 0.163748 ms |
| common `timeout` | 0.680403 ms | 0.716594 ms |
| medium | 0.036905 ms | 0.025842 ms |
| rare | 0.008665 ms | 0.001166 ms |
| zero-hit | 0.009201 ms | 0.001375 ms |
| Japanese | 0.016622 ms | 0.008077 ms |
| long substring | 0.009454 ms | 0.002446 ms |
| filename sample (67 hits) | 0.003860 ms | 0.004664 ms |

For the tested content-query set, vNext is now faster in every category except the extreme high-hit `timeout`, where it is within about 5%.

## 5. Correctness and quality gates

Final working tree:

- Search Core unit: 5/5 PASS
- Production oracle: 35/35 PASS
- Persistent index: 3/3 PASS
- vNext query: 9/9 PASS
- vNext segment: 17/17 PASS
- doc tests: PASS
- `cargo fmt --check`: PASS
- `cargo clippy --all-targets --locked --offline -- -D warnings`: PASS
- release `pr_portable`: PASS
- `SELF_TEST_PASS`

The query tests include:

- randomized substring oracle
- block-boundary correctness
- multi-anchor correctness
- no-benefit multi-anchor guard
- 8,200-document high-hit parallel verification against the naive oracle
- Japanese/Unicode cases
- Perf12/naive/vNext result equality

## 6. Production decision

The three performance blockers targeted by this pass are substantially resolved:

- high-true-hit common q3: near Perf12 parity
- selective filename/path: large regression removed; mostly parity or faster
- persistent-index build: text-heavy near parity and filename-heavy significantly faster, with materially lower RSS/index size

However, do **not** switch production to vNext yet.

The original production hard gate also requires generation/delta/merge semantics. The current vNext prototype is still primarily a standalone segment/query implementation; Perf12's Generation/Delta/Streaming Compaction integration remains the production oracle.

Recommended next phase:

1. Integrate vNext segment output with Generation/Delta semantics in parallel with Perf12.
2. Verify incremental changes for 1 / 10 / 100 / 1000 changed documents.
3. Verify last-write-wins, tombstones, generation merge, compaction, corruption/fail-closed behavior.
4. Run final Gate 5 A/B across build/query/RSS/index/open/delta.
5. Only then decide production switch.

## 7. Source-change policy

Perf12 production code remains available as the oracle. No git commit or push was performed.
