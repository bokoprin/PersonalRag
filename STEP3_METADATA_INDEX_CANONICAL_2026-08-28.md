# PersonalRag V2 Step 3 — Filename/Path Metadata Index

Date: 2026-08-28  
Result: **COMPLETE / FROZEN**

## Goal

Implement the Everything-style filename/full-path search backend as a lightweight metadata path independent of the content index.

## Implemented

- separate `PRV2MET1` immutable metadata snapshot
- filename substring search
- full-path substring search
- filename + path AND
- Unicode 15.1 NFC/full-case-fold semantics shared with frozen Step 1
- separator-equivalent path queries
- stable FileID and exact path identity
- size, modified time, file kind, source root, content/extraction flags
- exact q3 absence filtering
- budgeted rare q3 postings
- q4/q5 Bloom absence filtering
- budgeted rare q4/q5 postings
- exact final verification
- first-batch early termination
- CRC64-ECMA and format/semantic validation
- lossless Unix non-UTF8 identity round-trip
- compact/lazy load representation

The content index is not used by metadata-only search.

## Correctness

Focused tests cover:

- independent Unicode oracle for filename/path queries
- `Straße` / `STRASSE`
- NFC-equivalent decomposed filename
- Japanese filename/path
- case-sensitive behavior
- filename/path AND
- q3 absence
- q4/q5 candidate path without changing exact result
- persistent snapshot reload
- checksum corruption
- format/semantic mismatch
- duplicate FileID rejection
- non-UTF8 Unix path identity

Final full regression after integration is recorded in `STATE.json` and HANDOFF.

## Performance acceptance

The final one-million-record test deliberately includes the cases that cannot use q3/q4/q5 filtering:

- `~`: one-character zero-hit, verifies all **1,000,000** records
- `@@`: two-character zero-hit, verifies all **1,000,000** records

Results:

- one-char zero-hit: p50 **65.013 ms**, max **190.133 ms**
- two-char zero-hit: p50 **65.306 ms**, max **142.278 ms**

Both remain below the 300 ms hard first-batch target even when no hit allows early termination.

For common short queries, first 100 results are returned in roughly 0.01 ms because verification stops after the first batch.

At one million records:

- snapshot: **185.74 MB** / **185.743 bytes per file**
- load-only: **851.5 ms**
- steady RSS: **~206 MiB**
- rare filename: **0.009 ms p50**
- Unicode filename: **0.006 ms p50**
- path: **0.096 ms p50**
- filename/path AND: **0.129 ms p50**

## Review findings resolved during Step 3

- variable posting offset/end decoding corrected
- q4/q5 rare postings treated as opportunistic acceleration rather than correctness requirements
- eager posting expansion removed from load path
- folded filename/path steady-state duplication removed
- direct verification added for byte-preserving paths
- full-scan 1/2-character zero-hit cases added to the acceptance benchmark

No known correctness blocker remains in Step 3.

## Decision

Step 3 is frozen. The next roadmap item is **Step 4: Windows incremental indexing**.
Step 4 must connect Windows filesystem discovery/change tracking to `PRV2MET1` and the frozen content index without changing Step 1/2/3 semantics unless an explicit versioned change is justified.
