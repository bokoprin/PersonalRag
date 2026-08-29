# PersonalRag V2 Step 7 Final Windows E2E Retest

Date: 2026-08-30  
Purpose: final native-Windows acceptance after the Step 7 final stabilization wave.

## 1. Hard rules

- Run on the user's real native Windows machine. WSL-only, Linux, browser/cloud Linux, and GitHub Actions do not count as the final E2E.
- Use a **fresh disposable clone** of the latest GitHub `main`. Do not reuse the previous Step 7 worktree for the canonical preflight.
- Do not modify product source, tests, specifications, Cargo files, or `SOURCE_MANIFEST.sha256`.
- Do not repair defects during verification. Record them and continue independent checks where safe.
- Destructive/corruption tests must use disposable Step 7 data only.
- Every check is exactly `PASS`, `FAIL`, `BLOCKED`, or `SKIP`. Never mark an unexecuted check PASS.
- Report/evidence may be committed only to `reports/step7-windows-e2e-<timestamp>`. Never push or merge `main`.

## 2. Fresh-clone and source-integrity preflight

Clone into a new directory:

```powershell
git clone https://github.com/bokoprin/PersonalRag.git PersonalRag-Step7-Final
cd PersonalRag-Step7-Final
git switch main
git pull --ff-only origin main
git rev-parse HEAD
git rev-parse HEAD^{tree}
git status --porcelain
powershell -ExecutionPolicy Bypass -File .\tools\verify_source_manifest.ps1
```

Acceptance:

- checkout is clean,
- manifest verifier reports PASS,
- record exact HEAD/tree.

`tools/verify_source_manifest.ps1` may accept only an exact CRLF->LF normalization for a legacy LF-canonical text file. In this required fresh clone, `normalized_legacy_crlf` should normally be 0. Any other content mismatch is FAIL.

Re-evaluate `S7-BUILD-001`.

## 3. Toolchain/build gate

Required Rust: **1.97.1**.

Record:

```powershell
Get-ComputerInfo | Select-Object WindowsProductName,WindowsVersion,OsBuildNumber,OsArchitecture
rustc --version
cargo --version
git --version
cargo fmt --all -- --check
cargo clippy --offline --locked --all-targets -- -D warnings
cargo build --offline --locked --release
```

Confirm:

```powershell
Test-Path .\target\release\personalrag-v2-gui.exe
Test-Path .\target\release\personalrag-v2-indexer.exe
```

Re-evaluate `S7-BUILD-002`.

Do **not** run the final full `cargo test` until document helpers have been provisioned in section 4.

## 4. Document helper provisioning

Inspect:

```powershell
.\target\release\personalrag-v2-indexer.exe helpers
powershell -ExecutionPolicy Bypass -File .\tools\setup_windows_helpers.ps1
```

Expected Windows ZIP reader is normally native:

```text
C:\Windows\System32\tar.exe
```

Git/MSYS `Git\usr\bin\unzip.exe` must not be auto-selected.

Required capabilities:

- Poppler `pdftotext.exe`
- native ZIP reader (preferred Windows `tar.exe`)
- `zstd.exe`

If Poppler or zstd is missing and the user has authorized helper provisioning for this run, execute only:

```powershell
powershell -ExecutionPolicy Bypass -File .\tools\setup_windows_helpers.ps1 -Install
```

Then re-run both helper checks. Record resolved paths and first-line versions.

Now run:

```powershell
cargo test --offline --locked
cargo test --offline --locked --test document_extraction -- --nocapture
cargo test --offline --locked --test windows_document_helper -- --nocapture
```

The full test suite must not be called PASS unless all executed tests pass.

Re-evaluate `S7-DOC-001` and `S7-DOC-002`.

## 5. Disposable real corpus

Use a local NTFS directory outside the repository. Include:

- text files,
- nested paths,
- mixed-case filename,
- Japanese/Unicode text,
- >100 wildcard-matching files,
- rename/move/modify/delete targets,
- valid real PDF/DOCX/XLSX/PPTX fixtures with unique markers.

Use unique markers such as:

```text
PR_STEP7_FINAL_ALPHA_7F41
PR_STEP7_FINAL_SHARED_8C22
PR_STEP7_FINAL_日本語_9A73
PR_STEP7_FINAL_MODIFY_OLD_5C33
PR_STEP7_FINAL_MODIFY_NEW_5D44
```

Do not create fake document fixtures by renaming plain text.

## 6. Initial product index

Use the release product binary:

```powershell
.\target\release\personalrag-v2-indexer.exe init --root "$Root" --store "$Store"
.\target\release\personalrag-v2-indexer.exe status --root "$Root" --store "$Store"
```

Acceptance:

- `INIT_OK`,
- `STATUS_OK`,
- GUI can load the produced store.

Re-evaluate `S7-INIT-001`.

## 7. Real Win32 GUI

Launch:

```powershell
.\target\release\personalrag-v2-gui.exe --root "$Root" --store "$Store"
```

