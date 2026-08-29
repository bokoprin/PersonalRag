# PersonalRag Codex Instructions

## Step 7 Windows E2E verification

This file applies when Codex is asked to run **PersonalRag V2 Step 7 target-Windows E2E / performance / failure / usability verification**.

This is a **verification-only** task unless the user explicitly authorizes implementation.

### Hard rules

- Run on the user's real Windows machine. Linux, WSL-only, cloud sandbox, and browser-hosted Linux do not count as Windows E2E.
- Do not modify PersonalRag source, tests, specs, Cargo files, or `SOURCE_MANIFEST.sha256`.
- Do not fix defects during verification. Record them and continue independent checks where safe.
- Do not commit, push, create branches/PRs, reset, clean, or stash without explicit user authorization.
- Never claim an unexecuted check as PASS.
- Evidence/reports must be written outside the repository.
- Destructive/corruption tests must use disposable Step 7 data only, never the user's real files/store.

Use exactly: **PASS / FAIL / BLOCKED / SKIP**.
`BLOCKED` means a required environment, helper, product path, or input is missing. Do not hide missing prerequisites as SKIP.

## 1. Get the latest GitHub main

Repository:

```text
https://github.com/bokoprin/PersonalRag.git
```

If no clone exists:

```powershell
git clone https://github.com/bokoprin/PersonalRag.git
cd PersonalRag
git switch main
```

If a clone exists, first run:

```powershell
git remote get-url origin
git status --porcelain
```

If the worktree is dirty, STOP repository update and report BLOCKED. Do not discard user changes.

If clean:

```powershell
git fetch --prune origin
git switch main
git pull --ff-only origin main
git rev-parse HEAD
git rev-parse HEAD^{tree}
git status --porcelain
```

Record HEAD/tree in the final report. The current GitHub `main` is the source of truth; do not validate a ZIP, old handoff, or stale commit.

## 2. Verify source integrity

Verify every entry in `SOURCE_MANIFEST.sha256` with PowerShell `Get-FileHash -Algorithm SHA256`.

A minimal verifier:

```powershell
$fail = @()
Get-Content .\SOURCE_MANIFEST.sha256 | ForEach-Object {
    if ($_ -notmatch '^([0-9a-fA-F]{64})\s+\*?(.+)$') {
        $fail += "Malformed: $_"; return
    }
    $expected = $matches[1].ToLowerInvariant()
    $path = $matches[2].Trim()
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        $fail += "Missing: $path"; return
    }
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToLowerInvariant()
    if ($actual -ne $expected) { $fail += "Mismatch: $path" }
}
if ($fail.Count) { $fail | % { Write-Error $_ }; throw "manifest FAIL" }
"Manifest entries: $((Get-Content .\SOURCE_MANIFEST.sha256).Count) / PASS"
```

If this fails, source-integrity is FAIL and later results must not be presented as canonical acceptance.

## 3. Read before testing

Read:

```text
README.md
DEVELOPMENT_RULES.md
STEP6_GUI_CANONICAL_2026-08-29.md
docs/V2_GUI.md
docs/V2_INCREMENTAL_INDEX.md
docs/V2_DOCUMENT_EXTRACTION.md
docs/PERFORMANCE_SLO.md
```

Do not assume Tauri/Electron/web frontend. Step 6 GUI is Rust + Win32.

Launch contract:

```text
personalrag-v2-gui --root <indexed-root> --store <index-store> [--pdftotext <path>] [--unzip <path>] [--zstd <path>]
```

## 4. Windows/toolchain baseline

Record:

```powershell
Get-ComputerInfo | Select WindowsProductName,WindowsVersion,OsBuildNumber,OsArchitecture
rustc --version
cargo --version
git --version
where.exe pdftotext
where.exe unzip
where.exe zstd
```

Required sealed baseline: Rust **1.97.1**.

If `rustup` already exists, installing/selecting 1.97.1 is allowed. If Rust/rustup is absent and an installer/admin/policy change is required, ask the user before changing the machine and mark build BLOCKED until approved.

Run:

```powershell
cargo fmt --all -- --check
cargo clippy --offline --locked --all-targets -- -D warnings
cargo test --offline --locked
cargo build --offline --locked --release
```

Record exact outputs and actual test count. Do not silently replace a failed sealed command and call it PASS.

Confirm:

```powershell
Test-Path .\target\release\personalrag-v2-gui.exe
```

## 5. Critical precondition: initial index/store

Before GUI testing, determine how the current runnable product creates a valid `<index-store>` for an arbitrary `<indexed-root>`.

Search current source/docs:

```powershell
git grep -n -i "index.*build\|build.*index\|publish_generation\|write_bundle\|initial.*index\|index-store\|--store"
```

Benchmark binaries and Rust test helpers are not automatically an end-user indexing path.

Do not modify tests, write a new helper, invent an undocumented CLI, or fabricate bundle files.

To proceed with full GUI E2E, either:
1. a documented/supported product indexing path exists; or
2. the user explicitly provides a valid compatible store.

Otherwise record:

```text
BLOCKER S7-INIT-001
No supported initial index/store creation path is available for a fresh Windows user.
```

This is a product blocker, not merely a test inconvenience. Continue independent build/static/failure checks, but do not claim GUI search E2E PASS.

