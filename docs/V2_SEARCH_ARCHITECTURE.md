# PersonalRag V2 Search Architecture

Status: **Normative architecture specification**  
Scope: PersonalRag V2 search/index architecture  
Last updated: 2026-08-27

## 1. Purpose

This document defines the source-of-truth architecture for PersonalRag V2.

PersonalRag V2 is a **local Universal Grep**: an Everything-like desktop search application that combines extremely fast filename/path search with grep-like literal substring search across plain text, source code, logs, PDF, and Office documents.

This architecture is designed from first principles around the product SLOs in `docs/PERFORMANCE_SLO.md`.

The primary architecture targets are:

- **Search first-batch latency: <= 300 ms**
- **Persistent index size: <= 10% of selected source bytes**
- **Preferred search latency: <= 100 ms**
- **Preferred persistent index size: <= 5% of selected source bytes**
- **Literal substring correctness: no false negatives for supported search semantics**

Local micro-optimizations are subordinate to these product-level constraints.

---

## 2. Architectural reset

PersonalRag V2 is a clean-sheet design.

The previous architecture based on storing large normalized text blobs plus broad q3/trigram directory/posting structures is **not a compatibility constraint** for V2.

V2 SHALL NOT assume that:

- the index must answer a query without touching source or verification data,
- every substring occurrence must be represented directly in a posting list,
- full extracted text must be persisted uncompressed,
- lookup nanoseconds are more important than whole-index size,
- a local data-structure improvement is worthwhile if the overall index still violates the product SLO.

The V2 design principle is:

> **Use a compact index to identify where a match may exist, then perform exact grep verification only on those candidate regions.**

False positives in the candidate stage are allowed.  
False negatives in supported literal substring search are not allowed.

---

## 3. Product definition

PersonalRag V2 is:

> **Everything-style filename/path search + VS Code/ripgrep-style literal substring search + extraction adapters for PDF and Office documents.**

The primary user workflow is:

1. enter a filename/path query and/or content query,
2. receive the first useful result batch within 300 ms,
3. inspect file-grouped matches and snippets,
4. open the source file or logical document location.

V2 is not primarily a semantic search engine. Natural-language/LLM search may be added later as a query-planning layer above the deterministic retrieval engine.

---

## 4. Normative search semantics

### 4.1 Primary search: literal substring

Literal substring search is the primary content-search operation.

If searchable logical text contains:

```text
abcdef
```

then:

```text
cde
```

MUST match.

Word boundaries are not required.

Examples that MUST be supported:

- `CreateFileW`
- `File` inside `CreateFileW`
- `ABC123_DEF`
- Japanese literal text
- source-code fragments
- log fragments
- 1-byte / 1-character queries
- 2-byte / 2-character queries
- longer arbitrary substrings

### 4.2 Case

Default:

- **case-insensitive**

Optional mode:

- **case-sensitive**

Smart-case behavior SHALL NOT be used.

The candidate index MAY use one normalized/case-folded representation for both modes. Case-sensitive correctness MUST be enforced during exact verification against the original verification text.

### 4.3 Unicode

Searchable text and query text SHALL be normalized with:

- UTF-8 internal representation
- Unicode **NFC**

V2 SHALL NOT apply NFKC by default.

Examples:

- canonically equivalent sequences such as composed/decomposed accents MAY match after NFC,
- compatibility-equivalent forms such as full-width ASCII and ASCII SHALL NOT be silently collapsed by default.

### 4.4 Regex

Regex is supported as a secondary search mode.

The regex engine SHALL use a linear-time or otherwise non-catastrophic execution model.

The V2 <=300 ms SLO is guaranteed only for **indexable regex queries** from which at least one mandatory literal anchor can be extracted.

Examples:

```regex
ERROR_[0-9]{4}
Create.*File
Create(File|Directory)W
```

Queries without a useful mandatory literal, such as:

```regex
[A-Z]{8}
```

are outside the V2 guaranteed fast path.

The UI SHOULD indicate when a regex query cannot use the fast index path.

Backreferences, catastrophic-backtracking semantics, and similarly unsafe/expensive features are not required for V2.

### 4.5 Cross-boundary matching

