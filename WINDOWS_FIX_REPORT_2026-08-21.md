# PersonalRag Windows Build Fix Report — 2026-08-21

## Fixed from the Windows build log

- `search-core/src/vnext_generation_store.rs`
  - Gate `std::fs::File` behind `#[cfg(unix)]` so Windows does not fail `-D warnings` with an unused import.
- `search-core/tests/production.rs`
  - Release mmap-backed q2/positional readers before intentionally rewriting their sidecar files.
  - Addresses Windows error 1224 (`ERROR_USER_MAPPED_FILE`) in the four fail-closed corruption tests.
- `bridge-core/src/engine.rs`
  - Convert `verify_vnext_generation_store`'s report result to `Result<(), String>` with `.map(|_| ())` at both shadow verification call sites.
  - Remove the Clippy `unneeded_wildcard_pattern` violation.
  - Add missing `Path`, `PathBuf`, and `ShadowCompareKey` test imports.
- `src-tauri/src/main.rs`
  - Apply `rustfmt` ordering so `cargo fmt -- --check` succeeds.
- `scripts/verify-and-build-windows.ps1`
  - Check every native command's exit code explicitly. Windows PowerShell 5.1 no longer continues into later stages after a failed `cargo`/`npm` command.

## Validation performed in the current runtime

- `search-core` full regression: 167 tests PASS.
- `search-core` `cargo fmt -- --check`: PASS.
- `search-core` `cargo clippy --offline --all-targets -- -D warnings`: PASS.
- `search-core` release build: PASS.
- `pr_portable self-test`: `SELF_TEST_PASS`.
- `bridge-core` source parses/formats with Rust 1.97.1 rustfmt: PASS.
- `src-tauri` source parses/formats with Rust 1.97.1 rustfmt: PASS.
- Static review confirms every compiler/test error reported by the supplied Windows log has a corresponding fix.

## Windows release-build note

The ChatGPT execution container used for this repair is Linux and does not contain a Windows MSVC toolchain/WebView2 build environment, so it cannot physically link `personalrag-tauri.exe`. The package keeps `Build-And-Run.cmd`; on Windows it executes the corrected fail-fast verification sequence and verifies that `src-tauri\\target\\release\\personalrag-tauri.exe` exists before reporting `WINDOWS_GUI_OPTIMIZED_BUILD_PASS`.
