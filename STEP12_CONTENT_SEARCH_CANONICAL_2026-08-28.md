# PersonalRag V2 Step 1 + Step 2 canonical completion

Date: 2026-08-28  
Decision: **COMPLETE / CANONICAL**

## Scope

This wave integrates the frozen Step 1 search semantics into the Step 2 production persistent Variant-D tree and validates the combined implementation as one deterministic content-search core.

Completed:

- Unicode 15.1 NFC semantics
- full default Unicode case folding
- original UTF-8 byte-offset recovery
- literal / wildcard / safe regex search
- mandatory-literal candidate filtering
- Variant-D q3/q4/q5 filtering
- persistent format v2
- immutable generation/recovery/GC
- publish -> reload -> exact-search equivalence
- capacity and latency scaling at 4/96/256 MiB

## Implementation workflow

### Gate 0

Before the integration, the actual Step 2 source passed:

- fmt PASS
- clippy PASS
- tests 22/22 PASS
- release build PASS

### Design

The frozen Step 1 normative semantics were reapplied to the actual Step 2 source. Persistent search-semantic identity was versioned rather than silently reusing the former ASCII-fold identity.

### Implementation and focused testing

Source-contained Unicode 15.1 tables, normalization/folding, original-byte mapping, wildcard, regex NFA verification, persistent reload paths, and compatibility rejection tests were implemented.

### Review / correction loops

Review found and corrected:

1. full-fold expansions such as `ß -> ss` could map multiple normalized positions to one source start; result starts are now deduplicated by original byte position;
2. a byte-preserving Unicode fast path initially needed stronger NFC-composition safety; Hangul and non-Hangul composition-capable pairs now force full normalization where required;
3. composition checks initially caused avoidable ASCII/Japanese performance regression; the safe fast path was narrowed/optimized without changing semantics;
4. empty-match regex end offsets could report normalized byte length rather than original UTF-8 line length; original end offset is now retained explicitly.

No unresolved correctness issue remained after review.

## Unicode reproducibility

Tables are generated from Python 3.13.5 Unicode 15.1.0 and are source-contained.

- `src/unicode_tables.rs` SHA-256: `faeab29821784bf51b5000b38a23d914fec544b8a5f72694e823e4c13ee6851f`
- `tests/unicode_oracle_vectors.txt` SHA-256: `7cace97523c24cf2ff4f71dad1c8437f508d02b74e2b4fc0ffa8da0cd83fff93`
- independent oracle vectors: 516
- Rust vs independent oracle: all PASS

## Persistent identity

- magic: `PRV2IDX1`
- format version: **2**
- search semantic id: **`0x0003_0001`**
- sections: 7
- prior format v1: deliberately rejected

The version increase is required because indexed comparison semantics changed from the earlier ASCII-fold representation to frozen Unicode semantics.

## Final regression

Rust 1.97.1:

- `cargo fmt -- --check`: PASS
- `cargo clippy --offline --locked --all-targets -- -D warnings`: PASS
- `cargo test --offline --locked`: **40/40 PASS**
- `cargo build --offline --locked --release`: PASS

Test breakdown:

- library/unit: 13
- P0/Variant-D: 12
- persistent: 9
- search semantics: 6

## Final controlled performance

Affinity-controlled release binary:

| Corpus | index/source | publish | load |
|---|---:|---:|---:|
| 4 MiB | **2.669382%** | 148.794 ms | 1.167 ms |
| 96 MiB | **1.303512%** | 3986.824 ms | 9.891 ms |
| 256 MiB | **1.298823%** | 13214.225 ms | 29.210 ms |

256 MiB query results:

| Query | p50 | max | candidates |
|---|---:|---:|---:|
| rare q3 `abd` | 2.541 ms | 2.661 ms | 1 block |
| rare q4 `wxyz` | 2.432 ms | 2.650 ms | 1 block |
| rare q5 `klmno` | 2.524 ms | 2.656 ms | 1 block |
| adversarial `abcde` | 0 ms | 0 ms | 0 blocks |
| `STRASSE` -> `Straße` | 3.201 ms | 3.317 ms | 1 block |
| `CAFÉ` -> decomposed source | 2.444 ms | 2.892 ms | 1 block |
| indexable regex | 78.588 ms | 81.107 ms | 1 block |
| wildcard | 61.707 ms | 64.108 ms | 1 block |

All measured first-useful-batch cases are below 300 ms in this controlled environment. All measured persistent ratios are below the preferred 5% target as well as the hard 10% limit.

Final Windows acceptance remains Step 7.

## Canonical decision

Step 1 and Step 2 are now one coherent canonical content-search implementation. There is no longer a Step 1-source provenance gap in the canonical tree.

The next implementation wave is **Step 3: Everything-style filename/path metadata index**. Step 3 must not alter frozen content-search semantics unless an explicit versioned change is approved.