V2 does **not** match across logical text-unit boundaries.

Examples of logical units:

| Source type | Logical unit |
|---|---|
| plain text / source / log | line |
| PDF | normalized paragraph / text block |
| DOCX | paragraph |
| XLSX | cell |
| PPTX | text shape / paragraph |
| CSV | cell or row according to extractor |
| HTML | logical text node / block |

Extractor-specific soft wrapping MAY be normalized before indexing. For example, PDF line wrapping that is purely layout-driven may be merged by the extractor.

The search engine itself SHALL treat logical-unit boundaries as hard boundaries.

### 4.6 Result batching

The 300 ms SLO applies to the **first useful result batch**, not complete enumeration of every match in the corpus.

Initial result target:

- up to **100 files**
- up to **500 matches**
- up to **3 initial snippets per file**

Continuation SHALL use cursor-based or equivalent incremental enumeration.

Exact global hit count is not required before the first batch is returned.

This is mandatory for preserving bounded interactive latency on high-frequency queries such as one-character searches.

---

## 5. High-level architecture

```text
                    PersonalRag UI
                         |
                         v
                   Query Engine
                         |
          +--------------+--------------+
          |                             |
     File Catalog                Content Search
    (Everything-like)                  |
          |                     Candidate Index
          |                             |
          |                    Candidate Blocks
          |                             |
          +--------------+--------------+
                         |
                         v
                   Exact Verifier
                         |
            +------------+------------+
            |                         |
       Plain files              PDF / Office
            |                         |
       Source bytes          Compressed extracted
       mmap/read             verification store
            |                         |
            +------------+------------+
                         |
                         v
                   Exact matches
                         |
                         v
            First 100 files / 500 hits
```

---

## 6. File catalog

Filename/path search is logically separate from content search.

The file catalog SHALL track at least:

- stable FileID
- path
- filename
- source root
- size
- mtime
- file type
- searchable/extractable flags
- verification-store mapping where applicable

The catalog SHOULD be compact enough to remain memory-resident for normal desktop-scale corpora.

Content-search index structures SHALL NOT be required to answer filename/path-only queries.

---

## 7. Search block model

### 7.1 Target block size

The V2 target search-block size is:

> **1 MiB**

This is a target, not an instruction to split logical text units arbitrarily.

Blocks SHALL be packed from complete logical units.

Small files MAY share a virtual block.

Gram generation SHALL NOT cross:

- file boundaries,
- logical-unit boundaries.

### 7.2 Block-count formula

Let:

- `S` = searchable normalized bytes
- `B` = target block size
- `N = ceil(S / B)`

For a 10 GiB all-text corpus:

```text
S = 10 GiB
B = 1 MiB
N = 10,240 blocks
```

### 7.3 Why 1 MiB is fixed

For 10 GiB searchable text:

| Block size | Block count | Exact bigram bitmap size | Source ratio |
|---:|---:|---:|---:|
| 256 KiB | 40,960 | 320 MiB | 3.125% |
| 512 KiB | 20,480 | 160 MiB | 1.563% |
| **1 MiB** | **10,240** | **80 MiB** | **0.781%** |

Smaller blocks reduce verification granularity but materially increase persistent bitmap cost.

Because the rare-anchor path is designed to cap candidate verification to tens of MiB for selective queries, 1 MiB is the preferred capacity/latency balance.

**Decision: FIXED.**

---

## 8. Exact unigram and bigram block bitmap

### 8.1 Unigram index

For every possible byte value, V2 stores an exact bitmap of blocks in which that byte exists.

Universe:

```text
256 byte values
```

This structure primarily accelerates 1-byte/short-query handling.

### 8.2 Bigram index

For every possible 2-byte sequence, V2 stores an exact bitmap of blocks in which that bigram exists.

Universe:

```text
256 * 256 = 65,536 bigrams
```

For `N` blocks:

```text
BigramBytes = 65,536 * ceil(N / 8)
```

For 10 GiB all-text, 1 MiB blocks:

```text
N = 10,240
BigramBytes = 65,536 * 1,280
             = 83,886,080 bytes
             = 80 MiB
```

Unigram storage is approximately:

