# PersonalRag Codex Instructions

## Step 7 final Windows E2E retest

When the user asks Codex to verify PersonalRag Step 7 on Windows, this repository is a **verification target**, not an implementation workspace.

### Hard rules

- Run the final E2E on the user's real native Windows machine. Linux, WSL-only, browser/cloud Linux, and GitHub Actions do not count as final Step 7 acceptance.
- Use a **fresh disposable clone** of the latest GitHub `main`.
- Do not modify product source, tests, specifications, Cargo files, or `SOURCE_MANIFEST.sha256`.
- Do not repair failures during verification. Record them and continue independent checks where safe.
- Never claim an unexecuted check as PASS.
- Destructive/corruption checks must use disposable Step 7 data only.
- Use exactly `PASS`, `FAIL`, `BLOCKED`, or `SKIP`.
- Required helper provisioning may be performed only when the user explicitly authorizes the repository's `tools/setup_windows_helpers.ps1 -Install`; do not install unrelated software.

The user authorizes **report publication only** after verification: Codex may create and push a branch named `reports/step7-windows-e2e-<timestamp>` containing only the report/log/evidence bundle. Never push or merge `main`.

## Required procedure

1. Fresh-clone the latest GitHub `main`; record HEAD/tree and clean status.
2. Run `powershell -ExecutionPolicy Bypass -File .\tools\verify_source_manifest.ps1`.
3. Read `STEP7_WINDOWS_FINAL_RETEST_CODEX_2026-08-30.md` completely.
4. Follow it without substituting undocumented product commands.
5. Re-evaluate:
   - `S7-BUILD-001`
   - `S7-BUILD-002`
   - `S7-GUI-001`
   - `S7-INCREMENTAL-001`
   - `S7-INIT-001`
   - `S7-USN-001`
   - `S7-DOC-001`
   - `S7-DOC-002`
   - `S7-CAPACITY-001`
6. For `watch`, normal-user `mode=usn` and `mode=directory-notify` are both supported; acceptance requires live searchable updates, not administrator elevation.
7. Provision required document helpers before the final full `cargo test` when authorized.
8. Run the 4/96/256 MiB complete-store capacity measurement exactly as documented.
9. At the end, `git status --porcelain` must still be empty in the source checkout.
10. Publish only report/evidence to the dedicated `reports/...` branch.

If the final retest document conflicts with actual current source behavior, record the discrepancy as a defect rather than inventing a workaround.
