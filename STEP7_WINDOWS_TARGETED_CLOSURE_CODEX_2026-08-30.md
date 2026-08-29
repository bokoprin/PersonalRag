# PersonalRag Step 7 Windows Targeted Closure — Codex Instructions

Date: 2026-08-30  
Purpose: close the two final verification defects after the full real-machine E2E report `2ee23681f9f1a09864c421fa9e974fb003ea84af`.

## 1. Why this is targeted

The previous real-machine E2E already passed:

- `S7-BUILD-001`
- `S7-BUILD-002`
- `S7-GUI-001`
- `S7-INCREMENTAL-001`
- `S7-INIT-001`
- `S7-USN-001`
- `S7-DOC-001`
- `S7-CAPACITY-001`

and also passed real PDF/DOCX/XLSX/PPTX product GUI search, restart, fail-closed recovery, and normal-user live watch.

The only unresolved Step 7 issue was `S7-DOC-002`: the repository test fixture generator required a separate `zip.exe`. A second verification-tool anomaly was Windows PowerShell 5.1 incompatibility in `tools/measure_product_capacity.ps1`.

The new canonical source is expected to differ from the previous product-E2E source only in tests/tools/documentation/evidence. If `src/`, Cargo dependency files, or frozen-format/product implementation files changed unexpectedly, do not use this targeted procedure; report `TARGETED_RETEST_INVALID`.

## 2. Hard rules

- Run on the user's real native Windows machine.
- Use a brand-new disposable clone of latest GitHub `main`.
- Do not modify source, tests, Cargo files, specifications, or `SOURCE_MANIFEST.sha256`.
- Do not repair failures during verification.
- No unexecuted item may be called PASS.
- Do not rerun the entire manual GUI/watch matrix unless the source-diff guard below shows product implementation changed.
- Reports only may be pushed to `reports/step7-windows-closure-<timestamp>`; never push/merge `main`.

## 3. Fresh clone and source seal

```powershell
git clone https://github.com/bokoprin/PersonalRag.git PersonalRag-Step7-Closure
cd PersonalRag-Step7-Closure
git switch main
git pull --ff-only origin main
git rev-parse HEAD
git rev-parse HEAD^{tree}
git status --porcelain
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\verify_source_manifest.ps1
```

Required: clean checkout and manifest PASS.

## 4. Product-source diff guard

Previous full real-machine E2E product source:

```text
a33a32a81a344cdbdeee14431fe71a159afe2471
```

Run:

```powershell
git diff --name-status a33a32a81a344cdbdeee14431fe71a159afe2471..HEAD
```

Expected functional changes are limited to:

- `tests/document_extraction.rs`
- `tools/measure_product_capacity.ps1`
- final Step 7 documentation / AGENTS / evidence / manifest

There must be **no unexpected `src/` change and no Cargo dependency/format change**. If such a change exists, stop and report `TARGETED_RETEST_INVALID`.

## 5. Toolchain and helpers

Required Rust/Cargo: 1.97.1.

```powershell
rustc --version
cargo --version
git --version
.\target\release\personalrag-v2-indexer.exe helpers
```

If release binaries do not exist yet, build them first.

The previous E2E already installed/located Poppler, native Windows `tar.exe`, and Zstandard. If a required helper is missing, the user authorizes only:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File .\tools\setup_windows_helpers.ps1 -Install
```

Do not install a separate `zip.exe`; this retest specifically verifies it is no longer required.

## 6. Final Windows build/test gate

Run exactly:

```powershell
cargo fmt --all -- --check
cargo clippy --offline --locked --all-targets -- -D warnings
cargo build --offline --locked --release
where.exe zip
cargo test --offline --locked
cargo test --offline --locked --test document_extraction -- --nocapture
cargo test --offline --locked --test windows_document_helper -- --nocapture
```

`where.exe zip` may return not found; that is acceptable and desirable evidence that the repository test no longer depends on a separate `zip.exe`.

Acceptance:

- fmt PASS,
- clippy PASS,
- release build PASS,
- full `cargo test` has zero failures,
- Windows `document_extraction` is expected to report **10/10 PASS**,
- `windows_document_helper` PASS,
- no failure may be attributed to missing `zip.exe`.

Re-evaluate `S7-DOC-002`.

## 7. Windows PowerShell 5.1 capacity-tool closure

This section must use **Windows PowerShell 5.1**, i.e. `powershell.exe`, not only `pwsh`.

Run:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\tools\measure_product_capacity.ps1 -MiB "4,96,256" -Indexer ".\target\release\personalrag-v2-indexer.exe"
```

Expected:

- `CAPACITY_REQUEST mib=4,96,256`
- 4 MiB hard gate PASS
- 96 MiB hard gate PASS
- 256 MiB hard gate PASS
- no parser error
- no accidental giant request

Record exact ratios.

Then verify the safety bound:

```powershell
$before = @(Get-ChildItem -LiteralPath $env:TEMP -Directory -Filter 'PersonalRag-Capacity-*' -ErrorAction SilentlyContinue).Count
powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\tools\measure_product_capacity.ps1 -MiB "496256" -Indexer ".\target\release\personalrag-v2-indexer.exe"
$oversizedExit = $LASTEXITCODE
$after = @(Get-ChildItem -LiteralPath $env:TEMP -Directory -Filter 'PersonalRag-Capacity-*' -ErrorAction SilentlyContinue).Count
```

Acceptance:

- oversized invocation returns non-zero,
- error explicitly rejects the MiB value,
- `$after -eq $before`,
- no large temporary tree is created.

## 8. Final source cleanliness

```powershell
git diff --check
git status --porcelain
```

Both must be clean/zero before report publication.

## 9. Closure decision

If all targeted checks pass:

- `S7-DOC-002 = PASS`
- capacity PowerShell 5.1 anomaly = CLOSED
- retain all unchanged product-path PASS results from report commit `2ee23681f9f1a09864c421fa9e974fb003ea84af`
- final result: **`STEP7_COMPLETE`**

If any targeted check fails, final result is **`STEP7_NOT_COMPLETE`** and record the exact command/error without modifying product source.

## 10. Report publication

Create only:

```text
reports/step7-windows-closure-<timestamp>/
  PersonalRag_STEP7_WINDOWS_CLOSURE_REPORT_<timestamp>.md
  PersonalRag_STEP7_WINDOWS_CLOSURE_COMMANDS_<timestamp>.txt
  PersonalRag_STEP7_WINDOWS_CLOSURE_LOGS_<timestamp>.txt
  PersonalRag_STEP7_WINDOWS_CLOSURE_RESULTS_<timestamp>.zip
  SHA256SUMS.txt
  README.md
```

Commit/push that directory only to `reports/step7-windows-closure-<timestamp>`.

The report must state:

- latest HEAD/tree,
- source manifest result,
- diff-guard result,
- Rust/Cargo versions,
- helper state,
- full cargo-test counts,
- `document_extraction` count,
- `where.exe zip` result,
- exact PowerShell 5.1 capacity ratios,
- oversized-request rejection evidence,
- source checkout final cleanliness,
- `S7-DOC-002` result,
- final `STEP7_COMPLETE` or `STEP7_NOT_COMPLETE`.
