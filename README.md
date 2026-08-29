# PersonalRag V2 — deterministic Windows desktop search

Date: 2026-08-30  
Status: **Steps 1–6 FROZEN / Step 7 last-two fix COMPLETE / target-Windows targeted closure pending**

This repository contains the PersonalRag V2 deterministic search backend, native Win32 Everything-style GUI, and Step 7 product index lifecycle used to create/update/watch a Windows index store. The removed legacy implementation must not be restored as a compatibility layer.

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

The Windows watcher prefers the NTFS USN Journal when raw-volume access is available. Under a normal non-elevated token, raw USN access may be denied by Windows; the product now falls back to recursive Win32 directory-change notifications and still performs deterministic reconcile/publish. Final acceptance must verify the actual watch mode and live searchable updates on the user's Windows machine.

## Step 6 GUI

Windows GUI binary: `personalrag-v2-gui`

```text
personalrag-v2-gui --root <indexed-root> --store <index-store> [--pdftotext <path>] [--unzip <path>] [--zstd <path>]
```

The same paths may be supplied through `PERSONALRAG_ROOT` and `PERSONALRAG_STORE`. The GUI loads the frozen Step 1–5 bundle fail-closed, searches on a background worker, debounces live input, displays filename/path and content results, shows up to three snippets per file, and can open a result or reveal it in Explorer. `More` progressively expands the current result enumeration from the 100-file first batch.

Step 6 itself remains frozen. Step 7 stabilization adds product wiring around it without changing Step 1–5 durable identities.

## Step 7 product lifecycle

Windows lifecycle binary: `personalrag-v2-indexer`

```text
personalrag-v2-indexer init   --root <indexed-root> --store <index-store> [helper overrides]
personalrag-v2-indexer update --root <indexed-root> --store <index-store> [helper overrides]
personalrag-v2-indexer watch  --root <indexed-root> --store <index-store> [--interval-ms 250] [--once] [helper overrides]
personalrag-v2-indexer status --root <indexed-root> --store <index-store> [helper overrides]
personalrag-v2-indexer helpers
```

`init` creates and verifies a fresh Step 1–5 bundle. `update` explicitly reconciles filesystem state. On native Windows, `watch` prefers the NTFS USN Journal and automatically falls back to non-elevated recursive Win32 directory notifications when raw-volume access is unavailable; either mode triggers deterministic reconciliation/publish. `WATCH_READY` reports the selected mode and fallback reason. `status` verifies and reports the current bundle. See `docs/V2_PRODUCT_LIFECYCLE.md`.

PDF extraction uses `pdftotext`; verification compression uses `zstd`. On Windows, OOXML ZIP access prefers the built-in native `tar.exe` and deliberately does not auto-select Git/MSYS `unzip.exe`. `tools/setup_windows_helpers.ps1` reports helper availability and can provision Poppler/zstd through WinGet when explicitly invoked with `-Install`. Third-party helper binaries are not stored in this repository.

`.gitattributes` forces canonical text checkout to LF. `tools/verify_source_manifest.ps1` verifies canonical hashes and also accepts only an exact CRLF→LF normalization for legacy Git-clean Windows worktrees created before the LF rule; any real content change still fails.

The native-Windows Step 7 runs have now passed the product GUI, init/update/watch, real PDF/Office search, restart/recovery, manifest, helper, and 4/96/256 MiB capacity paths. The final residual verification defects were test-only `zip.exe` fixture generation and Windows PowerShell 5.1 incompatibility in the capacity script; both are fixed and pass Linux/Windows CI, including Windows full `cargo test`. Because no product `src/` code changed in this last wave, only a targeted real-machine closure is required. The Codex procedure is `STEP7_WINDOWS_TARGETED_CLOSURE_CODEX_2026-08-30.md`.

## Current product status

The deterministic engine, GUI, and runnable index lifecycle are implemented. The remaining roadmap is:

7. target-Windows targeted closure for the final test/tool fixes
8. V2 1.0

Semantic/LLM search remains deferred until deterministic Windows product acceptance is complete.
