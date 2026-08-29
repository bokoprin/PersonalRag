# PersonalRag V2 Step 5 Document Extraction and Verification Store

Date: 2026-08-29
Status: **STEP 5 COMPLETE / FROZEN**

## Scope

Step 5 adds deterministic local extraction for PDF, DOCX, XLSX and PPTX without changing the frozen Step 1-4 identities:

- `PRV2IDX1` v2 / semantic `0x0003_0001`
- `PRV2MET1` v1
- `PRV2DEL1` v1
- `PRV2INC1` v1
- `PRV2BND1` v1

The new derived verification sidecar is `PRV2VER1` v1. A sidecar generation is named from, and bound one-to-one to, the content generation it verifies.

## Process boundary

Extraction is local/offline and uses explicit helper-process boundaries:

- PDF: Poppler `pdftotext`
- OOXML ZIP access: ZIP-reader process (`unzip` on the original Step 5 host; native Windows `tar.exe` is accepted as an equivalent transport)
- verification compression: `zstd`

The helper paths are configurable. Step 7 final stabilization changes only the Windows ZIP transport: native `tar.exe` is preferred and Git/MSYS `unzip.exe` is not auto-selected. This does not change logical-unit semantics, verification bytes, or any frozen persistent identity. Missing `pdftotext`/`zstd` helpers remain explicit errors. No network service or cloud conversion is permitted.

## Logical units

Hard boundaries remain authoritative under the frozen Step 1 semantics.

- plain text/source/log: one source line (unchanged)
- PDF: one normalized paragraph/text block; page boundaries always break units
- DOCX: one Word paragraph (`w:p`)
- XLSX: one non-empty worksheet cell; formula and visible/cached value are kept within that cell unit
- PPTX: one DrawingML paragraph (`a:p`) from slides and notes

Extracted units are UTF-8, have internal CR/LF collapsed to spaces, and are serialized as one LF-delimited verification stream. Therefore no search can cross a document logical-unit boundary.

## Verification store

`verify-%020u.prv2ver` contains:

- magic/version/semantic identity
- bound content generation
- extractor format revision
- one record per extracted source file, keyed by content-internal file id
- source size/CRC64/mtime identity
- extractor fingerprint CRC64
- logical-unit count
- uncompressed/compressed lengths
- uncompressed and compressed CRC64 values
- independent zstd-compressed payloads
- whole-file CRC64 footer

The store is immutable. Search validates the original source metadata and verifies candidate hits against the exact decompressed bytes used to build that content generation.

## Corruption and recovery

A content generation requiring extraction is usable through the Step 5 loader only when its matching verification sidecar is structurally and cryptographically valid. `load_latest_with_verification` checks the advisory current generation first and then older immutable generations; a missing/corrupt sidecar causes fail-closed fallback rather than re-extracting bytes behind an existing candidate index.

Re-extraction is allowed only while building a new content generation or a changed-content incremental cache. This prevents extractor-version drift from introducing candidate false negatives.

## Capacity

The hard product metric is unchanged:

`(PRV2IDX1 bytes + PRV2VER1 bytes) / selected source bytes <= 10%`.

The Step 5 publisher measures the combined footprint before publishing `CURRENT` and rejects an over-budget generation. Verification text is independently zstd-compressed per source document.

## Incremental rules

- path-only rename/move: reuse the base content internal id and its bound verification payload; only source verification path is overridden
- content modify/create: suppress stale base content and extract the changed document into the lazy delta content cache
- repeated query without a content mutation: reuse the lazy changed-content cache
- compaction: build a new content generation and matching verification sidecar before the new content generation becomes current

`PRV2BND1` remains unchanged. Step 5 bundle-aware loading binds the verification sidecar through the referenced content generation number rather than adding a fifth field to the frozen bundle format.

## Exactness

Candidate filtering never decides the result. Literal/wildcard/regex final verification uses the same Unicode 15.1 NFC/full-fold/NFA logic as plain text, against the exact extracted logical-unit bytes stored in `PRV2VER1`.

## Canonical acceptance

The Step 5 implementation is accepted only through the extraction-aware APIs. Legacy/plain-text APIs remain available for backward compatibility and do not silently invoke document extraction.

Canonical Step 5 acceptance on 2026-08-29:

- PDF/DOCX/XLSX/PPTX logical-unit extraction: PASS
- exact verification from `PRV2VER1`: PASS
- Unicode 15.1 NFC/full-fold semantics on extracted text: PASS
- path-only rename reuse and modify/create re-extraction: PASS
- missing/corrupt sidecar fail-closed fallback: PASS
- source-drift rejection: PASS
- Step 5 generation capacity hard gate: PASS
- controlled first-batch SLO: PASS (max 8.870 ms)
- final Rust 1.97.1 full gate: 78/78 tests PASS, fmt/clippy/release PASS

The measured Step 5 generation ratio is `PRV2IDX1 + PRV2VER1` divided by selected source bytes. The cross-component whole-product footprint (metadata/delta/bundle reserve included) remains an explicit Step 7 target-Windows E2E acceptance metric, as required by `PERFORMANCE_SLO.md`; Step 5 does not weaken that product-level gate.