```text
256 * 1,280
= 327,680 bytes
~= 0.31 MiB
```

Combined worst-case 10 GiB all-text cost:

```text
~= 80.3 MiB
~= 0.784% of source
```

### 8.3 Query behavior

For a 2-byte query, the exact bigram bitmap is the initial candidate-block set.

For 3+ byte queries, overlapping bigram bitmaps are intersected.

Example:

```text
CreateFileW
 -> Cr
 -> re
 -> ea
 -> at
 -> te
 -> eF
 -> Fi
 -> il
 -> le
 -> eW
```

Candidate blocks are the intersection of these block-presence bitmaps.

### 8.4 Limitation

Bigram presence alone may be weak for common byte pairs.

For a bigram with approximate per-byte occurrence probability `q`, probability of appearing at least once in a 1 MiB block is approximately:

```text
P ~= 1 - exp(-q * 1,048,576)
```

Illustrative values:

| Approx. bigram frequency | Presence in 1 MiB block |
|---:|---:|
| 1 / 10,000 bytes | approximately 100% |
| 1 / 100,000 bytes | approximately 99.997% |
| 1 / 1,000,000 bytes | approximately 65% |
| 1 / 10,000,000 bytes | approximately 10% |

Therefore the exact bigram bitmap is a short-query and first-stage filter, not the sole long-query accelerator.

**Decision: FIXED.**

---

## 9. Global trigram presence

The full byte-trigram universe is:

```text
256^3 = 16,777,216
```

A global corpus-presence bitmap therefore requires:

```text
16,777,216 bits
= 2 MiB
```

V2 SHALL retain this full global-presence bitmap.

If any required query trigram is absent globally, a literal query can terminate immediately with zero matches.

This is especially useful for zero-hit queries.

---

## 10. Sparse rare-trigram anchor index

### 10.1 Purpose

The sparse anchor index is the primary accelerator for selective 3+ byte literal queries when bigram intersections remain broad.

V2 SHALL use **rare byte trigrams**, not a generic 4/5-gram index, as the first sparse-anchor design.

### 10.2 Selection rule

A trigram is eligible for the sparse anchor index when its block document frequency satisfies:

```text
block_df <= 64
```

Only selected rare trigrams store block postings.

Common trigrams do not.

### 10.3 Why df <= 64

With 1 MiB target blocks:

```text
64 blocks * 1 MiB
= 64 MiB
```

Therefore one usable rare anchor limits exact verification to at most roughly 64 MiB of candidate block data before further filtering.

This is a deliberate performance bound for selective queries.

### 10.4 Capacity budget

Sparse anchors SHALL be budget-capped at:

> **<= 1.5% of selected source bytes**

For 10 GiB source:

```text
10 GiB * 1.5%
= 153.6 MiB
```

Selection SHOULD prefer lower block_df values first.

Failure to retain an eligible trigram because the sparse-anchor budget is exhausted MUST NOT create a false negative. The query falls back to bigram filtering plus exact verification.

### 10.5 Illustrative storage model

Suggested compact persistent structures:

- observed trigram bitmap: approximately 2 MiB
- selected trigram bitmap: approximately 2 MiB
- rank metadata: approximately 0.125 MiB
- selected trigram record: approximately 6 bytes
- block postings: delta-varint encoded

Illustrative selected record:

```text
posting_offset : u32
posting_count  : u8
flags          : u8
```

Assuming block IDs fit comfortably within the desktop corpus range and delta-varint postings average approximately 2 bytes/block, approximate bytes per anchor are:

```text
6 + 2 * block_df
```

Illustrative capacity under a 153.6 MiB budget:

| Average block_df | Approx. bytes/anchor | Approx. anchors storable |
|---:|---:|---:|
| 4 | 14 B | ~11.2M |
| 8 | 22 B | ~7.1M |
| 16 | 38 B | ~4.1M |
| 32 | 70 B | ~2.2M |
| 64 | 134 B | ~1.17M |

The total trigram universe is only 16.78M, so this budget can retain a substantial rare subset.

**Decision: FIXED for V2 prototype.**

---

## 11. Exact verification

Candidate filtering never determines final correctness.

