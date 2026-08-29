# PersonalRag V2 Step 7 Last-Two Fix Wave

Date: 2026-08-30  
Status: **LAST-TWO FIX COMPLETE / CANONICAL CANDIDATE — real-machine targeted closure pending**

## Scope

This wave intentionally changes only the two residual verification defects left by the native-Windows Step 7 report commit `2ee23681f9f1a09864c421fa9e974fb003ea84af`:

1. `tests/document_extraction.rs` depended on Unix-style `zip.exe` to build OOXML fixtures on Windows.
2. `tools/measure_product_capacity.ps1` used generic `[Array]::Fill[byte](...)` syntax that Windows PowerShell 5.1 cannot parse, and its MiB input lacked a hard safety bound.

No product source under `src/`, frozen format, deterministic search semantic, Cargo dependency, or product lifecycle behavior is changed.

## Fix 1 — Windows document fixture generation

The document-extraction test fixture builder now uses:

- Windows: native `tar.exe -a -c -f`
- non-Windows: existing `zip -q -r`

A Windows-only focused regression verifies that the generated OOXML archive actually contains `word/document.xml`.

This removes the test-only requirement for a separate `zip.exe` on Windows. It does not change the product extractor, which already uses the native Windows ZIP-reader path.

## Fix 2 — Windows PowerShell 5.1 capacity tool

`tools/measure_product_capacity.ps1` now:

- parses `MiB` values explicitly from either array arguments or comma-separated text,
- accepts only 1–1024 MiB per requested size,
- accepts at most 16 requested sizes,
- rejects malformed/oversized requests before creating any capacity directory,
- fills the reusable 1 MiB byte buffer without PowerShell generic-method syntax,
- remains compatible with PowerShell 7.

The safety bound specifically prevents a malformed `496256` request from generating tens of gigabytes of temporary data.

## Gate 0

GitHub Actions run: **33279610286** — PASS.

Before source changes:

- canonical `SOURCE_MANIFEST.sha256`: PASS,
- Linux fmt/clippy/full regression/release: PASS,
- Linux regression: 88/88 PASS,
- Windows fmt/clippy: PASS,
- Windows GUI: 2/2 PASS,
- Windows incremental: 19/19 PASS,
- Windows product lifecycle: 1/1 PASS,
- Windows watch: 1/1 PASS,
- Windows document helper: 1/1 PASS,
- Windows release build: PASS,
- the PowerShell 5.1 parser defect was reproduced with non-zero exit.

## Focused validation

GitHub Actions run: **33279766770** — PASS.

- Linux document extraction: **9/9 PASS**.
- Windows native fixture ZIP focused regression: **1/1 PASS**.
- Windows PowerShell 5.1 capacity command accepted exactly `4,96,256`.
- Complete-store ratios:
  - 4 MiB: **5.087256% PASS**
  - 96 MiB: **2.659954% PASS**
  - 256 MiB: **2.637484% PASS**
- Oversized `496256` request: rejected before capacity-data creation.

## Full regression

GitHub Actions run: **33279968873** — PASS.

Linux, Rust/Cargo 1.97.1:

- fmt: PASS
- clippy `-D warnings`: PASS
- full regression: **88/88 PASS**
- release build: PASS

Native Windows, Rust/Cargo 1.97.1:

- test-only Poppler/Zstandard executables made available to the runner,
- fmt: PASS
- clippy `-D warnings`: PASS
- full `cargo test --offline --locked`: **89/89 PASS**
- `document_extraction`: **10/10 PASS**
- Windows PowerShell 5.1 capacity 4/96/256 MiB: PASS
- complete-store ratios remained 5.087256% / 2.659954% / 2.637484%
- release build: PASS

## Acceptance boundary

The previous real-machine report already passed the product paths:

- fresh source integrity,
- native Win32 GUI,
- init,
- explicit incremental update,
- normal-user live watch,
- real PDF/DOCX/XLSX/PPTX product GUI search,
- restart/recovery,
- whole-store capacity.

Because this wave changes only a test fixture builder and a verification PowerShell tool, the final Codex closure run is intentionally targeted. It must use a fresh clone, prove the product-source diff remains absent, run the Windows full test gate including `document_extraction`, and run the capacity script under Windows PowerShell 5.1 including the oversized-input rejection.

Instructions: `STEP7_WINDOWS_TARGETED_CLOSURE_CODEX_2026-08-30.md`.
