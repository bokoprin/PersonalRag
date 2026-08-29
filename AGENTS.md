# PersonalRag Codex Instructions

## Step 7 Windows E2E retest

When the user asks Codex to verify PersonalRag Step 7 on Windows, this repository must be treated as a **verification target**, not as an implementation workspace.

### Hard rules

- Run the E2E on the user's real native Windows machine. Linux, WSL-only, browser/cloud Linux, or GitHub Actions do not count as final Step 7 E2E.
- Fetch the latest GitHub `main` before testing. Do not validate an old ZIP, stale clone, or old commit.
- Do not modify source, tests, specifications, Cargo files, or `SOURCE_MANIFEST.sha256` during the verification run.
- Do not repair failures during verification. Record the failure and continue independent checks where safe.
- Never claim an unexecuted check as PASS.
- Destructive/corruption checks must use disposable Step 7 data only.
- Use exactly `PASS`, `FAIL`, `BLOCKED`, or `SKIP` for every check.
- `BLOCKED` means a prerequisite or capability was unavailable. Do not hide a missing prerequisite as SKIP.

The user authorizes **report publication only**: after verification, Codex may create and push a branch named `reports/step7-windows-e2e-<timestamp>` containing only the verification report/log bundle. Codex must not modify or push `main`, product source, tests, specs, or the source manifest.

## Required procedure

1. Update a clean clone to the latest GitHub `main` and record:
   - `git rev-parse HEAD`
   - `git rev-parse HEAD^{tree}`
   - `git status --porcelain`
2. Run `powershell -ExecutionPolicy Bypass -File .\tools\verify_source_manifest.ps1`.
3. Read `STEP7_WINDOWS_RETEST_CODEX_2026-08-29.md` completely.
4. Follow that document from preflight through the final report without substituting undocumented product commands.
5. Re-test every previously observed Step 7 issue, especially:
   - `S7-BUILD-001`
   - `S7-BUILD-002`
   - `S7-GUI-001`
   - `S7-INCREMENTAL-001`
   - `S7-INIT-001`
   - `S7-USN-001`
   - `S7-DOC-001`
   - `S7-DOC-002`
6. At the end verify `git status --porcelain` is still empty in the source checkout.
7. Publish only the report/evidence to the dedicated `reports/...` branch.

If the detailed retest document conflicts with actual current source behavior, record the discrepancy as a defect rather than inventing a workaround.