Every candidate MUST be exact-verified.

Literal verification SHALL:

- use the requested case mode,
- respect logical-unit boundaries,
- return exact locations,
- produce snippets only from verified matches.

Candidate false positives are acceptable.

Supported literal-search false negatives are not acceptable.

Implementation MAY use SIMD/memmem-like substring search, mmap, buffered reads, decompression, or other optimized verification techniques.

---

## 12. Verification storage strategy

### 12.1 Plain text / source / logs

For file formats where source bytes are directly searchable or cheaply normalized:

- V2 SHALL NOT persist a full duplicate text blob by default.
- Candidate ranges SHOULD be verified directly from the source file using mmap/read or equivalent bounded I/O.

This avoids the previous failure mode of duplicating multi-GiB text into the index.

### 12.2 PDF / Office

PDF and Office files cannot generally be reparsed on every query while preserving the 300 ms SLO.

During indexing:

```text
source document
    -> text extraction
    -> logical units
    -> UTF-8 / NFC normalization
    -> compressed verification blocks
```

The compressed extracted text store SHALL:

- be block-addressable,
- permit independent decompression of candidate regions,
- preserve mappings back to logical document locations,
- avoid requiring whole-document decompression for a local hit.

Exact compression algorithm and sub-block size are implementation/prototype choices, but the design target is roughly 64 KiB–256 KiB independently decodable compression blocks.

### 12.3 Extracted-text capacity risk

Let:

- `E_raw` = extracted searchable text bytes requiring retained verification storage
- `r` = compressed-size ratio

Then:

```text
E_stored = E_raw * r
```

Illustrative examples:

| Extracted text | Compression ratio | Stored cache |
|---:|---:|---:|
| 1 GiB | 25% | 256 MiB |
| 2 GiB | 25% | 512 MiB |
| 3 GiB | 20% | 614 MiB |
| 4 GiB | 20% | 819 MiB |

This store is expected to be the dominant capacity risk for Office/PDF-heavy corpora.

**Decision: compressed block verification store is FIXED; exact codec/block size remains prototype-driven.**

---

## 13. Whole-index capacity budget

Let:

```text
I =
    FileCatalog
  + BlockMap
  + UnigramBigram
  + GlobalTrigramPresence
  + SparseAnchor
  + ExtractedVerificationStore
  + ManifestDeltaReserve
```

Normative rule:

```text
I / SelectedSourceBytes <= 0.10
```

Preferred:

```text
I / SelectedSourceBytes <= 0.05
```

### 13.1 Planning budget

Initial planning budget:

| Component | Planning budget |
|---|---:|
| file catalog + block map | ~0.5% |
| unigram + bigram | ~0.8% worst all-text 10 GiB case |
| sparse trigram anchor | <=1.5% |
| operational/delta reserve | ~0.5% |
| compressed extracted text | remaining budget |
| **hard total** | **<=10%** |

These are planning allocations, not independent hard quotas except where explicitly stated.

### 13.2 Dynamic budget behavior

The builder SHALL treat the 10% limit as a first-class constraint.

If projected capacity approaches the limit:

1. reduce optional sparse-accelerator coverage,
2. adjust representation/compression choices,
3. preserve mandatory correctness structures,
4. do not silently exceed the product SLO.

If the corpus cannot satisfy the budget while preserving required semantics, the system SHALL report an explicit budget-unsatisfied condition rather than silently generating an oversized index.

---

## 14. Capacity simulations

### 14.1 Scenario A: 10 GiB mostly plain text/source

Assume:

- source: 10 GiB
- searchable text: 10 GiB
- extracted retained cache: 0

Illustrative budget:

| Component | Size |
|---|---:|
| unigram + bigram | 80.3 MiB |
| sparse anchors max | 153.6 MiB |
| catalog/block map | 51.2 MiB |
| reserve | 51.2 MiB |
| extracted cache | 0 |
| **total** | **~336 MiB** |
| **ratio** | **~3.28%** |

Result: comfortably inside 10%.

### 14.2 Scenario B: general mixed corpus

Assume:

- source: 10 GiB
- searchable normalized text: 4 GiB
- PDF/Office extracted retained text: 2 GiB
- compression ratio: 25%

