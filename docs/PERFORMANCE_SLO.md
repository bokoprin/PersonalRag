# PersonalRag Performance SLO

Status: **Normative development target**  
Scope: PersonalRag search/index architecture and performance work  
Last updated: 2026-08-25

## 1. Purpose

This document defines the performance targets that guide PersonalRag architecture, implementation, optimization, and acceptance decisions.

PersonalRag is intended to be a daily-use local desktop search application with a DocFetcher-like user experience, while retaining room for stronger filename/path/content search and future natural-language/LLM-assisted retrieval.

The primary objective is **not to minimize raw lookup nanoseconds at any cost**. The objective is to provide search that feels instantaneous to a human user while keeping the persistent index compact enough for ordinary local storage.

All future search/index changes should be evaluated against the SLOs in this document before accepting local micro-optimizations.

---

## 2. Product-level performance principles

1. **Human-perceived responsiveness is the primary search-speed criterion.**
   - A search implementation does not need to be the theoretically fastest implementation if it remains comfortably within the latency SLO.
   - Storage, update cost, memory use, robustness, and maintainability may take priority over sub-millisecond lookup improvements that are not visible to the user.

2. **Persistent index size is a first-class product constraint.**
   - An index approaching the size of the source corpus is unacceptable for the intended desktop-search product.
   - The architecture should target a small fraction of the selected source bytes.

3. **Architecture is judged end-to-end, not by one internal data structure.**
   - Reducing one section by a few percent is not sufficient if the whole index remains above the product limit.
   - Likewise, a microbenchmark regression may be acceptable if end-to-end search remains within SLO and the storage reduction is materially better.

4. **The default durable index should store search structures, not an unnecessary uncompressed duplicate of the source corpus.**
   - Full extracted/normalized text should not be persisted uncompressed merely to make verification convenient.
   - If retained text is necessary, prefer bounded/structured compressed storage or another design justified by end-to-end measurements.

5. **Rare or expensive search modes may use more work than normal search, but supported search must remain bounded.**
   - It is acceptable for substring/wildcard/fuzzy/fallback search to be slower than ordinary token/filename/path search.
   - It is not acceptable for a supported interactive search mode to routinely exceed the hard latency limit.

---

## 3. Normative search latency SLO

### 3.1 Scope of measurement

The latency SLO applies to the **PersonalRag retrieval/search-engine portion** of a request.

For future natural-language/LLM search, measure separately:

```text
natural-language interpretation / LLM
        ↓
query construction
        ↓
PersonalRag retrieval/search engine  ← this document's latency SLO
        ↓
optional LLM answer generation
```

LLM inference time is not included in the 300 ms search-engine hard limit.

### 3.2 Normal interactive search target

For ordinary filename, path, token/term, and common content queries:

| Metric | Target |
|---|---:|
| p50 | <= 30 ms |
| p95 | <= 100 ms |
| p99 | <= 200 ms |
| hard interactive limit | <= 300 ms |

The normal user experience should feel effectively immediate.

### 3.3 Expensive supported search target

Examples include substring fallback, wildcard, phrase, fuzzy, unusually common terms, compound filters, and other supported queries that are expected to require more work.

| Metric | Target |
|---|---:|
| p50 | <= 100 ms |
| p95 | <= 200 ms |
| hard interactive limit | <= 300 ms |

The 300 ms limit is the product-level boundary: a representation that uses more CPU than the fastest design may still be preferable if its end-to-end query remains inside this limit and it substantially improves index size or another first-class metric.

### 3.4 No nanosecond-driven architecture decisions

Internal lookup latency such as `CQ3DIR ns/op` remains useful for diagnosis, but it is not a product acceptance criterion by itself.

For example, a representation that makes a metadata lookup 2x slower must not be rejected solely on that ratio if the absolute additional cost is sub-microsecond and end-to-end search remains well under 300 ms.

---

## 4. Normative index-size SLO

### 4.1 Primary metric

The primary product index-size metric is:

```text
IndexSourceRatio = finalPersistentIndexBytes / selectedSourceBytes
```

`selectedSourceBytes` means the total original byte size of files selected for indexing, before text extraction.

This is the user-visible storage ratio and therefore the main acceptance metric.

