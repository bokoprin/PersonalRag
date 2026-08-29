# PersonalRag Codex Instructions

## Step 7 targeted Windows closure

When the user asks Codex to finish PersonalRag Step 7 verification, this repository is a **verification target**, not an implementation workspace.

### Hard rules

- Run on the user's real native Windows machine.
- Use a **fresh disposable clone** of latest GitHub `main`.
- Do not modify product source, tests, specifications, Cargo files, or `SOURCE_MANIFEST.sha256`.
- Do not repair failures during verification.
- Never claim an unexecuted check as PASS.
- Do not install a separate `zip.exe`; the final test verifies that it is no longer required.
- Required Poppler/Zstandard provisioning may use only the repository-provided `tools/setup_windows_helpers.ps1 -Install` when needed.
- Publish reports only to `reports/step7-windows-closure-<timestamp>`; never push or merge `main`.

## Required procedure

1. Fresh-clone latest `main`; record HEAD/tree and clean status.
2. Verify `SOURCE_MANIFEST.sha256`.
3. Read `STEP7_WINDOWS_TARGETED_CLOSURE_CODEX_2026-08-30.md` completely.
4. Run its product-source diff guard against `a33a32a81a344cdbdeee14431fe71a159afe2471`.
5. If unexpected product `src/` or Cargo-format/dependency changes exist, stop with `TARGETED_RETEST_INVALID`.
6. Otherwise execute the targeted Windows full test/document/capacity closure exactly as documented.
7. Windows PowerShell **5.1** (`powershell.exe`) must execute the 4/96/256 MiB capacity measurement.
8. The oversized `496256` request must be rejected before creating test data.
9. Final source checkout must remain clean.
10. If all targeted checks pass, report `S7-DOC-002 = PASS` and final `STEP7_COMPLETE`.

The previous full real-machine E2E evidence is report commit `2ee23681f9f1a09864c421fa9e974fb003ea84af`. Do not rerun the entire manual GUI/watch matrix unless the diff guard proves product implementation changed.