Then:

```text
extracted cache ~= 512 MiB
bigram ~= 32 MiB
```

Illustrative total:

```text
~800 MiB
~7.8%
```

Result: PASS.

### 14.3 Scenario C: Office-heavy corpus

Assume:

- retained extracted text: 3 GiB
- compression ratio: 20%

```text
cache ~= 614 MiB
```

Illustrative total:

```text
~894 MiB
~8.7%
```

Result: PASS.

### 14.4 Scenario D: extraction-cache stress

Assume:

- retained extracted text: 4 GiB
- compression ratio: 20%

```text
cache ~= 819 MiB
```

Illustrative total:

```text
~1,107 MiB
~10.8%
```

Result: FAIL.

Therefore V2 does not claim that every arbitrary 10 GiB corpus is mathematically guaranteed to fit within 10%. The 10% value is a product SLO to be verified against defined reference/stress corpora.

The system MUST detect/report an unsatisfied budget rather than silently violating it.

---

## 15. Latency model

### 15.1 Selective-query target

A selected rare trigram with:

```text
block_df <= 64
```

limits candidate verification to at most approximately:

```text
64 MiB
```

Using a deliberately conservative prototype design assumption:

```text
effective exact-verifier throughput = 512 MiB/s
```

then:

```text
64 MiB / 512 MiB/s
= 0.125 s
= 125 ms
```

Illustrative latency budget:

| Stage | Planning budget |
|---|---:|
| query normalization/planning | 5–10 ms |
| bitmap/anchor lookup | <=10 ms |
| candidate management | <=10 ms |
| exact verification | <=125 ms for <=64 MiB selective path |
| snippet/result assembly | <=30 ms |
| contingency/OS/cache variance | remaining budget |
| **total target** | **<=300 ms** |

The 512 MiB/s value is not a guaranteed hardware fact. It is a prototype acceptance assumption that MUST be replaced with measured end-to-end data.

### 15.2 High-hit common queries

Queries such as:

```text
e
the
```

may have broad candidate sets.

They remain practical because the first response stops after reaching the initial result batch.

Illustrative scan volume to find 500 matches:

| Hit density | Scan needed for 500 matches |
|---:|---:|
| 100 / MiB | 5 MiB |
| 10 / MiB | 50 MiB |
| 5 / MiB | 100 MiB |
| 1 / MiB | 500 MiB |

The engine SHALL prioritize returning the first useful batch over computing a complete global result count.

### 15.3 Known hard case

The most difficult class is:

> a long literal whose individual bigrams/trigrams are common, but whose complete sequence is rare or absent.

Such a query may leave a large candidate set despite a low final hit count.

V2 does not claim a mathematical universal <=300 ms proof for adversarial inputs.

Instead:

- adversarial zero-hit/common-gram queries MUST be included in the benchmark suite,
- if defined reference/stress workloads exceed the 300 ms SLO, the architecture MUST be revisited rather than relaxing the SLO by default.

---

## 16. Query execution paths

### 16.1 One-byte query

```text
query
 -> NFC / case fold
 -> exact unigram block bitmap
 -> ordered candidate verification
 -> first result batch
```

### 16.2 Two-byte query

```text
query
 -> exact bigram block bitmap
 -> ordered candidate verification
 -> first result batch
```

### 16.3 Three-or-more-byte literal

```text
query
 -> overlapping bigram intersection
 -> global trigram absence test
 -> choose best available rare trigram anchor
 -> candidate block set
 -> exact verification
 -> first result batch
```

If no selected rare trigram exists, fall back to bigram-filter candidates.

### 16.4 Case-sensitive literal

Candidate generation MAY use the same normalized candidate index.

Final exact verification MUST use case-sensitive original verification text.

### 16.5 Regex

```text
regex
 -> extract mandatory literal(s)
 -> candidate index
 -> linear-time regex verification
 -> first result batch
```

If no suitable mandatory literal exists, the query is outside the guaranteed fast path.

---

## 17. Incremental update model

V2 SHOULD use immutable generations/segments with bounded deltas.

Conceptual model:

