# PersonalRag V2 — Steps 1–6 deterministic desktop search

Date: 2026-08-29  
Status: **Steps 1–6 COMPLETE / Step 6 GUI FROZEN**

This repository contains the PersonalRag V2 deterministic search backend and the Step 6 Windows Everything-style GUI. The removed legacy implementation must not be restored as a compatibility layer.

## Frozen completed steps

1. **Content-search semantics** — Unicode 15.1 NFC/full case-fold, literal, wildcard, safe regex, exact verification.  
   Spec: `docs/V2_SEARCH_SEMANTICS.md`
2. **Persistent content index** — `PRV2IDX1` v2, Variant-D q3/q4/q5 filtering, immutable generations, CRC/fallback/GC.  
   Spec: `docs/V2_PERSISTENT_FORMAT.md`
3. **Everything-style filename/path index** — independent `PRV2MET1` v1 metadata snapshot, million-file first-batch/full-scan acceptance.  
   Spec: `docs/V2_METADATA_INDEX.md`
4. **Windows incremental indexing** — base+delta overlay, USN/reconciliation state, crash-safe bundle commit, no per-change base rebuild.  
   Spec: `docs/V2_INCREMENTAL_INDEX.md`

Step 4 adds `PRV2DEL1` v1, `PRV2INC1` v1, and `PRV2BND1` v1 without changing frozen Step 1/2/3 identities.

5. **PDF / Office extraction and verification store** — deterministic PDF/DOCX/XLSX/PPTX logical-unit extraction, immutable `PRV2VER1` v1 exact-verification sidecars, corruption fallback, and Step 4 incremental reuse/re-extraction integration.  
   Spec: `docs/V2_DOCUMENT_EXTRACTION.md`

Step 5 adds `PRV2VER1` v1 without changing the frozen Step 1–4 identities.

6. **Everything-style Windows GUI** — filename/path field, independent content field, literal/regex/wildcard mode, case mode, grouped content hits, preview, open/reveal, bundle reload, asynchronous search worker, and progressive result continuation.  
   Spec: `docs/V2_GUI.md`

Step 6 does not add or alter any persistent format identity. It consumes the frozen Step 1–5 bundle contracts.

## Step 4 final controlled evidence

One-million-record base, 10,000 create + 10,000 rename + 10,000 delete:

- create: **5.789 ms**
- rename: **8.679 ms**
- delete: **2.323 ms**
- 30,000-change delta: **1,910,064 bytes**
- delta publish: **20.633 ms**
- delta reload: **131.341 ms**
- incremental metadata searches: p50 **2.816–3.187 ms**

The old renamed path and deleted record return 0 hits.

See `STEP4_INCREMENTAL_INDEX_CANONICAL_2026-08-29.md` and `evidence/step4-incremental/`.

## Step 5 final controlled evidence

Controlled document corpus acceptance:

- PDF/DOCX/XLSX/PPTX extractor focused acceptance: **9/9 PASS**
- final full Rust regression: **78/78 PASS**
- combined Step 5 generation ratio (`PRV2IDX1 + PRV2VER1`): **2.1714%**
- first useful content batch: cold/max **8.870 ms**, p50 **0.047 ms**, p95/p99 **0.069 ms**
- source-drift rejection, corrupt verification fallback, bundle fallback, capacity hard gate: **PASS**

See `STEP5_DOCUMENT_EXTRACTION_CANONICAL_2026-08-29.md` and `evidence/step5-document/`.

## Development gate

Rust **1.97.1**:

```bash
cargo fmt -- --check
cargo clippy --offline --locked --all-targets -- -D warnings
cargo test --offline --locked
cargo build --offline --locked --release
```

The sealed result is recorded in `STATE.json` and `HANDOFF.md`.

## Environment boundary

The Windows USN adapter is implemented and forced-`cfg(windows)` type-checked, while USN parsing/state transitions are tested with synthetic records. A live NTFS/USN E2E run was not possible on the Linux execution host and is **not counted as PASS**; that remains part of Step 7 target-Windows product acceptance.

## Step 6 GUI

Windows GUI binary: `personalrag-v2-gui`

```text
personalrag-v2-gui --root <indexed-root> --store <index-store> [--pdftotext <path>] [--unzip <path>] [--zstd <path>]
```

The same paths may be supplied through `PERSONALRAG_ROOT` and `PERSONALRAG_STORE`. The GUI loads the frozen Step 1–5 bundle fail-closed, searches on a background worker, debounces live input, displays filename/path and content results, shows up to three snippets per file, and can open a result or reveal it in Explorer. `More` progressively expands the current result enumeration from the 100-file first batch.

The Step 6 Windows-only source is forced-`cfg(windows)` type-checked on the Linux acceptance host. A real Windows window launch, ShellExecute/Explorer integration, pinned helper packaging, live NTFS/USN behavior, DPI/keyboard usability, and target-Windows latency/footprint acceptance are **not counted as PASS here**; they are Step 7.

## Current product status

The deterministic filename/path + content search application source is complete through the desktop GUI boundary. The next roadmap item is:

7. target-Windows E2E / performance / failure / usability acceptance
8. V2 1.0

Semantic/LLM search remains deferred until deterministic Windows product acceptance is complete.
