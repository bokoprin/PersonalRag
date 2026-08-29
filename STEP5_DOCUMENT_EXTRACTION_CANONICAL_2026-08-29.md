# PersonalRag V2 Step 5 Document Extraction — Canonical Completion Report

Date: 2026-08-29  
Status: **COMPLETE / CANONICAL / FROZEN**

## Scope completed

Step 5 integrates deterministic local PDF/Office extraction into the frozen Step 1 search semantics, Step 2 persistent content index, and Step 4 incremental model without changing any frozen Step 1–4 persistent identity.

Implemented:

- PDF extraction through configurable `pdftotext`,
- DOCX paragraph extraction,
- XLSX cell extraction including shared/inline strings and formula/cached values,
- PPTX slide/notes paragraph extraction,
- deterministic hard logical-unit boundaries,
- UTF-8 plus UTF-16 BOM-aware OOXML XML decoding,
- immutable `PRV2VER1` v1 verification sidecar bound 1:1 to a content generation,
- per-document zstd payloads and CRC64 integrity checks,
- original source identity validation before exact verification,
- missing/corrupt verification fail-closed generation fallback,
- extraction-aware bundle fallback and GC,
- path-only rename/move reuse of existing extracted verification,
- modify/create lazy re-extraction in the incremental overlay,
- extraction-aware compaction into a new content generation + matching verification sidecar,
- pre-publish Step 5 combined capacity hard gate,
- focused correctness/corruption/recovery/capacity/SLO acceptance tests.

No network/cloud conversion is used. Extraction helper paths are explicit/configurable.

## Frozen identities

Unchanged:

- content: `PRV2IDX1` v2 / semantic `0x0003_0001`
- metadata: `PRV2MET1` v1
- delta: `PRV2DEL1` v1
- incremental state: `PRV2INC1` v1
- bundle: `PRV2BND1` v1
- Unicode: 15.1.0

Added:

- extracted verification: `PRV2VER1` v1 / semantic `0x0003_0001`

`PRV2BND1` is intentionally unchanged; its content-generation reference determines the matching verification sidecar.

## Logical units

- PDF: normalized paragraph/text block, with page boundaries hard
- DOCX: Word paragraph (`w:p`)
- XLSX: non-empty worksheet cell
- PPTX: DrawingML paragraph (`a:p`) from slides and notes

Extracted units are serialized as LF-delimited UTF-8 verification streams. Matches cannot cross unit boundaries. Final matching still uses the frozen Unicode 15.1 NFC/full-fold/wildcard/safe-regex semantics.

## Failure / recovery acceptance

Focused tests prove:

- corrupt newest verification sidecar falls back to a valid older generation,
- bundle loading falls back when its referenced sidecar is corrupt,
- source drift is rejected before exact verification,
- capacity overflow fails before publishing `CURRENT`,
- rename/move reuses bound base verification,
- content modification re-extracts and suppresses stale base content.

Re-extraction is never used to mutate the meaning of an already-built persistent candidate index.

## Capacity / SLO evidence

Controlled document corpus output:

```text
STEP5_SLO selected_bytes=4199696 verification_bytes=1136 combined_ratio=0.021714 cold_ms=8.870 p50_ms=0.047 p95_ms=0.069 p99_ms=0.069 max_ms=8.870 candidate_blocks=1 candidate_bytes=176 verification_scan_bytes=176
```

The measured Step 5 generation ratio (`PRV2IDX1 + PRV2VER1`) is **2.1714%**, below both the 5% preferred target and 10% hard gate. First useful batch max/cold is **8.870 ms**, below both the 100 ms preferred and 300 ms hard latency limits.

The global product rule in `docs/PERFORMANCE_SLO.md` still requires metadata/delta/manifests/reserve to be included in whole-product footprint acceptance. That cross-component target-Windows measurement remains Step 7 scope; Step 5 does not waive or redefine it.

## Final Rust gate

Rust 1.97.1:

- `cargo fmt -- --check`: **PASS**
- `cargo clippy --offline --locked --all-targets -- -D warnings`: **PASS**
- `cargo test --offline --locked`: **78/78 PASS**
- `cargo build --offline --locked --release`: **PASS**
- document extraction focused suite: **9/9 PASS**

Evidence is sealed under `evidence/step5-document/`.

## Environment boundary

Canonical Linux-host acceptance used:

- Poppler `pdftotext` 25.06.0
- Info-ZIP `unzip` 6.00
- `zstd` helper

Windows packaging must supply compatible pinned helper binaries/paths and is validated in Step 7. The existing live NTFS/USN Windows acceptance caveat also remains Step 7 scope.

## Next

Step 6: Everything-style GUI with a second file-content search field, consuming the frozen Step 1–5 backend contracts.
