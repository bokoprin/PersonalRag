# PersonalRag Segment Format vNext — Common q3 Multi-Anchor Planner A/B Report

Date: 2026-08-17  
Base: `PersonalRag_GUI_PortableCore_SegmentVNext_PersistentIndex_AB_2026-08-17`  
Scope: Search Core / Linux  
Production switch: **HOLD**

## 1. Purpose

The PersistentIndex pass removed q1/q2 and filename/path scan fallbacks, but the text-heavy 20k common content query (`timeout`) remained substantially slower than Perf12 because one q3 anchor could leave almost every block for exact verification.

This pass adds a bounded second/third-anchor planner for common q3 queries without changing `.prseg2` serialization.

## 2. Planner design

For q3+ content queries:

1. Deduplicate query trigrams and read posting cardinalities.
2. Preserve exact zero-hit on any absent trigram.
3. Choose the rarest trigram as the primary anchor.
4. Only consider multi-anchor filtering when the primary posting is common:
   - at least 64 blocks, and
   - at least `block_count / 8` blocks.
5. Evenly sample up to 64 blocks across the entire primary posting.
6. Consider only the second and third rarest distinct trigrams.
7. Adopt an extra anchor only when the sample candidate count falls by at least 12.5%.
8. Intersect the complete primary posting against each selected anchor.
9. Exact-verify only the surviving primary blocks.

The sample is evenly distributed across posting ordinal space. An earlier first-N sample was measured during development and rejected because sorted block IDs made it corpus-order biased.

## 3. Boundary-safe block intersection

A direct `primary_block_id == secondary_block_id` intersection is incorrect because query trigrams at different offsets may be owned by adjacent blocks.

For each primary candidate block, the planner computes the possible owner-block range of each secondary trigram from:

- primary query offset
- secondary query offset
- block size
- primary block byte range
- document block range

It then tests posting membership only within that safe same-document range. This over-approximates possible alignment when necessary, so it may keep false positives but cannot drop a valid boundary-spanning hit.

`VNextQ3Posting::contains(u16)` was added with encoding-specific lookup:

- Empty: false
- Singleton: direct equality
- RawU16: binary search
- DenseBitmap: O(1) bit lookup

## 4. Common high-hit protection

Extra anchors are not forced merely because the primary posting is large.

A 128-document `timeout` test where every document is a real hit confirms:

- selected anchors = 1
- candidate blocks = primary anchor blocks

This avoids paying full multi-anchor intersection when it cannot reduce exact verification.

## 5. One-block exact-verification fast path

The text-heavy synthetic 20k corpus has one vNext block per document. The prior exact path repeatedly reloaded document block metadata even though the anchor block already contained the whole document.

For one-block documents, exact verification now directly scans the already-addressed block content. Multi-block documents keep the boundary-safe existing path.

This is a query-only optimization and does not alter the segment format.

## 6. Correctness

Final source regression:

- Search Core unit: 5/5 PASS
- Production tests: 35/35 PASS
- Persistent-index tests: 3/3 PASS
- vNext query tests: 8/8 PASS
- vNext segment tests: 17/17 PASS
- Doc tests: PASS
- `cargo fmt --check`: PASS
- `cargo clippy --all-targets --locked --offline -- -D warnings`: PASS
- Release `pr_portable`: PASS
- `SELF_TEST_PASS`: PASS

New/strengthened coverage includes:

- second + third anchors are both selected on false-positive-heavy common postings
- primary 110 blocks -> 10 candidate blocks in the boundary test
- exact hit begins at byte 7 with block size 8, so primary `abc` is in block 0 while `bcd`/`cde` are in block 1
- no boundary false negative
- useless extra anchors are skipped for 128/128 true-hit `timeout`
- `contains()` matches expected behavior for Singleton, RawU16 and DenseBitmap postings
- existing randomized substring oracle remains PASS

## 7. A/B: false-positive-heavy common q3

Controlled 33,000-document corpus, query `abcde`, 1,000 exact hits. All three query trigrams are common individually, but their co-occurrence is selective.

Three alternating old/new runs, p50 query latency:

| Pair | Persistent single-anchor | Multi-anchor final |
|---|---:|---:|
| 1 | 0.771905 ms | 0.311905 ms |
| 2 | 0.767688 ms | 0.310281 ms |
| 3 | 0.769832 ms | 0.310661 ms |

Median:

- old: **0.769832 ms**
- new: **0.310661 ms**
- speedup: **~2.48x**
- latency reduction: **~59.6%**

Final diagnostics:

- primary anchor blocks: 11,000
- selected anchors: 3
- candidate blocks after intersection: 1,000
- exact verified blocks: 1,000
- hits: 1,000

## 8. A/B: real text-heavy 20k `timeout`

`timeout` is a different shape: almost every primary candidate is a true hit.

Three alternating PersistentIndex-old / final runs:

| Pair | old vNext | final vNext |
|---|---:|---:|
| 1 | 2.386831 ms | 1.619874 ms |
| 2 | 2.460162 ms | 1.642315 ms |
| 3 | 2.512342 ms | 1.774802 ms |

Median:

- old vNext: **2.460162 ms**
- final vNext: **1.642315 ms**
- improvement: **~1.50x**
- latency reduction: **~33.2%**

Planner diagnostics remain:

- anchor blocks: 19,674
- selected anchors: 1
- candidate blocks: 19,674
- verified blocks: 19,674
- hits: 19,672

The planner correctly rejects 2nd/3rd anchors here because they cannot meaningfully reduce candidates. The improvement comes primarily from the one-block exact-verification fast path.

In the paired final runs, Perf12 common-query p50 median was approximately **0.703 ms**, so vNext is still about **2.34x slower** on this extreme high-hit common query.

## 9. Serialization/build impact

No `.prseg2` format change was made.

The same deterministic text-heavy 20k corpus produced byte-identical segment files before and after this pass:

```text
SHA-256 84533f3161b39e3aae378a9ac8012dff667710548cf4f3d1705380c658823819
PRSEG2_BYTE_IDENTICAL
```

Therefore this pass adds no new persistent-index bytes and no new writer work.

## 10. Decision

**Keep the multi-anchor planner.**

It materially improves the intended false-positive-heavy common-q3 case, preserves block-boundary correctness, and dynamically refuses useless intersections.

However production adoption remains **HOLD** because:

1. the extreme high-hit `timeout` query is still slower than Perf12 (~2.34x in the paired final sample),
2. the previous PersistentIndex report's text-heavy build regression is unchanged by this query-only pass,
3. selective filename/path query regression remains a separate unresolved item.

Recommended next performance work before production adoption:

- profile/optimize the high-hit q3 exact-verification/output path further,
- optimize selective filename/path planner,
- reduce persistent q1/q2 build cost without reintroducing the already-rejected fused q1/q2-in-q3 hot loop.
