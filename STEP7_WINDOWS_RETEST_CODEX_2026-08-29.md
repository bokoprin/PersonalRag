# PersonalRag V2 Step 7 Windows E2E Retest Instructions for Codex

Date: 2026-08-29  
Scope: target-Windows verification after the Step 7 stabilization wave  
Mode: **verification only**

## 1. Objective

Re-run Step 7 against the latest canonical GitHub `main` and determine whether the runnable Windows product lifecycle now works end to end:

```text
create/obtain index
-> launch GUI
-> filename/path search
-> content search
-> PDF/Office search
-> Open / Show in Explorer
-> NTFS create/modify/rename/move/delete
-> live USN producer publishes updated bundle
-> GUI reload
-> restart/reload
-> fail-closed recovery
```

The stabilization wave adds a supported product index lifecycle binary, Windows USN producer wiring, Windows path-mapping correction, Windows-clippy fixes, LF-stable source checkout, and helper discovery/provisioning support. None of those may be assumed PASS until executed on the target Windows machine.

## 2. Canonical source preflight

Use a clean clone of:

```text
https://github.com/bokoprin/PersonalRag.git
```

If an existing clone is dirty, do not reset/stash/clean it. Use a fresh disposable clone instead.

```powershell
git fetch --prune origin
git switch main
git pull --ff-only origin main
git rev-parse HEAD
git rev-parse HEAD^{tree}
git status --porcelain
powershell -ExecutionPolicy Bypass -File .\tools\verify_source_manifest.ps1
```

Acceptance:

- checkout is clean,
- manifest verification is PASS,
- record exact HEAD and tree in the report.

The repository contains `.gitattributes` forcing canonical text to LF. `S7-BUILD-001` is PASS only if a normal Windows checkout verifies the source manifest without manual line-ending repair.

## 3. Windows and Rust baseline

Record:

```powershell
Get-ComputerInfo | Select-Object WindowsProductName,WindowsVersion,OsBuildNumber,OsArchitecture
rustc --version
cargo --version
git --version
```

Required Rust baseline: **1.97.1**.

Run exactly:

```powershell
cargo fmt --all -- --check
cargo clippy --offline --locked --all-targets -- -D warnings
cargo test --offline --locked
cargo build --offline --locked --release
```

Record the actual number of tests executed. Confirm both binaries exist:

```powershell
Test-Path .\target\release\personalrag-v2-gui.exe
Test-Path .\target\release\personalrag-v2-indexer.exe
```

`S7-BUILD-002` is PASS only if Windows clippy succeeds with warnings denied.

## 4. Helper discovery and provisioning

First inspect without installing anything:

```powershell
.\target\release\personalrag-v2-indexer.exe helpers
powershell -ExecutionPolicy Bypass -File .\tools\setup_windows_helpers.ps1
```

The product discovers helpers from explicit environment variables, executable-local `helpers\`, PATH-compatible names, common Git for Windows locations, WinGet locations, and common Scoop locations.

Required document helpers:

- `pdftotext.exe`
- `unzip.exe`
- `zstd.exe`

If any are missing, do not silently install software. Ask the user for permission to run:

```powershell
powershell -ExecutionPolicy Bypass -File .\tools\setup_windows_helpers.ps1 -Install
```

After installation or if already present, run `personalrag-v2-indexer.exe helpers` again and record resolved paths/versions.

`S7-DOC-001` remains BLOCKED rather than PASS if the required helper cannot be made available.

## 5. Create a disposable NTFS corpus

Use a local NTFS drive. Do not use the repository as the indexed root. Example:

```powershell
$Stamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$Step7 = Join-Path $env:TEMP "PersonalRag-Step7-$Stamp"
$Root = Join-Path $Step7 'corpus'
$Store = Join-Path $Step7 'store'
$Evidence = Join-Path $Step7 'evidence'
New-Item -ItemType Directory -Force $Root,$Store,$Evidence | Out-Null
New-Item -ItemType Directory -Force `
  (Join-Path $Root 'text'), `
  (Join-Path $Root 'path-case'), `
  (Join-Path $Root 'rename'), `
  (Join-Path $Root 'move'), `
  (Join-Path $Root 'modify'), `
  (Join-Path $Root 'delete'), `
  (Join-Path $Root 'docs') | Out-Null
```

