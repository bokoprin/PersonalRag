# PersonalRag Segment Format vNext — Persistent q1/q2 + filename/path Index A/B Report

Date: 2026-08-17  
Base: Segment vNext Gate4  
Scope: Search Core / Linux  
Production switch: **HOLD (not adopted yet)**

## 1. Purpose

Gate4 proved the block-level q3 query prototype, but q1/q2 content queries and filename/path queries still used scan fallbacks. Before the production adoption decision, this pass adds persistent indexes for those paths and repeats the A/B against Perf12.

Perf12 remains the production/correctness oracle. vNext remains a parallel prototype.

## 2. Implemented persistent indexes

`.prseg2` was extended from format v3 to v4 (`PRSEG2A4`). The new sections are:

- Content q1 fixed-key posting index
- Content q2 fixed-key posting index
- Path q1 fixed-key posting index
- Path q2 fixed-key posting index
- Path q3 first-byte-sharded posting index

Posting specialization is reused:

- Empty
- Singleton
- RawU16
- DenseBitmap

DenseBitmap is selected only when its encoded bytes are strictly smaller than RawU16. Ties stay RawU16.

Content q1/q2 postings use local block IDs. Path/name postings use local document IDs. Path bytes use the same ASCII-fold semantics used by the existing query layer.

## 3. Query behavior after the change

Content:

- q1/q2: persistent block posting → document hit conversion; no full content scan fallback
- q3+: existing rarest-trigram anchor + exact mmap verification

Path/name:

- q1/q2: fixed persistent doc postings
- q3: persistent q3 doc posting
- q4+: rarest path-q3 anchor + exact path verification

## 4. Correctness

Final source regression:

- Search Core unit: 5/5 PASS
- Production tests: 35/35 PASS
- vNext segment tests: 17/17 PASS
- Gate4 query tests: 6/6 PASS
- Persistent-index tests: 3/3 PASS
- Doc tests: PASS
- `cargo fmt --check`: PASS
- `cargo clippy --all-targets --locked --offline -- -D warnings`: PASS
- Release build: PASS
- `SELF_TEST_PASS`: PASS

Persistent-index-specific coverage includes:

- q1/q2 vs naive oracle
- block-boundary q2 ownership
- Japanese UTF-8
- path q1/q2/q3+ vs naive path oracle
- malformed fixed-index encoding with repaired section/file checksum still fails closed

## 5. Text-heavy 20k query A/B

Same deterministic corpus, same process environment, release binaries, 31 rounds. Values below are p50.

| Query | Hits | Perf12 p50 | vNext p50 | Result |
|---|---:|---:|---:|---:|
| q1 | 20,000 | 0.223358 ms | 0.159959 ms | vNext 1.40x faster |
| q2 | 19,774 | 0.554235 ms | 0.158968 ms | vNext 3.49x faster |
| common `timeout` | 19,672 | 0.592488 ms | 2.590410 ms | vNext 4.37x slower |
| medium `deep_timeout_path` | 207 | 0.037005 ms | 0.044084 ms | vNext 1.19x slower |
| rare `unique_marker_970` | 2 | 0.008827 ms | 0.001161 ms | vNext 7.60x faster |
| zero-hit | 0 | 0.009304 ms | 0.000812 ms | vNext 11.46x faster |
| Japanese | 95 | 0.016842 ms | 0.014303 ms | vNext 1.18x faster |
| long rare | 1 | 0.009663 ms | 0.001976 ms | vNext 4.89x faster |
| filename `module_42_` | 67 | 0.003693 ms | 0.002859 ms | vNext 1.29x faster |

The persistent indexes eliminate the Gate4 q1/q2 scan regression. Rare, zero-hit, Japanese and long substring remain strong. The largest remaining content-query regression is the very common q3 path, where one rarest anchor still leaves too many candidate blocks for exact verification.

## 6. Text-heavy 20k build A/B

