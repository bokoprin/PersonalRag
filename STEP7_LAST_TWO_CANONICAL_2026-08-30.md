# PersonalRag V2 Step 7 Last-Two Closure

Date: 2026-08-30  
Status: **IMPLEMENTATION / CI COMPLETE — targeted real-machine closure pending**

## Scope

This wave changes only the two residual verification defects reported by native-Windows E2E commit `2ee23681f9f1a09864c421fa9e974fb003ea84af`:

1. `tests/document_extraction.rs` required an external `zip.exe` only to build OOXML test fixtures.
2. `tools/measure_product_capacity.ps1` was not Windows PowerShell 5.1 compatible and accepted an accidentally concatenated gigantic MiB request.

No `src/` product implementation file and no Cargo dependency/format definition is changed by this wave.

## Fix 1 — Windows document fixture portability

The test fixture generator is now platform-specific:

- Windows: native `tar.exe -a -c -f`
- non-Windows: existing `zip -q -r`

A Windows-only focused test creates a DOCX fixture with native `tar.exe`, lists it again with native `tar.exe`, and verifies the expected OOXML entry exists.

This removes the test-only dependency on a separately installed `zip.exe`; it does not alter production document extraction semantics.

## Fix 2 — Windows PowerShell 5.1 capacity tool

`tools/measure_product_capacity.ps1` now:

- parses comma-separated and array MiB arguments explicitly,
- runs under Windows PowerShell 5.1 without `[Array]::Fill<T>`,
- constructs the fill buffer using PowerShell-5.1-compatible APIs,
- rejects MiB values outside 1..1024 before creating capacity data,
- rejects excessive request counts,
- prints normalized `CAPACITY_REQUEST` before measurement.

The previously dangerous accidental `496256` request is therefore rejected before any temporary capacity tree is created.

## Gate evidence

### Pre-change Gate 0

GitHub Actions run **33279610286**: PASS.

- Linux source manifest: PASS
- Linux fmt: PASS
- Linux clippy with warnings denied: PASS
- Linux full regression: PASS
- Linux release build: PASS
- Windows fmt: PASS
- Windows clippy with warnings denied: PASS
- existing GUI/incremental/product-lifecycle/watch/document-helper regressions: PASS
- Windows release build: PASS
- old PowerShell-5.1 capacity-script defect: reproduced as baseline

### Focused validation

GitHub Actions run **33279766770**: PASS.

- Linux document extraction regression: 9/9 PASS
- Windows native fixture ZIP focused regression: PASS
- Windows PowerShell 5.1 capacity measurement: PASS
  - 4 MiB complete ratio: **5.087256%**
  - 96 MiB complete ratio: **2.659954%**
  - 256 MiB complete ratio: **2.637484%**
- oversized `496256` MiB request: rejected before data creation

### Full regression

GitHub Actions run **33279968873**: PASS.

Linux, Rust/Cargo 1.97.1:

- fmt: PASS
- clippy with warnings denied: PASS
- full regression: **88/88 PASS**
- document extraction: 9/9 PASS
- release build: PASS

Native Windows, Rust/Cargo 1.97.1:

- fmt: PASS
- clippy with warnings denied: PASS
- full regression: **89/89 PASS**
- document extraction: **10/10 PASS**
- Windows document helper: PASS
- Windows watch: PASS
- Windows PowerShell 5.1 capacity 4/96/256 MiB: PASS
- release build: PASS

## Acceptance boundary

The previous real-machine E2E already passed all product paths except `S7-DOC-002`, including GUI, init, live normal-user watch, explicit update, real PDF/DOCX/XLSX/PPTX product search, restart/recovery, source manifest, and capacity.

Because this wave changes only tests/tools/documentation/evidence and no product `src/` implementation, Step 7 final closure uses the targeted native-Windows procedure:

`STEP7_WINDOWS_TARGETED_CLOSURE_CODEX_2026-08-30.md`

If its diff guard confirms there is no unexpected product implementation/Cargo change and the targeted Windows checks pass, the retained previous product-path PASS results plus the new closure evidence are sufficient to declare **STEP7_COMPLETE**.