Create at minimum:

```text
text\alpha.txt
text\bravo.log
path-case\MixedCaseName.TXT
rename\before-rename.txt
move\before-move.txt
modify\modify-me.txt
delete\delete-me.txt
```

Use unique markers:

```text
PR_STEP7_ALPHA_ONLY_7F41
PR_STEP7_BRAVO_ONLY_4D65
PR_STEP7_SHARED_TOKEN_8C22
ERROR_1234
ERROR_5678
PR_STEP7_日本語検索_9A73
CaseSensitiveStep7Token
PR_STEP7_RENAME_3A11
PR_STEP7_MOVE_4B22
PR_STEP7_MODIFY_OLD_5C33
PR_STEP7_DELETE_6D44
```

Create >100 additional matching text files if practical so the GUI `More` path can be tested.

Document fixtures must be real valid PDF/DOCX/XLSX/PPTX documents, not renamed text files. Put one unique marker in searchable content of each and record how each fixture was created.

## 6. Supported initial index path — retest S7-INIT-001

The supported product command is now:

```powershell
.\target\release\personalrag-v2-indexer.exe init --root "$Root" --store "$Store"
```

Optional explicit helper overrides remain supported:

```text
--pdftotext <path> --unzip <path> --zstd <path>
```

Acceptance:

- exits successfully,
- prints `INIT_OK`,
- produces a loadable bundle in `$Store`,
- reports metadata/searchable counts and store bytes,
- `status` succeeds:

```powershell
.\target\release\personalrag-v2-indexer.exe status --root "$Root" --store "$Store"
```

If this succeeds from a fresh root/store, `S7-INIT-001` is PASS. A Rust test helper is not a substitute for this executable product path.

## 7. Real Win32 GUI smoke and search matrix

Launch:

```powershell
.\target\release\personalrag-v2-gui.exe --root "$Root" --store "$Store"
```

Verify actual window behavior:

- window opens and closes cleanly,
- File/path and Content inputs,
- Full path and Case sensitive controls,
- Literal / Regex / Wildcard selector,
- result list and preview,
- Open / Show in Explorer / Reload index / More,
- typing and resize do not freeze the UI,
- Japanese text renders legibly.

Then execute through the real GUI:

| Check | Query / operation | Expected |
|---|---|---|
| filename | `alpha` | `alpha.txt` |
| full path | Windows path fragment | correct matching path |
| filename case OFF | `MIXEDCASENAME.TXT` | `MixedCaseName.TXT` |
| filename case ON | wrong casing | no result |
| literal content | `PR_STEP7_SHARED_TOKEN_8C22` | alpha + bravo |
| AND | file=`alpha`, shared content | alpha only |
| regex | `ERROR_[0-9]{4}` | alpha + bravo |
| wildcard | `*ONLY_*` | sentinel files |
| Japanese | `PR_STEP7_日本語検索_9A73` | correct result + readable preview |
| content case | wrong case with Case sensitive ON | no result |
| preview | select content result | correct marker/context |
| More | >100 matching files | enumeration expands |

`S7-GUI-001` is PASS only if Windows content search returns the expected mapped files. `S7-INCREMENTAL-001` is PASS only if the focused Windows regression and actual product lifecycle no longer lose nested-path content mappings.

## 8. PDF / DOCX / XLSX / PPTX

For each valid document fixture:

- initialize/reconcile it through the product path,
- search its unique marker in the GUI,
- verify correct file,
- verify preview/location is coherent with the current Step 5 logical-unit contract,
- record the helper path/version used.

Current GUI may show generic `Unit N`; do not require page/sheet/cell/slide-specific display labels unless the implementation actually provides them.

Re-run the document test suite after helpers are available:

```powershell
cargo test --offline --locked --test document_extraction -- --nocapture
```

Re-evaluate both `S7-DOC-001` and `S7-DOC-002` from this run; do not carry forward the old result without execution.

## 9. Live USN producer — retest S7-USN-001