Five alternating Perf12/vNext runs. Median:

| Metric | Perf12 | vNext persistent | Result |
|---|---:|---:|---:|
| Build wall | 1,070.890 ms | 1,499.196 ms | vNext 1.40x slower |
| Index bytes | 76,733,278 | 50,385,256 | vNext 34.34% smaller |
| Peak RSS sample | 298,992 KiB | 197,352 KiB | vNext ~34.0% lower |

The persistent q1/q2/path indexes improve query latency but currently add too much text-heavy build cost. An attempted fused q1/q2 collection inside the q3 hot loop was measured and rejected because it made build time worse. The retained implementation keeps q3 and fixed-index construction independent and parallel.

## 7. Filename-heavy 100k A/B

vNext respects the u16 segment bound by using two 50k-document segments.

Build/resource result:

| Metric | Perf12 | vNext persistent | Result |
|---|---:|---:|---:|
| Build wall | 463.303 ms | 415.363 ms | vNext ~1.12x faster |
| Index bytes | 58,777,904 | 19,186,856 | vNext ~67.36% smaller |
| Peak RSS | 199,784 KiB | 108,368 KiB | vNext ~45.76% lower |

Filename query p50, 31 rounds:

| Query | Hits | Perf12 | vNext | Result |
|---|---:|---:|---:|---:|
| `component_00042` | 12 | 0.062445 ms | 0.191867 ms | vNext 3.07x slower |
| `group_0042` | 25 | 0.040136 ms | 0.181211 ms | vNext 4.52x slower |
| `repeated_component_component_099999` | 1 | 0.013817 ms | 0.043233 ms | vNext 3.13x slower |
| `png` | 100,000 | 1.813409 ms | 1.195117 ms | vNext 1.52x faster |
| missing marker | 0 | 0.000182 ms | 0.000109 ms | vNext 1.67x faster |

The catastrophic full-scan behavior from Gate4 is gone. Common/zero-hit filename queries are already faster, but selective long filename queries still trail Perf12.

## 8. Production adoption decision

**HOLD — do not switch production to vNext yet.**

Correctness is in good shape and several resource/query metrics are clearly better, but the production performance gate is not yet satisfied:

1. Text-heavy build is about 1.40x slower than Perf12 after adding persistent q1/q2/path indexes.
2. Common content query is about 4.37x slower.
3. Selective filename queries at 100k are about 3.1–4.5x slower.

Positive evidence worth preserving:

- q1 content: faster than Perf12
- q2 content: substantially faster
- rare/zero/long substring: substantially faster
- Japanese: no regression in this A/B
- text-heavy index size: ~34% smaller
- text-heavy RSS: ~34% lower in the measured sample
- filename-heavy 100k build: faster
- filename-heavy size/RSS: substantially lower

## 9. Recommended next optimization work before another production decision

### 9.1 Common q3 planner

The current planner uses one rarest trigram anchor. For highly common queries, add a second/third selective anchor and intersect candidate block postings before exact verification. This directly targets the `timeout` regression without changing exact semantics.

### 9.2 Selective filename/path planner

Persistent path q3 eliminated scanning, but long/selective filename queries still pay more overhead than Perf12. Profile and optimize path q3 anchor selection/intersection and exact verification before adding more format complexity.

### 9.3 Persistent-index build cost

Profile the remaining fixed-index construction/serialization cost, especially content q2. The already-tested "collect q1/q2 inside the q3 hot loop" approach was slower and should not be reintroduced without new evidence.

## 10. Status

Current phase:

- Gate0: PASS
- Gate1 `.prseg2` skeleton: PASS
- Gate2 block q3: PASS
- Gate3 posting specialization: PASS
- Gate4 query prototype: PASS correctness
- Persistent q1/q2 + path/name pre-adoption pass: IMPLEMENTED / A-B COMPLETE
- Production adoption: **HOLD**

Perf12 remains the production/oracle path.
