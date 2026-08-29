# PersonalRag V2 Performance SLO

Status: **Canonical normative development target**  
Scope: PersonalRag V2 search/index architecture  
Last updated: 2026-08-30

## 1. Product objective

PersonalRag V2 is intended to become a daily-use local desktop search application. Search must feel immediate while the persistent index remains a small fraction of selected source data. Whole-system product behavior takes precedence over isolated nanosecond-level microbenchmarks.

## 2. Search latency

The hard limit applies to the **first useful result batch**, not exhaustive enumeration.

| Metric | Target |
|---|---:|
| preferred first useful batch | <= 100 ms |
| hard first useful batch | <= 300 ms |

Initial batch target:

- <= 100 files
- <= 500 matches
- <= 3 initial snippets per file

For future LLM-assisted search, LLM inference time is measured separately; the deterministic V2 retrieval engine remains subject to this SLO.

## 3. Persistent capacity

Primary metric:

```text
IndexSourceRatio = finalPersistentIndexBytes / selectedSourceBytes
```

| Ratio | Classification |
|---:|---|
| <= 5% | preferred / excellent |
| >5% and <=10% | acceptable |
| >10% | fails hard capacity gate |

The percentage hard gate is evaluated for selected source sets of **4 MiB or larger**. Below 4 MiB, fixed bundle/header/rollback-reserve overhead dominates the ratio; measure and report it, but do not fail product acceptance from that percentage alone. This floor does not weaken any generation-level fail-closed capacity checks.

The complete persistent footprint must be counted, including catalog/block metadata, gram structures, sparse postings, manifests/deltas/reserve that are part of normal operation, and compressed extracted verification data where required.

## 4. Correctness is not traded for speed

- supported literal substring search: zero false negatives
- candidate false positives: allowed
- final results: exact verification required
- hard logical-unit boundaries must be preserved
- case-sensitive search must verify original semantics
- Unicode behavior must follow the normative architecture once implemented

## 5. Sparse anchor budget

Rare-trigram posting structures are capped at:

```text
<= 1.5% of selected source bytes
```

Exhausting this budget must fall back safely to broader candidate verification; it must never cause false negatives.

## 6. Current canonical evidence

As of 2026-08-29, Steps 1–5 are integrated and frozen. Step 5 adds compressed extracted-document verification through `PRV2VER1` without changing the Step 2 `PRV2IDX1` identity.

Affinity-controlled final release measurements:

- 4 MiB persistent/source ratio: **2.6694%**
- 96 MiB persistent/source ratio: **1.3035%**
- 256 MiB persistent/source ratio: **1.2988%**
- 256 MiB rare q3/q4/q5 searches: one candidate block, approximately **2.4-2.5 ms p50**
- 256 MiB adversarial `abcde`: **0 candidate blocks**
- 256 MiB Unicode full-fold `STRASSE` -> `Straße`: **3.201 ms p50**
- 256 MiB NFC-equivalent `CAFÉ` -> decomposed source: **2.444 ms p50**
- 256 MiB indexable regex: **78.588 ms p50**
- 256 MiB wildcard: **61.707 ms p50**

All recorded first-batch cases are below the 300 ms hard target on the affinity-controlled Linux host. Final product acceptance still requires Step 7 testing on the intended Windows environment.

Normative evidence is in `evidence/step12-final/`.

## 7. Acceptance rules

A design is not accepted because one internal operation is faster. It must be evaluated end-to-end against:

1. literal correctness,
2. first-batch latency,
3. full persistent capacity,
4. build/update cost,
5. memory use,
6. failure behavior and recoverability,
7. implementation complexity and maintainability.

If two designs both remain comfortably below 300 ms, prefer the materially smaller/simpler design unless the faster design creates a user-visible benefit.

## 7.0 Step 7 final-stabilization whole-store evidence

The product-level probe forces a compaction-producing update and counts the complete retained store after normal two-bundle rollback retention. The pre-fix diagnostic run `33274120802` measured:

| Selected source | Init store/source | Complete store/source after compacting update |
|---:|---:|---:|
| 4 MiB | 2.689290% | **5.387712%** |
| 96 MiB | 1.356620% | **2.713791%** |
| 256 MiB | 1.341617% | **2.683691%** |

All normative sizes are below the 10% hard gate. The earlier ~1.2 MiB Step 7 corpus measured 24.6876% because fixed dual-generation rollback overhead dominated the tiny denominator; that observation remains valid as a diagnostic but is not a hard-gate failure under the clarified >=4 MiB acceptance floor.

The reproducible Windows-side measurement command is `tools/measure_product_capacity.ps1`. Final target-Windows retest must re-run 4/96/256 MiB and record the exact complete-store ratios.

## 7.1 Step 5 controlled document evidence

On the controlled ~4 MiB PDF corpus, the extraction-aware content generation measured:

- `PRV2IDX1 + PRV2VER1` / selected source: **2.1714%**
- first-batch cold/max: **8.870 ms**
- p50: **0.047 ms**
- p95/p99: **0.069 ms**
- candidate bytes / verification scan bytes: **176 / 176**

This is the Step 5 generation-level metric. The complete product footprint rule above remains normative and is re-measured with metadata/delta/bundle reserve during Step 7 target-Windows E2E acceptance.

## 8. Required benchmark reporting

For each architecture candidate record at minimum:

- selected source bytes
- searchable/extracted bytes
- persistent bytes and per-section bytes
- source/index ratio
- p50/p95/p99/max latency
- candidate blocks/bytes
- exact-verification bytes
- first-batch counts
- zero-hit cases
- common/high-hit cases
- rare selective cases
- short 1-byte/2-byte cases
- non-ASCII correctness cases
- build time and peak memory when those paths become production-relevant
