# PersonalRag Segment Format vNext Gate 4 Report

Date: 2026-08-17  
Scope: Query prototype on top of Gate3 `.prseg2`  
Production/oracle: Perf12 remains unchanged

## 1. Gate 4 goal

Gate 4 implements the first complete query path for the vNext segment while preserving Perf12 as the production/correctness oracle.

Required query classes:

- q1 / q2 short content queries
- q3+ content queries
- rarest-trigram anchor selection
- exact block-local verification with cross-block correctness
- filename/path search
- `Perf12 == vNext == naive substring oracle`

## 2. Implementation

New module:

```text
search-core/src/vnext_query.rs
```

Public prototype API on `VNextSegmentReader`:

```text
search_content(...)
search_content_with_diagnostics(...)
search_path(...)
search_name(...)
```

### Query normalization

Queries use the same ASCII fold used by Perf12:

```text
fold_ascii(query)
```

UTF-8 bytes outside ASCII are preserved. Japanese substring semantics therefore remain byte-exact and match the existing oracle.

### q1 / q2

Gate4 deliberately starts with an exact mmap scan for one- and two-byte content queries. This keeps correctness isolated from a new persistent short-query format. The Gate5 A/B numbers below show that a dedicated q1/q2 accelerator is likely needed before production adoption.

### q3+

For every query trigram:

1. look up block posting cardinality;
2. return exact zero-hit immediately if any trigram is absent;
3. choose the smallest posting as the anchor;
4. iterate candidate owner blocks;
5. exact-verify only the bounded block neighborhood required by the anchor offset and query length;
6. deduplicate matched document IDs.

The exact verifier reuses Perf12's existing AVX2/BMH `ExactMatcher` implementation rather than introducing another substring implementation.

### Block-boundary verification

If the selected trigram is at query byte offset `a`, the candidate block verification window extends:

```text
left  = a bytes
right = query_len - a - 1 bytes
```

around the owner block, clipped to the containing document. This is sufficient for matches spanning adjacent blocks, including queries spanning several blocks, without scanning the entire document for rare anchors.

### Filename/path

Gate4 searches `display_path` with the same effective semantics as Perf12:

```text
normalized_name = fold_ascii(display_path)
```

The first prototype is an exact path scan. The Gate5 A/B shows this is the largest remaining query regression and should receive a persistent name/path index before production switch.

## 3. Correctness tests

New file:

```text
search-core/tests/vnext_query.rs
```

Gate4-specific tests: 6/6 PASS.

Coverage includes:

- q1 and q2 exact search
- ASCII case folding
- Japanese UTF-8
- filename/path semantics
- rarest trigram selection
- absent-trigram exact zero-hit
- exact block verification
- multiple block-boundary crossings
- Perf12/vNext/naive three-way oracle
- 600 deterministic randomized substring queries against naive oracle

Final full regression:

```text
existing unit       5 / 5   PASS
production         35 / 35  PASS
vNext Gate1-3      17 / 17  PASS
vNext Gate4         6 / 6   PASS
doc tests                    PASS
cargo fmt --check            PASS
Clippy -D warnings           PASS
release build                PASS
SELF_TEST_PASS               PASS
```

## 4. Gate3 build-format preservation

Gate4 does not modify the `.prseg2` writer or format.

The original Gate3 ZIP and Gate4 work tree were both used to build the same deterministic 20k corpus.

Gate3:

```text
elapsed_ms=554.350
file_bytes=43417320
q3_keys=34801
q3_posting_ids=14033692
posting_bytes=17329908
```

Gate4:

```text
elapsed_ms=546.350
file_bytes=43417320
q3_keys=34801
q3_posting_ids=14033692
posting_bytes=17329908
```

Generated `.prseg2` SHA-256 for both:

```text
49538e03bcfd1cc70b8f76e47ce78a67a24a5ccfa58078c9fb6fe7aade480d92
```

Result:

```text
PRSEG2_BYTE_IDENTICAL
```

Therefore Gate4 is a pure query-layer addition and does not perturb Gate3 index bytes.

## 5. Initial 20k query A/B

Environment: same Linux sandbox, Rust 1.97.1, same deterministic 20k corpus, release builds, 15 measured rounds after first call.

| Query | Hits | Perf12 p50 ms | vNext p50 ms | vNext relative |
|---|---:|---:|---:|---:|
| q1 `e` | 20000 | 0.161527 | 1.548023 | 9.58x slower |
| q2 `ti` | 19774 | 0.649843 | 1.445749 | 2.22x slower |
| common `timeout` | 19672 | 0.629541 | 2.378148 | 3.78x slower |
| medium `deep_timeout_path` | 207 | 0.031102 | 0.040309 | 1.30x slower |
| rare `unique_marker_970` | 2 | 0.008487 | 0.000972 | 8.73x faster |
| zero-hit | 0 | 0.009035 | 0.000990 | 9.13x faster |
| Japanese `日本語検索` | 95 | 0.014168 | 0.012386 | 1.14x faster |
| long rare substring | 1 | 0.008031 | 0.001508 | 5.33x faster |
| filename `module_42_` | 67 | 0.002891 | 0.912460 | 315.62x slower |

Important interpretation:

- rare/zero/long queries validate the block-q3 design strongly;
- Japanese is already competitive;
- medium query is close;
- common q3 still pays for ~19.7k exact block verifications;
- q1/q2 are scan fallbacks and need a short-query accelerator if Gate5 requires Perf12 parity;
- filename/path scan is not acceptable for production and is the clearest next optimization candidate.

The initial naive exact verifier produced ~12.4 ms for the common query. Reusing Perf12's AVX2/BMH matcher reduced it to ~2.38 ms without changing index bytes or result semantics.

## 6. Gate 4 conclusion

Gate4 correctness is complete:

```text
Perf12 result == vNext result == naive substring oracle
```

for the implemented oracle set, including randomized and block-boundary cases.

However Gate4 is not a production-switch decision. The initial A/B exposes three remaining query weaknesses:

1. filename/path indexing;
2. q1/q2 persistent acceleration;
3. very common q3 queries with dense candidate blocks.

Those findings should be carried into Gate5 rather than hidden by changing several index structures in Gate4 itself.

Perf12 remains production/oracle. No production path was switched.