### 4.2 Targets

| IndexSourceRatio | Classification |
|---:|---|
| <= 5% | **Target / Excellent** |
| > 5% and <= 10% | **Acceptable** |
| > 10% and <= 15% | **Improvement required** |
| > 15% | **Architecture review required** |

Normative product requirements:

- **Target:** index <= **5%** of selected source bytes.
- **Hard capacity limit:** index <= **10%** of selected source bytes.

An architecture that is structurally unable to approach 10% should not be treated as the long-term production target merely because it can be locally optimized.

### 4.3 Capacity examples

| Selected source corpus | Target (5%) | Hard limit (10%) |
|---:|---:|---:|
| 1 GB | 50 MB | 100 MB |
| 10 GB | **500 MB** | **1 GB** |
| 100 GB | 5 GB | 10 GB |
| 1 TB | 50 GB | 100 GB |

For the intended product, a 10 GB source corpus producing an index near 10 GB is categorically unacceptable.

### 4.4 Secondary diagnostic metric

Also record:

```text
IndexExtractedTextRatio = finalPersistentIndexBytes / extractedSearchableTextBytes
```

This is a diagnostic metric, not the main product acceptance ratio, because PDF/Office/binary-heavy corpora can have source bytes far larger than extracted text while TXT/source-code corpora may have extracted text close to source size.

The long-term design should normally avoid an index that is larger than extracted searchable text unless there is a measured and justified product benefit.

---

## 5. Current baseline and interpretation

The current measured `C:\Program Files` corpus used by the 2026-08-25 CQ3DIR experiments had approximately:

- selected source bytes: **15.32 GB**
- extracted/searchable bytes read: **2.79 GB**
- persistent index: **4.96 GiB**
- current source/index ratio: approximately **32%**

This baseline is therefore **well above the 10% product hard capacity limit**.

The implication is important:

> Further CQ3DIR record-level optimization may still be useful, but it cannot by itself define the long-term architecture if the total index remains far above 10%.

Future work should prioritize architecture alternatives capable of materially changing the whole-index ratio.

---

## 6. Search capability and storage trade-off

PersonalRag does not require every search mode to use the same index representation.

A preferred direction is to evaluate a layered/hybrid architecture such as:

```text
Primary search index
  - filename/path
  - token/term inverted index
  - metadata

Optional lightweight auxiliary structures
  - substring candidate narrowing
  - code/identifier-oriented search
  - filename/path substring support

Verification / preview
  - source file access where appropriate
  - or bounded compressed extracted-text storage if justified
```

The current design of keeping a full normalized text store plus broad q3 structures should be treated as one candidate architecture, not an assumed invariant.

The following question must be answered experimentally:

> Can token/inverted-index or hybrid designs achieve <= 10% persistent index size while keeping all supported interactive searches <= 300 ms?

If yes, those designs should be preferred over a materially larger index even if individual low-level lookups are slower.

---

## 7. Required architecture comparison

The next architecture-level benchmark should compare at least:

### A. Current full q3-oriented architecture

Use the current implementation as the correctness and performance baseline.

### B. Token inverted index

A Lucene/DocFetcher-like concept:

- tokenize extracted content,
- dictionary term -> postings,
- index filename/path/title/metadata separately,
- do not require an uncompressed durable duplicate of full extracted text.

### C. Token index + lightweight substring auxiliary index

Use token/inverted search for the common path, with a smaller auxiliary structure or candidate-narrowing path for exact substring requirements.

The purpose is not to copy Lucene's file format. The purpose is to measure whether its architectural principle better satisfies PersonalRag's product SLO.

---

## 8. Mandatory benchmark dimensions

Every architecture candidate must report the following on the same frozen corpus/configuration when possible.

### 8.1 Persistent capacity

- selected source bytes
- extracted searchable text bytes
- total persistent index bytes
- `IndexSourceRatio`
- `IndexExtractedTextRatio`
- per-section/per-component bytes

### 8.2 Search latency

Measure at minimum:

- filename exact/prefix/substring where supported
- path search
- common term hit
- rare term hit
- term miss
- multi-term AND/OR
- phrase where supported
- exact substring hit
- exact substring miss / hard fallback case
- high-hit-count query

