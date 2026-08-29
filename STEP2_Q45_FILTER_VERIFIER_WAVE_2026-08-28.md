> **HISTORICAL / SUPERSEDED:** This report records an earlier development wave. The normative Step 1 + Step 2 state is `STEP12_CONTENT_SEARCH_CANONICAL_2026-08-28.md`, `HANDOFF.md`, `STATE.json`, and the frozen docs.

# PersonalRag V2 — adaptive q4/q5 + verifier performance wave

Date: 2026-08-28
Status: **implementation/evaluation PASS for the surviving Variant-C prototype source**

## Scope

This wave was requested to address the Step-2 adversarial exact-verification bottleneck:

1. prototype adaptive rare q4/q5 filtering inside the capacity SLO,
2. A/B measure Variant C versus the new Variant D,
3. accelerate the exact byte verifier,
4. bring the 96 MiB adversarial case below 300 ms,
5. validate scaling on a larger corpus.

Important source-state note: the surviving canonical ZIP available to this session is the 0.2.0 Variant-C prototype (9-test baseline). The later Step-1 Unicode/regex implementation and the transient Step-2 persistent-reader/writer source were not present in that ZIP. Therefore this wave is **not promoted as a replacement canonical Step-2 tree**. It is a tested performance implementation/evidence package to be integrated into the restored/final Step-2 persistent code without redefining frozen Step-1 semantics.

## Gate 0

Before source changes:

- `cargo fmt -- --check`: PASS
- `cargo clippy --offline --locked --all-targets -- -D warnings`: PASS
- `cargo test --offline --locked`: 9/9 PASS
- `cargo build --offline --locked --release`: PASS

## Design

Variant D extends Variant C with two conservative mechanisms.

### Global q4/q5 Bloom presence

- q4 and q5 are encoded with the width in the key.
- The Bloom filter uses 4 deterministic hashes.
- It is insertion-only and is used only for **definite absence**.
- Bloom false positives are allowed; Bloom false negatives are not.
- A missing bit can safely turn the candidate set into zero blocks.
- Persistent budget is 0.5% of selected source bytes, capped at 4 MiB.

### Rare q4/q5 postings

- Block DF is estimated by a fixed Count-Min-style sketch after block-local de-duplication.
- Sketch collision can only over-estimate DF, which may lose an optimization but cannot lose a real search result.
- Candidate q4/q5 anchors are bounded by a deterministic pool.
- Final postings are built exactly and only anchors with exact `block_df <= 64` are retained.
- q3 sparse budget is 1.0%, q4/q5 sparse budget is 0.5%, preserving the existing total sparse-anchor hard budget of 1.5%.
- Block-local higher-gram keys use an upper-bounded cache; high-cardinality blocks automatically fall back to recomputation rather than unbounded global caching.

### Verifier

The previous Horspool-only exact matcher was changed to:

- word-at-a-time byte discovery for 1-byte needles,
- existing short-width shift path for 2-byte needles,
- word-at-a-time last-byte anchor discovery plus exact slice comparison for width >= 3.

Overlapping matches and exact result semantics remain unchanged.

## Correctness

Final focused/full suite in this source tree covers:

- all previous literal/oracle regressions,
- Variant D in the existing oracle loop,
- q4 and q5 rare-anchor candidate reduction,
- q4 global-absence adversarial shortcut,
- total q3+q4/q5 sparse budget <= 1.5%,
- Variant D total index hard capacity <= 10%,
- actual present q4/q5 substrings with no false negatives,
- serialized size equality.

Final suite: lib 1 + integration 12 = **13/13 PASS**.

## Verifier A/B on the identical 96 MiB corpus

The exact same pre-change generated corpus was reused.

- before, Variant C `abcde`: p50 **113.840 ms**, max **125.976 ms**
- after verifier change, Variant C `abcde`: p50 **59.562 ms**, max **65.015 ms**

This is approximately a **47.7% p50 reduction** before using q4/q5 filtering.

Evidence:

- `evidence/q45-wave/baseline-96mib-before.txt`
- `evidence/q45-wave/verifier-after-same-96mib.txt`

## 96 MiB Variant C versus D

Controlled corpus:

- selected source: 100,663,296 bytes
- blocks: 96

Capacity:

- C: 798,731 bytes = **0.7935%**
- D: 1,304,219 bytes = **1.2956%**
- D remains well below preferred <=5% and hard <=10%.

Representative results:

| Case | C | D | Candidate change |
|---|---:|---:|---:|
| rare q4 `wxyz` | p50 30.392 ms | **0.132 ms** | 96 -> 1 |
| rare q5 `klmno` | p50 38.620 ms | **0.276 ms** | 96 -> 1 |
| adversarial `abcde` | p50 52.441 ms | **~0 ms** | 96 -> 0 |

D adversarial max was effectively zero at the printed timer resolution, comfortably below the 300 ms hard target.

Evidence: `evidence/q45-wave/final-96mib-q45.txt`.

## 256 MiB scaling

Controlled corpus:

- selected source: 268,435,456 bytes
- blocks: 255
- D index ratio: **1.2932%**
- D build after bounded key-cache optimization: **10.875 s** on this shared host

Representative results:

| Case | C p50 | D p50 | Candidate change |
|---|---:|---:|---:|
| rare q4 `wxyz` | 169.364 ms | **0.187 ms** | 255 -> 1 |
| rare q5 `klmno` | 104.648 ms | **0.351 ms** | 255 -> 1 |
| adversarial `abcde` | 140.132 ms | **~0 ms** | 255 -> 0 |

The C q4 run also exhibited one shared-host wall-clock outlier (5.4 s), while D remained sub-millisecond because it did not scan the corpus.

Evidence: `evidence/q45-wave/scale-256mib-cd.txt`.

## Small-corpus capacity

At 4 MiB:

- D index ratio: **2.4363%**
- q4/q5 candidate reduction remains 4 -> 1
- adversarial q4 absence remains 4 -> 0

This remains below the preferred <=5% target.

Evidence: `evidence/q45-wave/small-4mib-d.txt`.

## Decision

The performance hypothesis is supported:

- the exact verifier itself is materially faster,
- adaptive q4/q5 removes the current adversarial common-trigram scan,
- q4/q5 rare anchors scale from 96 to 255 blocks without latency growth,
- capacity remains comfortably inside both product thresholds at 4, 96, and 256 MiB.

The next integration action is to carry Variant D's q4/q5 Bloom/postings sections and verifier into the actual Step-2 persistent reader/writer tree while preserving the frozen Unicode/wildcard/regex semantics. This report does not declare the missing transient persistent implementation reconstructed or Step 2 complete.
