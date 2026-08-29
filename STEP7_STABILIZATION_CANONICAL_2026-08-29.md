# PersonalRag V2 Step 7 Stabilization

Date: 2026-08-29  
Status: **STABILIZATION COMPLETE / CANONICAL / SEALED — real-machine Step 7 E2E retest pending**

## Trigger

The first native-Windows Step 7 run reported:

- source-manifest mismatches caused by Windows CRLF checkout,
- Windows-only clippy findings,
- nested-path content mapping failures caused by `/` vs `\\`,
- no supported fresh-user initial index/store command,
- no runnable live USN -> publish producer,
- missing PDF/Office helper discovery/provisioning.

## Stabilization changes

1. Windows content mapping canonicalizes separator direction only for metadata/content cross-index lookup.
2. Win32 source is clean under target-Windows clippy with warnings denied.
3. `.gitattributes` fixes canonical repository text to LF; `tools/verify_source_manifest.ps1` verifies the seal on Windows.
4. `personalrag-v2-indexer init/update/status` provides a supported product lifecycle.
5. `personalrag-v2-indexer watch` wires native NTFS USN detection to deterministic reconcile/publish.
6. helper auto-discovery and `tools/setup_windows_helpers.ps1` provide deterministic helper resolution/provisioning.
7. `tests/product_lifecycle.rs` drives init -> GUI search -> modify -> rename -> move -> delete -> create and durable checkpoint publication.

Crate version: **0.9.0**.

## Durable compatibility

No frozen persistent-format/semantic identity is changed.

## Acceptance boundary

Passing Linux/full regression and Windows CI prove build/test portability but do **not** replace the real-machine Step 7 GUI/USN/helper/performance E2E. The final Codex retest procedure is `STEP7_WINDOWS_RETEST_CODEX_2026-08-29.md`.

## Final stabilization gate

Local acceptance host, Rust/Cargo 1.97.1:

- `cargo fmt --all -- --check`: PASS
- `cargo clippy --offline --locked --all-targets -- -D warnings`: PASS
- regression: **88/88 PASS**
- `cargo build --offline --locked --release`: PASS

Native Windows CI run `33252517910` passed on the exact stabilization source:

- Rust 1.97.1 / Cargo 1.97.1
- fmt: PASS
- clippy with warnings denied: PASS
- GUI regression: 2/2 PASS
- incremental regression: 19/19 PASS
- product lifecycle: 1/1 PASS
- library regression: 24/24 PASS
- release build: PASS, 22.51 s
- `personalrag-v2-gui.exe`: present
- `personalrag-v2-indexer.exe`: present

This Windows CI gate is portability/build evidence only; it does not replace the user-machine Step 7 GUI/NTFS-USN/helper/performance retest.

## Next acceptance action

Codex must fetch the latest GitHub `main`, read `AGENTS.md` and `STEP7_WINDOWS_RETEST_CODEX_2026-08-29.md`, and execute the real native-Windows E2E without modifying product source. The resulting report may be published only to a dedicated `reports/step7-windows-e2e-<timestamp>` branch.