Record:

- p50
- p95
- p99
- maximum observed latency
- results count / limit
- relevant internal diagnostics such as candidate counts and verifier cost

### 8.3 Correctness

- result-set equivalence against the agreed reference behavior
- deterministic repeated-run output
- no false negatives for supported search semantics
- explicit declaration of any intentionally changed search semantics

### 8.4 Build and update cost

Record:

- cold/full build time
- warm/repeat build time where applicable
- incremental single-file update latency
- amount of data rewritten for a small update
- temporary peak disk usage

### 8.5 Memory and startup

Initial product targets:

| Metric | Target |
|---|---:|
| idle resident memory | <= 300 MB |
| ordinary search resident memory | <= 500 MB |
| exceptional search peak | <= 1 GB |
| existing-index open/startup | <= 1 s |

These are secondary targets and may be revised after measurement, but designs should avoid requiring the whole index to be resident in RAM.

### 8.6 Incremental freshness

Target for ordinary small files:

```text
single file change → searchable index update <= 1 second
```

Parser-heavy PDF/Office documents may be reported separately when extraction itself dominates.

A one-file change should not require rewriting a large unrelated portion of the index without a strong justification.

---

## 9. Acceptance rules for optimization work

### 9.1 Whole-product metrics override local wins

A change is not considered strategically important merely because it improves one internal benchmark.

Example:

```text
CQ3DIR improves significantly
but whole index moves from 32% to 29%
```

This may be a useful engineering improvement, but the architecture remains above the 10% product limit and therefore still requires higher-level redesign.

### 9.2 Search-speed headroom may be spent on capacity

If an architecture increases query time from, for example, 5 ms to 30 ms while reducing persistent index size from multiple gigabytes to a few hundred megabytes, it should normally be considered a strong improvement because both values remain comfortably within the user-facing latency SLO.

### 9.3 Hard failures

A candidate fails the product target if any of the following are true without an explicit approved exception:

- normal or supported expensive search exceeds 300 ms under the accepted benchmark corpus/workload,
- persistent index exceeds 10% of selected source bytes as a steady-state long-term architecture,
- correctness semantics regress silently,
- corruption/truncation handling stops being fail-closed,
- deterministic build/search requirements are violated,
- a small incremental update causes unacceptable broad rewrites or storage amplification.

### 9.4 Prototype before production-format migration

Architecture experiments should first be implemented as isolated/read-only prototypes where practical.

Only after a candidate demonstrates:

- capacity viability,
- end-to-end latency viability,
- correctness,
- bounded update/build behavior,

should production format/version migration be designed.

---

## 10. Benchmark discipline

When comparing architectures:

1. Use the same corpus snapshot or explicitly mark `CORPUS_CHANGED`.
2. Freeze parser/search/build configuration.
3. Record source bytes, extracted bytes, file count, and corpus identity.
4. Warm/cold conditions must be labeled, not mixed.
5. Compare end-to-end search latency in addition to internal microbenchmarks.
6. Avoid accepting an architecture based solely on a synthetic benchmark.
7. Keep raw machine-readable benchmark output and a concise Markdown report.
8. Temporary multi-GB indexes generated for experiments should be deleted after the necessary measurements and durable reports are captured.

---

## 11. Development priority derived from this SLO

Until the persistent index is within the intended range, prioritize work in this order:

1. **Architecture capable of <= 10% source/index ratio**
2. **Preserve all supported search modes <= 300 ms**
3. Correctness, deterministic/fail-closed behavior, and durable safety
4. Incremental update behavior
5. Build time and memory
6. Local micro-optimizations after the architecture is inside the product envelope

This means that current q3/CQ3DIR compression experiments remain useful evidence, but they should no longer consume the main optimization effort unless they contribute materially toward the whole-index SLO.

---

## 12. North Star

PersonalRag's performance target is:

> **For a 10 GB local document corpus, aim for an approximately 500 MB persistent index and never exceed 1 GB as the normal product design, while returning ordinary search comfortably under 100 ms and keeping every supported interactive search path within 300 ms.**

This North Star is the default criterion for future index/search architecture decisions unless this specification is deliberately revised with new benchmark evidence.