The supported live producer is:

```powershell
.\target\release\personalrag-v2-indexer.exe watch --root "$Root" --store "$Store" --interval-ms 250
```

Expected startup output begins with `WATCH_READY` and includes journal ID / next USN.

Keep `watch` running and perform these filesystem operations inside `$Root`:

1. create a file with a new unique marker,
2. modify `modify-me.txt` from `PR_STEP7_MODIFY_OLD_5C33` to a new unique marker,
3. rename `before-rename.txt` -> `after-rename.txt`,
4. move `before-move.txt` to another indexed subdirectory,
5. delete `delete-me.txt`.

Wait for `WATCH_UPDATE`. In the GUI press **Reload index** after each observed publish (or relaunch against the same root/store) and verify:

- create appears,
- old modify marker disappears and new marker appears,
- old rename path disappears and new path appears,
- move reflects the new path and preserves content,
- delete removes filename/path/content hits.

Record change-to-searchable latency where measurable.

The Step 7 stabilization producer uses the USN Journal to trigger a deterministic reconciliation/publish. It is intentionally correctness-first; do not assume direct per-FRN mutation performance.

`S7-USN-001` is PASS only if the native Windows executable actually reads the live journal and publishes searchable updates. Source presence alone is not PASS.

## 10. Explicit update path

Also test the non-watch reconciliation command independently:

```powershell
.\target\release\personalrag-v2-indexer.exe update --root "$Root" --store "$Store"
```

It must print `UPDATE_OK` and produce a bundle that the GUI can reload.

## 11. Restart / reload / recovery

- stop `watch`, close the GUI,
- relaunch `status`, GUI, and `watch` using the same root/store,
- repeat representative filename/content queries,
- verify Reload index remains functional.

For fail-closed recovery, copy the disposable store first. Corrupt/remove one required newest-generation artifact in the **copy only**, then launch/status against the copy. Record whether the loader falls back to a valid older bundle or fails closed as designed. It must not invent plausible unverified results.

Never corrupt the user's real store.

## 12. Performance / usability evidence

Record defensible observations only:

- indexed file count,
- store bytes,
- release EXE sizes,
- GUI working set/private memory after load,
- approximate launch-to-usable time,
- representative filename/path first-result latency,
- representative content first-result latency,
- live change-to-searchable latency,
- UI responsiveness while typing/resizing,
- normal DPI behavior.

Compare applicable measurements with `docs/PERFORMANCE_SLO.md`. Do not fabricate p50/p95/p99 from manual timing.

## 13. Required final report

Write evidence outside the source checkout. The final report must include:

- Windows version/build/architecture,
- GitHub origin, exact `main` HEAD, exact Git tree,
- source manifest entry count/result,
- Rust/cargo/git versions,
- helper paths/versions,
- all baseline command results and actual test count,
- initial index/status results,
- GUI smoke/search matrix,
- PDF/Office matrix,
- Open/Explorer results,
- live create/modify/rename/move/delete matrix,
- restart/reload/recovery results,
- performance/usability evidence,
- explicit re-evaluation of all eight prior issue IDs,
- final PASS/FAIL/BLOCKED/SKIP totals,
- remaining risks.

For each FAIL/BLOCKED include:

```text
ID:
Severity:
Area:
Preconditions:
Steps to reproduce:
Expected:
Actual:
Evidence/log:
Reproducible:
```

At the end, `git status --porcelain` in the source checkout must be empty.

## 14. Report publication

Create a report-only branch:

```text
reports/step7-windows-e2e-<timestamp>
```

The branch may contain a directory such as:

```text
reports/step7-windows-e2e-<timestamp>/
  README.md
  PersonalRag_STEP7_WINDOWS_E2E_REPORT_<timestamp>.md
  PersonalRag_STEP7_WINDOWS_E2E_COMMANDS_<timestamp>.txt
  PersonalRag_STEP7_WINDOWS_E2E_LOGS_<timestamp>.txt
  PersonalRag_STEP7_WINDOWS_E2E_RESULTS_<timestamp>.zip
  SHA256SUMS.txt
```

Do not merge it and do not modify `main`.