## 6. Disposable E2E corpus

Create data outside the repo, e.g. under `%TEMP%\PersonalRag-Step7-<timestamp>`.

At minimum create:

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
```

PDF/DOCX/XLSX/PPTX fixtures must be valid real documents. Never rename plain text to a document extension and count that as a format test.

Build/obtain the index only through the supported path established above. Record root/store paths, commands, elapsed time, and store bytes.

## 7. Real Win32 GUI smoke

Launch the release EXE with the real root/store and helper paths when needed.

Verify individually:

- window opens and closes cleanly,
- File/path and Content fields,
- Full path and Case sensitive controls,
- Literal/Regex/Wildcard selector,
- result list and preview,
- Open / Show in Explorer / Reload index / More,
- typing and resize remain responsive.

Each item gets PASS/FAIL/BLOCKED/SKIP.

## 8. Search E2E matrix

Execute through the real GUI:

- filename `alpha` -> `alpha.txt`
- Full path with Windows path fragment -> correct file
- case-insensitive filename `MIXEDCASENAME.TXT` -> `MixedCaseName.TXT`
- case-sensitive wrong casing -> no result
- literal content `PR_STEP7_SHARED_TOKEN_8C22` -> alpha + bravo
- file `alpha` AND shared content -> alpha only
- Regex `ERROR_[0-9]{4}` -> alpha + bravo
- Wildcard `*ONLY_*` -> corresponding sentinel files
- Japanese `PR_STEP7_日本語検索_9A73` -> correct result + readable preview
- case-sensitive content -> only exact casing
- Preview -> correct marker, no UTF-8 corruption
- More -> when >100 matching files are practical, expands enumeration without changing semantics/freezing UI

Record actual counts and representative returned paths.

## 9. Windows shell integration

On disposable results only:

- **Open** must open the correct file with the Windows default application.
- **Show in Explorer** must open Explorer and select the correct file.

Wrong/no action/crash is FAIL.

## 10. PDF / Office

For real PDF/DOCX/XLSX/PPTX fixtures, search a unique marker and verify correct file plus coherent logical-unit location/preview.

Record helper path/version. Missing helper/sample -> BLOCKED, never PASS.

Do not require page/sheet/cell/slide labels if current GUI only exposes generic `Unit N`.

## 11. Live incremental / NTFS / USN

Identify the actual runnable product component that consumes live NTFS/USN changes and publishes updated Step 4 bundles.

Do not assume `Reload index` scans the filesystem; Step 6 says it reloads a published bundle.

If source contains a USN adapter but no runnable process/command/service wires it into live bundle publication, record:

```text
BLOCKER S7-USN-001
Live Windows incremental index producer is not wired into a runnable product path.
```

If a supported path exists, test on NTFS:

1. create,
2. modify,
3. rename,
4. move,
5. delete.

Verify new state becomes searchable, stale metadata/content disappears, modify replaces old content, and delete removes results. Record change-to-searchable latency where measurable.

## 12. Restart / reload / failure recovery

With disposable valid data:

- close/relaunch against same root/store,
- repeat filename/content searches,
- use Reload index,
- confirm correctness/responsiveness.

For corruption testing, copy the disposable store, corrupt/remove one required artifact in the copy only, and launch against the copy.

Expected: fail closed with visible/reported error; no plausible stale/unverified search results.

## 13. Windows performance/usability evidence

Record objectively where possible:

- indexed file count,
- store bytes,
- release EXE size,
- process working set/private memory,
- launch-to-usable time,
- representative filename/content first-result latency,
- live change-to-searchable latency if available,
- typing/resizing responsiveness,
- keyboard/Tab reachability,
- Japanese rendering,
- user's normal DPI usability.

Do not fabricate p50/p95/p99 from manual observation.

## 14. Report

Write outside the repo:

```text
PersonalRag_STEP7_WINDOWS_E2E_REPORT_<timestamp>.md
PersonalRag_STEP7_WINDOWS_E2E_COMMANDS_<timestamp>.txt
PersonalRag_STEP7_WINDOWS_E2E_LOGS_<timestamp>.txt
```

Include Windows/build info, Git HEAD/tree, manifest result, tool versions, every matrix result, helper/indexing path, blockers/defects, performance evidence, and final PASS/FAIL/BLOCKED/SKIP counts.

For every FAIL/BLOCKER include:

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

Use IDs such as `S7-BUILD-001`, `S7-GUI-001`, `S7-SEARCH-001`, `S7-DOC-001`, `S7-USN-001`, `S7-RECOVERY-001`, `S7-PERF-001`.

At the end:

```powershell
git status --porcelain
```

It must still be empty. Do not commit/discard anything without user approval.

## Step 7 completion rule

Do not declare Step 7 complete merely because it builds or the GUI opens.

Product acceptance requires evidence for the runnable lifecycle:

```text
create/obtain valid index
-> launch GUI
-> filename/path search
-> content search
-> PDF/Office where required
-> open/reveal
-> filesystem create/modify/rename/move/delete
-> incremental state becomes correct
-> restart/reload
-> search remains correct
-> fail-closed recovery
```

If the initial indexing path or live incremental producer is missing, report it as a **product blocker**. Do not work around it and then call the search system complete.