```text
Base Generation
    + Delta Segment(s)
    + Tombstones
```

A single file update SHALL NOT require rebuilding the entire persistent index.

Update flow:

```text
changed file
 -> extract/normalize as needed
 -> build replacement block metadata
 -> build local candidate-index contribution
 -> publish delta
 -> tombstone obsolete generation entries
```

Segment/delta count SHALL be bounded to prevent query degradation.

Compaction SHALL run when configured thresholds are reached.

Exact segment count, compaction threshold, and update format remain implementation decisions for the V2 prototype/production design.

---

## 18. Durability and fail-closed behavior

Index publication SHALL use a crash-safe generation model:

```text
build temporary generation
 -> validate
 -> checksum
 -> verify
 -> atomic publish
```

A failed or interrupted build MUST NOT corrupt the previous valid generation.

Malformed/truncated/corrupt index components MUST fail closed.

Candidate-index corruption MUST NOT be silently interpreted as "no match", because that could create false negatives.

---

## 19. Memory policy

V2 SHALL NOT require the full persistent index to be resident in memory.

Preferred memory-resident structures:

- file catalog metadata necessary for immediate filename/path search,
- block metadata,
- small index directories,
- compact presence/rank metadata where beneficial.

Large postings, compressed extracted text, and other payloads SHOULD use mmap or on-demand reads.

Initial product planning target:

- approximately 100–300 MiB normal resident working set for a 10 GiB-class reference corpus,

subject to prototype measurement.

This is a planning target, not yet a normative SLO.

---

## 20. UI/result contract

The search engine SHALL support the agreed Everything-like UI.

For content hits, results are grouped by file, then match location.

Examples:

```text
▼ io_win32.cpp                  4 matches
    128:20  ... CreateFileW(...)
    203:16  ... CreateFileW(...)
    441:9   ... CreateFileW(...)
    + more

▼ Windows_IO_Design.pdf         3 matches
    p.42    ... CreateFileW ...
    p.43    ... CreateFileW ...
    p.88    ... CreateFileW ...

▼ File_API_Spec.docx            2 matches
    §3.2    ... CreateFileW ...
    §7.4    ... CreateFileW ...
```

Result location semantics are format-specific but MUST be stable enough to open/preview the logical hit:

- text/source/log: line + column
- PDF: page + logical text offset where available
- DOCX: paragraph/section mapping
- XLSX: sheet + cell
- PPTX: slide + text object/paragraph where available

---

## 21. Prototype plan

Before production implementation, build a read-only / isolated prototype against representative corpora.

Compare at minimum:

### A. Baseline compact filter

- unigram bitmap
- exact bigram block bitmap
- exact verification

### B. Sparse-trigram design

- A
- global trigram-presence bitmap
- rare trigram block postings
- `block_df <= 64`
- anchor budget <=1.5%

Optional alternatives MAY be added only if A/B fail the SLOs or a clearly superior representation is identified.

---

## 22. Prototype benchmark corpus

The benchmark suite SHALL include:

- source-code-heavy corpus
- log/text-heavy corpus
- Japanese text
- PDF-heavy corpus
- Office-heavy corpus
- mixed desktop corpus
- zero-hit queries
- one-hit rare queries
- high-hit common queries
- 1-byte queries
- 2-byte queries
- 3-byte queries
- long literals
- case-sensitive queries
- case-insensitive queries
- adversarial common-gram / rare-combination queries
- indexable regex queries

The benchmark MUST report both source bytes and extracted searchable bytes.

---

## 23. Prototype metrics

At minimum record:

### Capacity

- selected source bytes
- searchable normalized bytes
- block count
- file catalog bytes
- block-map bytes
- unigram bytes
- bigram bytes
- global trigram presence bytes
- sparse anchor metadata bytes
- sparse anchor posting bytes
- compressed extracted verification bytes
- total persistent index bytes
- `indexBytes / selectedSourceBytes`
- `indexBytes / searchableNormalizedBytes`

### Search

- p50
- p95
- p99
- max observed first-batch latency
- candidate blocks
- candidate bytes
- verification bytes
- verification time
- result assembly time
- files returned
- matches returned
- whether continuation was required