Verify through the actual native GUI:

- filename search,
- relative/full-path mode,
- case-insensitive and case-sensitive behavior,
- literal content,
- filename/path + content AND,
- regex,
- wildcard,
- Unicode/Japanese,
- Preview,
- More (>100 results),
- Open,
- Show in Explorer,
- Reload index,
- close/relaunch,
- resize/typing responsiveness.

Record representative first-result latency; do not invent p50/p95/p99 from manual timing.

Re-evaluate `S7-GUI-001`.

## 8. PDF / Office through product and GUI

With the real document fixtures and resolved helpers:

1. build a disposable document root/store with product `init`,
2. search each unique PDF/DOCX/XLSX/PPTX marker in the GUI,
3. verify correct file, coherent preview, and logical-unit location,
4. verify Japanese/Unicode if included.

Generic `Unit N` remains acceptable if that is what the current GUI exposes.

Re-evaluate `S7-DOC-001` / `S7-DOC-002`.

## 9. Normal-user live watch

Run `watch` from a **normal non-elevated PowerShell**:

```powershell
.\target\release\personalrag-v2-indexer.exe watch --root "$Root" --store "$Store" --interval-ms 250
```

Expected startup:

```text
WATCH_READY mode=<usn|directory-notify> ...
```

Both modes are valid:

- `mode=usn`: raw NTFS USN access was available.
- `mode=directory-notify`: normal-user raw-volume access was unavailable and the Win32 recursive notification fallback was selected.

If fallback is selected, record `fallback_reason`. Administrator elevation is **not** required for product acceptance.

While watch remains running, perform separately:

1. create,
2. content modify,
3. rename,
4. move,
5. delete.

For each operation:

- wait for `WATCH_UPDATE`,
- press GUI **Reload index**,
- verify new state appears and stale path/content disappears,
- record change-to-searchable latency where defensible.

Do not use explicit `update` to make this live-watch subsection pass.

`S7-USN-001` is PASS when the normal-user product watcher successfully publishes live changes in either supported mode. Also record the selected mode.

## 10. Explicit update regression

After the live-watch matrix, independently verify:

```powershell
.\target\release\personalrag-v2-indexer.exe update --root "$Root" --store "$Store"
```

and GUI Reload.

Re-evaluate `S7-INCREMENTAL-001`.

## 11. Restart and fail-closed recovery

- close GUI/watch,
- relaunch status, GUI and watch against the same root/store,
- repeat representative filename/content searches,
- confirm Reload works.

For corruption recovery, copy the disposable store. Corrupt/remove one newest-generation required artifact in the **copy only**. Verify the product either falls back to a valid older bundle or fails closed; it must never return plausible unverified newest-generation results.

## 12. Complete product capacity

Build release first, then run:

```powershell
powershell -ExecutionPolicy Bypass -File .\tools\measure_product_capacity.ps1 -MiB 4,96,256
```

The script forces a compaction-producing update and counts the complete retained store including rollback generations.

Normative gate:

- selected source >= 4 MiB,
- complete persistent store / selected source <= 10%.

Record exact 4/96/256 MiB ratios. A <4 MiB ratio may be reported diagnostically but must not by itself be classified as the hard product-capacity failure.

Re-evaluate `S7-CAPACITY-001`.

## 13. Final source cleanliness

At the end of testing, before publishing reports:

```powershell
git status --porcelain
```

It must be empty in the source checkout.

## 14. Report publication

Create only:

```text
reports/step7-windows-e2e-<timestamp>/
  PersonalRag_STEP7_WINDOWS_E2E_REPORT_<timestamp>.md
  PersonalRag_STEP7_WINDOWS_E2E_COMMANDS_<timestamp>.txt
  PersonalRag_STEP7_WINDOWS_E2E_LOGS_<timestamp>.txt
  PersonalRag_STEP7_WINDOWS_E2E_RESULTS_<timestamp>.zip
  SHA256SUMS.txt
  README.md
```

Commit/push this directory only to `reports/step7-windows-e2e-<timestamp>`.

The report must include:

- Windows/tool versions,
- HEAD/tree,
- source manifest result,
- helper paths/versions,
- full test counts,
- each issue ID result,
- watch mode/fallback reason,
- GUI/document/lifecycle results,
- 4/96/256 MiB capacity ratios,
- latency/memory observations,
- PASS/FAIL/BLOCKED/SKIP totals,
- final `STEP7_COMPLETE` or `STEP7_NOT_COMPLETE`.

Required issue IDs to re-evaluate:

- `S7-BUILD-001`
- `S7-BUILD-002`
- `S7-GUI-001`
- `S7-INCREMENTAL-001`
- `S7-INIT-001`
- `S7-USN-001`
- `S7-DOC-001`
- `S7-DOC-002`
- `S7-CAPACITY-001`

Step 7 is complete only when all required product paths are executed on the real Windows machine with no unresolved FAIL/BLOCKED that invalidates normal product use.
