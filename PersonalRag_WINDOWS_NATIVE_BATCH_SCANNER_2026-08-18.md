# PersonalRag Windows Native Batch Scanner — 2026-08-18

> **Superseded implementation detail:** The current package includes `PersonalRag_WINDOWS_NATIVE_SCANNER_HOTPATH_2026-08-18.md`. This file documents the first Win32 batch-scanner wave; the hot-path report is authoritative for the current scanner implementation.

## Baseline

`PersonalRag_GUI_PortableCore_Q2AdaptiveShard_2026-08-18`

## Goal

Replace the previous `ScannerMode::WindowsNative` implementation, whose traversal still used `ignore::WalkBuilder` and per-entry metadata work, with a real Win32 batched directory enumeration path while preserving the existing WalkBuilder path as the correctness fallback.

## Implemented design

### Win32 batch enumeration

On Windows, `Auto` / `WindowsNative` now attempts a dedicated scanner backed by:

- `CreateFileW` directory handles with `FILE_FLAG_BACKUP_SEMANTICS`;
- `GetFileInformationByHandleEx(FileIdExtdDirectoryInfo)`;
- a 256 KiB reusable enumeration buffer per scanner worker;
- up to 8 bounded scanner workers, following the existing scanner CPU cap.

One returned directory record provides the filename, file size, last-write FILETIME, attributes, and file ID fields without calling `fs::metadata()` for every regular file.

### USN directory tracking integration

The existing USN tracker uses 64-bit directory File IDs. The native scanner obtains that ID with `GetFileInformationByHandle` from the same directory handle already opened for enumeration, avoiding the previous extra open-per-directory tracking lookup.

If directory identity cannot be obtained, scanning still completes but `directory_tracking` is marked incomplete so the USN fast path fails closed to the existing full-scan behavior.

### Bounded work queue

Directory traversal uses a shared pending-work queue with a condition variable. Each worker owns and reuses:

- one 256 KiB directory-information buffer;
- one UTF-16 filename scratch buffer;
- one child-directory scratch vector;
- one 4096-entry file result batch.

Child directories found in one Win32 batch are enqueued together, reducing queue-lock traffic.

### Single-pass userspace parsing

The `FILE_ID_EXTD_DIR_INFO` buffer is visited in-place through `NextEntryOffset`. Runtime no longer materializes an intermediate vector of parsed records and no longer scans the batch twice. UTF-16 scratch storage is reused per worker.

### Semantics / fallback

The native path preserves the existing standard directory exclusions and custom relative-path exclusions.

Cases requiring `ignore::WalkBuilder` semantics continue to use the old scanner:

- `.gitignore` support enabled;
- custom glob overrides configured;
- explicit `WalkDir` mode;
- native root open / first batch unsupported before any native result is observed.

Directory reparse points are not traversed, matching the existing `follow_links(false)` policy. Regular-file reparse points remain file candidates rather than being dropped wholesale.

If native enumeration fails after native results have already been observed, the scanner returns an error instead of silently restarting with WalkBuilder and producing duplicate/backwards progress. `PR_NATIVE_SCANNER_REQUIRE=1` disables unsupported-root fallback for the Windows acceptance benchmark.

## Diagnostics / Windows acceptance

`PR_PROFILE_SCANNER=1` prints:

`WINDOWS_NATIVE_SCAN elapsed_ms=... workers=... directory_handles=... batch_calls=... discovered=... files=... bytes=... errors=...`

A Windows benchmark example was added:

`bridge-core/examples/windows_native_scanner_bench.rs`

and a runner:

`scripts/benchmark-windows-native-scanner.ps1 -Root <path> -Rounds 7`

The runner first executes the Windows-only WalkDir-vs-native correctness oracle, then performs equivalent result validation and timing.

## Validation completed in the ChatGPT Linux environment

### Pre-change

- Search Core regression: 138 tests PASS.
- Search Core Clippy `-D warnings`: PASS.
- Bridge offline Cargo build: blocked before source compilation by the pre-existing missing `ignore 0.4.33` cache entry.

### Post-change

- Search Core regression: 138/138 PASS (Search Core source is byte-identical to baseline).
- Search Core Clippy `-D warnings`: PASS.
- Search Core CGU16 release examples/bins: PASS.
- release `pr_portable self-test`: `SELF_TEST_PASS`.
- changed Bridge Rust files: Rust 2021 `rustfmt --check` PASS.
- portable native-record/parser/fallback tests: 4/4 PASS with `rustc -D warnings`.
- Windows-only scanner branch cfg-lift type check: PASS with `rustc -D warnings`.
- Windows-only scanner branch cfg-lift Clippy: PASS with `-D warnings`.
- Bridge Cargo offline gate remains blocked before source compilation by missing `ignore 0.4.33`, identical to the pre-change environment limitation.

## Windows runtime hard gate

A real Windows filesystem / kernel32 runtime benchmark cannot be executed inside the Linux container. Therefore no Windows speedup number is claimed in this report.

The package includes a Windows-native correctness test and benchmark script so the hard gate can be run on the target Windows 11 laptop. Adoption of performance claims should be based on that native run.

## Source changes

- `bridge-core/src/lib.rs` — native dispatch before WalkBuilder fallback.
- `bridge-core/src/windows_native_scanner.rs` — new batch scanner and tests.
- `bridge-core/examples/windows_native_scanner_bench.rs` — Windows A/B benchmark.
- `scripts/benchmark-windows-native-scanner.ps1` — native acceptance runner.
- this report.

No Search Core persistent format, q2/q3 logic, query semantics, generation semantics, frontend contract, or USN journal format was changed.
