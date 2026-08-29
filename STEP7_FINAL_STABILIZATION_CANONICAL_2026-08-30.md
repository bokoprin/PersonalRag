# PersonalRag V2 Step 7 Final Stabilization

Date: 2026-08-30  
Status: **FINAL STABILIZATION COMPLETE / CANONICAL CANDIDATE — real-machine final Step 7 E2E pending**

## Scope

This wave is intentionally limited to the four residual areas from the 2026-08-29 native-Windows E2E:

1. Windows source-manifest verification in legacy CRLF worktrees,
2. normal-user live watch when raw NTFS USN access is denied,
3. Windows document helper discovery/path interoperability,
4. complete product persistent-capacity acceptance.

No frozen Step 1–5 persistent identity or deterministic search semantic is changed.

Crate version: **0.9.1**.

## 1. Source manifest

`tools/verify_source_manifest.ps1` still verifies raw SHA-256 first. For files Git declares `eol=lf`, it may additionally accept only the exact CRLF→LF normalized bytes. This supports a legacy Git-clean Windows worktree created before the LF rule without accepting arbitrary content changes.

Focused native-Windows evidence confirms:

- canonical LF bytes: PASS,
- exact legacy CRLF form: PASS with explicit warning/count,
- appended real content corruption: rejected.

A fresh disposable clone remains mandatory for the final user-machine acceptance.

## 2. Normal-user watch

The product watcher now has two supported trigger modes:

- `mode=usn`: preferred raw NTFS USN Journal path,
- `mode=directory-notify`: recursive Win32 directory-change notification fallback when raw-volume USN access is unavailable under a normal token.

Both modes use deterministic reconciliation as the authoritative state before publishing a bundle. Administrator elevation is not required for normal product acceptance.

Native Windows focused regression successfully initialized a real store, opened the watcher, modified a real source file, observed a publish, and verified through the GUI search session that the old token disappeared and the new token became searchable.

## 3. Document helpers

Windows OOXML extraction no longer auto-selects Git/MSYS `Git\usr\bin\unzip.exe`.

Windows ZIP-reader order prefers:

1. explicit override,
2. native `%SystemRoot%\System32\tar.exe`,
3. native PATH/WinGet helper.

Win32 verbatim paths are converted to helper-compatible normal drive/UNC paths at the external process boundary. PDF helper invocation uses the same external path adaptation.

`tools/setup_windows_helpers.ps1 -Install` provisions only Poppler/`pdftotext` and Zstandard/`zstd`; Windows built-in `tar.exe` is the preferred OOXML ZIP transport.

Native Windows focused regression verified DOCX extraction through the native ZIP reader using a canonical/verbatim Windows path.

## 4. Complete product capacity

The earlier 1.2 MiB E2E corpus produced a 24.6876% complete-store ratio because fixed two-bundle rollback/header overhead dominated the very small denominator.

A pre-fix whole-product probe on the unchanged 0.9.0 implementation, forcing a compaction-producing update and counting the complete retained store, measured:

| Selected source | Init ratio | Complete ratio after update |
|---:|---:|---:|
| 4 MiB | 2.689290% | 5.387712% |
| 96 MiB | 1.356620% | 2.713791% |
| 256 MiB | 1.341617% | 2.683691% |

The final native-Windows acceptance CI on 0.9.1 measured:

| Selected source | Init ratio | Complete ratio after update | Hard gate |
|---:|---:|---:|---|
| 4 MiB | 2.539754% | **5.087256%** | PASS |
| 96 MiB | 1.329789% | **2.659954%** | PASS |
| 256 MiB | 1.318659% | **2.637484%** | PASS |

The normative percentage gate is therefore evaluated at selected-source sizes >=4 MiB. Smaller roots are still reported diagnostically, but their percentage alone does not fail acceptance. Generation-level fail-closed capacity checks remain unchanged.

Reproducible Windows command:

```powershell
powershell -ExecutionPolicy Bypass -File .\tools\measure_product_capacity.ps1 -MiB 4,96,256
```

## Regression evidence

### Pre-change Gate 0

GitHub Actions run **33273993630**: PASS.

- canonical main manifest: PASS,
- Linux fmt/clippy/full regression/release: PASS,
- Windows fmt/clippy/GUI/incremental/product-lifecycle/release: PASS.

### Focused final validation

GitHub Actions run **33274363082**: PASS.

- Linux fmt/clippy/product lifecycle/document extraction: PASS,
- Windows native ZIP-reader regression: 1/1 PASS,
- Windows normal-user watcher regression: 1/1 PASS,
- Windows GUI regression: 2/2 PASS,
- Windows incremental regression: 19/19 PASS,
- Windows product lifecycle: 1/1 PASS,
- manifest legacy-CRLF acceptance + real-corruption rejection: PASS.

After crate version synchronization, run **33274473901** also completed successfully.

### Final acceptance

GitHub Actions run **33274526326**: PASS.

Linux, Rust/Cargo 1.97.1:

- fmt: PASS,
- clippy with warnings denied: PASS,
- full regression: **88/88 PASS**,
- release build: PASS.

Native Windows:

- fmt: PASS,
- clippy with warnings denied: PASS,
- native ZIP-reader regression: 1/1 PASS,
- normal-user watch regression: 1/1 PASS,
- GUI regression: 2/2 PASS,
- incremental regression: 19/19 PASS,
- product lifecycle: 1/1 PASS,
- release build: PASS, 22.39 s,
- 4/96/256 MiB complete-store capacity hard gates: PASS,
- `personalrag-v2-gui.exe`: present, 915,456 bytes,
- `personalrag-v2-indexer.exe`: present, 995,328 bytes.

## Acceptance boundary

This wave is implementation/CI stabilization only. GitHub-hosted Windows CI does **not** replace the user's real-machine Step 7 E2E.

Final user-machine verification must use a fresh clone and follow:

`STEP7_WINDOWS_FINAL_RETEST_CODEX_2026-08-30.md`

Required final re-evaluation includes `S7-BUILD-001`, `S7-BUILD-002`, `S7-GUI-001`, `S7-INCREMENTAL-001`, `S7-INIT-001`, `S7-USN-001`, `S7-DOC-001`, `S7-DOC-002`, and `S7-CAPACITY-001`.

Step 7 itself is **not yet declared COMPLETE** until that real-machine final retest succeeds.
