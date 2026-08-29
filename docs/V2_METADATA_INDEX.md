# PersonalRag V2 Metadata Index Specification

Date: 2026-08-28  
Status: **FROZEN — Step 3 COMPLETE**

## Purpose

This format is the lightweight filename/full-path search path used by the Everything-style product UI.
It is deliberately independent of the content index: filename/path-only queries MUST NOT load or query `PRV2IDX1` content-index structures.

## Identity

- magic: `PRV2MET1`
- metadata format version: **1**
- search semantic id: **`0x0003_0001`**
- Unicode semantics: same frozen Step 1 semantics as content search
  - Unicode 15.1.0
  - case-sensitive: NFC
  - case-insensitive: NFC -> full default Unicode case fold -> NFC
  - NFKC is not used
- snapshot model: immutable; existing snapshot paths are never silently overwritten
- checksum: whole-payload CRC64-ECMA

A loader MUST reject unknown format versions, unknown search-semantic IDs, checksum mismatch, malformed ranges, duplicate FileIDs, and malformed posting metadata.

## Record model

Every metadata record contains:

- stable `file_id`
- exact filesystem path identity
- `source_root`
- size
- modified timestamp
- file kind
- content-searchable flag
- extractable flag

Path identity is lossless:

- Windows: exact UTF-16LE path units
- Unix: raw path bytes

Non-UTF8 Unix paths are retained as identity but are not text-searchable.

## Query model

Supported deterministic fields:

- filename substring
- full-path substring
- filename AND full-path
- case-sensitive / case-insensitive

For path matching, `/` and `\\` are treated as equivalent separators for query purposes. The stored identity is not rewritten.

The search returns the first requested batch and stops verification as soon as the batch is full.
The default first batch is **100** records.

## Candidate index

Filename and full path each have their own compact field index.

Candidate filtering uses:

1. exact adaptive global q3 presence
2. budgeted rare q3 postings
3. q4/q5 global Bloom presence used only for safe absence checks
4. budgeted rare q4/q5 postings
5. exact verification against the original metadata record

Candidate false positives are allowed. Supported-query false negatives are not.
A case-sensitive exact hit must also be a member of the case-folded candidate set, so the folded candidate index is safe for both modes.

### 1-2 character queries

No q3 anchor exists. They therefore scan metadata records directly, but stop immediately when the requested first batch is full.
This is intentional and is covered by the one-million-record worst-case acceptance measurement.

## Persistent layout

Header size: **64 bytes**.

The snapshot contains:

1. fixed-size metadata record table
2. exact path blob
3. filename field index
4. full-path field index
5. CRC64-ECMA payload checksum in the header

Posting blobs are decoded lazily at query time. The loader does not eagerly expand all postings into HashMaps.
Folded filename/path strings are build-time temporaries and are not retained as duplicate steady-state copies.

## Publication

`write_snapshot`:

1. refuses to overwrite an existing final snapshot
2. serializes the immutable snapshot
3. writes a sibling temporary file
4. `sync_all` on the file
5. renames the temporary file to the final path
6. syncs the containing directory where supported

Windows incremental generation/catalog publication is Step 4 work; Step 3 defines the immutable metadata snapshot itself.

## Acceptance evidence

Normative release/CPU-affinity evidence:

### 100,000 records

- persistent bytes: **19,591,691**
- bytes/file: **195.917**
- build: **1.930 s**
- publish: **174.5 ms**
- load: **89.5 ms**
- one-char zero-hit full scan: p50 **7.091 ms**, max **11.374 ms**
- two-char zero-hit full scan: p50 **7.476 ms**, max **23.888 ms**

### 1,000,000 records

- persistent bytes: **185,743,016**
- bytes/file: **185.743**
- build: **16.473 s**
- publish: **2.086 s**
- load during full benchmark: **813.8 ms**
- load-only: **851.5 ms**
- steady RSS after load: **211,240 KiB** (~206 MiB)
- rare filename: p50 **0.009 ms**
- Unicode `STRASSE` -> `Straße`: p50 **0.006 ms**
- q3 zero-hit: approximately **0 ms**, 0 candidates
- common one-char first 100: p50 **0.011 ms**
- common two-char first 100: p50 **0.008 ms**
- one-char zero-hit full 1,000,000-record scan: p50 **65.013 ms**, max **190.133 ms**
- two-char zero-hit full 1,000,000-record scan: p50 **65.306 ms**, max **142.278 ms**
- path query: p50 **0.096 ms**
- filename + path AND: p50 **0.129 ms**

All measured first-batch and deliberate full-scan worst cases are below the **300 ms hard target** on the affinity-pinned Linux test host. Windows product acceptance remains Step 7.

Normative evidence files:

- `evidence/step3-metadata/metadata-100k-seal.txt`
- `evidence/step3-metadata/metadata-1m-seal.txt`
- `evidence/step3-metadata/metadata-1m-load-seal.txt`

## Frozen boundary

Step 4 may add Windows discovery/change tracking and atomic catalog replacement around this snapshot, but MUST NOT silently change:

- filename/path Unicode semantics
- `PRV2MET1` format version 1 layout/meaning
- stable FileID semantics once chosen by the Windows adapter
- exact path identity requirements

An incompatible change requires a new metadata format version and explicit rejection/migration tests.