### Correctness

- false negatives: MUST be zero
- false positives after exact verification: MUST be zero
- case semantics
- logical-boundary semantics
- Unicode/NFC semantics
- deterministic result ordering where specified

### Build/update

- full build time
- bytes read
- peak memory
- one-file incremental update time
- resulting delta/index growth

---

## 24. Acceptance gates

A V2 prototype is architecturally acceptable only if it satisfies all mandatory gates.

### Gate A — Correctness

- no false negatives on supported literal search test suite
- no false positives after exact verification
- logical-boundary behavior matches specification
- case behavior matches specification
- malformed/corrupt prototype artifacts fail closed

### Gate B — Capacity

Reference product corpus:

```text
total persistent index / selected source bytes <= 10%
```

Preferred:

```text
<= 5%
```

If >10%, the prototype fails regardless of lookup microbenchmark speed.

### Gate C — Interactive latency

For the defined supported query suite:

```text
first useful batch <= 300 ms
```

Preferred:

```text
<= 100 ms
```

Evaluate p50/p95/p99 and max observed supported-query latency.

### Gate D — No invisible-performance trade

If two designs both satisfy <=300 ms, prefer the materially smaller/simpler design unless the faster design creates a human-visible UX benefit.

The architecture SHALL NOT spend large persistent capacity solely to reduce latency from, for example, 80 ms to 30 ms without a justified product benefit.

---

## 25. Fixed decisions

The following are fixed for the V2 prototype unless new evidence demonstrates that they cannot satisfy the SLOs:

| Area | Decision |
|---|---|
| primary content semantics | literal substring / grep |
| target search block | **1 MiB** |
| short-query filter | exact unigram + bigram block bitmap |
| bigram representation | exact block-presence bitmap |
| global trigram presence | full 2 MiB bitmap |
| sparse anchor | **rare trigram block postings** |
| sparse eligibility | **block_df <= 64** |
| sparse budget | **<=1.5% of selected source bytes** |
| plain text verification | source file; no default durable duplicate |
| PDF/Office verification | compressed extracted text blocks |
| capacity hard SLO | **<=10%** |
| capacity preferred | **<=5%** |
| first-batch latency hard SLO | **<=300 ms** |
| first-batch preferred | **<=100 ms** |
| initial result batch | <=100 files / <=500 matches |
| initial snippets | <=3 per file |
| Unicode | UTF-8 + NFC; no default NFKC |
| case | insensitive default; explicit sensitive |
| cross-unit matching | no |
| regex fast path | mandatory-literal/indexable regex only |

---

## 26. Open implementation choices

The following are intentionally NOT frozen by this architecture document:

- exact persistent file format
- exact compression codec
- exact compressed verification sub-block size within the planned 64–256 KiB range
- exact bitmap compression, if any
- sparse posting integer codec
- block ordering
- candidate scheduling strategy
- mmap vs buffered read details
- SIMD substring implementation
- segment/delta file format
- compaction thresholds
- exact file catalog representation
- exact ranking/sorting policy
- UI implementation technology

These choices MUST be made from benchmark evidence while preserving the fixed semantics and SLOs.

---

## 27. Non-goals for V2 core

The following are not part of the V2 deterministic core architecture:

- semantic similarity search
- embeddings as the primary literal-search mechanism
- LLM-generated final answers
- fuzzy natural-language ranking as a replacement for exact grep
- storing the entire source corpus inside the index
- maximizing microbenchmark lookup speed at the expense of the 10% capacity SLO

These may be layered above the deterministic search engine later.

---

## 28. Architecture decision rule

All future V2 search/index work SHALL be judged in this order:

1. Does it preserve supported-search correctness?
2. Does the whole persistent index remain <=10% of selected source bytes?
3. Does the first useful result batch remain <=300 ms?
4. If multiple designs pass, which is smaller/simpler?
5. Only then: which is faster in internal microbenchmarks?

The core rule is:

> **Do not buy invisible speed with large persistent storage.**

---

## 29. Current status

This document defines the architecture target only.

No production V2 implementation is authorized by this document.

The next engineering step is an isolated prototype and measurement phase, not migration of the existing production format.
