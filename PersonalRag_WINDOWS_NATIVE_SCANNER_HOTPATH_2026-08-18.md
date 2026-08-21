# PersonalRag Windows Native Scanner Hot-Path Acceleration — 2026-08-18

## Baseline

`PersonalRag_GUI_PortableCore_Win32BatchScanner_2026-08-18`

## Scope

This wave keeps the Win32 batch scanner architecture and removes work still performed per entry or per directory after the first native-scanner implementation. Search Core, persistent formats, query semantics, frontend contracts, and USN journal format are unchanged.

## Adopted changes

### 1. FileIdBothDirectoryInfo and parent-record directory IDs

Directory enumeration now uses `GetFileInformationByHandleEx(FileIdBothDirectoryInfo)` rather than `FileIdExtdDirectoryInfo`.

The returned `FILE_ID_BOTH_DIR_INFO` record contains the existing 64-bit file ID needed by the USN directory map. Child directory tasks carry that ID from the parent enumeration record, so `GetFileInformationByHandle` is no longer issued once per child directory. Only the root directory needs the handle-based 64-bit ID lookup because it has no parent record.

The Windows-only correctness oracle was strengthened to compare the complete `(relative_path, file_id)` directory tracking set from the native scanner against the existing WalkDir/handle-based implementation.

### 2. Worker-local progress counters

The first native implementation updated shared atomics for discovered, selected, pruned and selected bytes on nearly every filesystem entry.

The new implementation keeps those counters worker-local and flushes them in 1024-entry groups. The existing UI progress cadence remains approximately 1024 discovered entries via a global report ticket, while final counters are flushed exactly before the final report.

This removes cross-core atomic read-modify-write traffic from the per-file hot loop.

### 3. Batched USN directory tracking output

`TrackedDirectory` rows are now accumulated in a 1024-entry worker-local batch and appended to the shared vector under one mutex acquisition. The previous native scanner took the shared directory-tracking mutex once per directory.

### 4. Path construction moved behind rejection checks

The entry hot path now classifies with attributes/size before building strings and paths.

- Oversized regular files are rejected directly from the directory record before UTF-16 filename decoding, relative-path String creation, or PathBuf joins.
- Standard directory-name exclusions are checked before relative-path and PathBuf construction.
- Custom relative-path filtering is only evaluated when configured.
- A regular file now creates only its output/scan `PathBuf`; the old native path also constructed an `open_path` `PathBuf` that was immediately discarded.

### 5. Reusable UTF-16 directory-open scratch

The UTF-16 buffer passed to `CreateFileW` is now worker-local and reused for every directory. The previous implementation allocated a new `Vec<u16>` for every directory open.

## Mechanistic microbenchmarks on the Linux validation host

These are deliberately **not** claimed as Windows end-to-end scanner speedups. They isolate only the CPU-side work removed in this wave.

- 4 threads, 8 million selected-entry counter updates:
  - per-entry shared relaxed atomics: 226903 us median
  - 1024-entry worker-local batching: 1223 us median
  - isolated counter-overhead reduction: 99.5%
- 2 million synthetic regular-file path builds:
  - two `PathBuf::join` operations per file: 224139 us median
  - one `PathBuf::join` operation per file: 146688 us median
  - isolated join-path reduction: 34.6%
- 4 threads, 2 million synthetic directory tracking pushes:
  - mutex per item: 59103 us median
  - 1024-item worker-local append: 8890 us median
  - isolated mutex-section reduction: 85.0%

Actual Windows scanning also includes kernel directory enumeration, filesystem cache behavior, filename conversion, allocation, exclusion checks, output retention and scheduling. Therefore the Windows benchmark script remains the performance hard gate.

## Correctness / validation

### Pre-change

- Search Core: 138/138 PASS.
- Search Core Clippy `-D warnings`: PASS.
- Bridge Cargo offline: blocked before source compilation by the pre-existing missing `ignore 0.4.33` cache entry.

### Post-change working tree

- Search Core: 138/138 PASS (target-split final regression).
- Search Core Clippy `-D warnings`: PASS.
- Search Core CGU16 release examples/bins: PASS.
- CGU16-built release `pr_portable self-test`: `SELF_TEST_PASS`.
- changed Bridge scanner file: Rust 2021 rustfmt PASS.
- portable directory-record parser/fallback tests: 4/4 PASS with `rustc -D warnings`.
- Windows-only production branch cfg-lift: `rustc -D warnings` PASS.
- Windows-only production branch cfg-lift: Clippy `-D warnings` PASS.
- Bridge Cargo offline: same pre-existing `ignore 0.4.33` resolution block before source compile.

### Review fixes

Two implementation review loops caught Windows-only issues before packaging:

1. a mutable-borrow boundary introduced by worker-local progress batching; fixed by passing only `LocalProgressCounters` into the reporting helper;
2. the parser had been converted to `FILE_ID_BOTH_DIR_INFO` while the FFI call still used information-class 19; fixed by using `FileIdBothDirectoryInfo` class 10 consistently.

## Windows hard gate

Run on the target Windows 11 machine:

```powershell
.\scripts\benchmark-windows-native-scanner.ps1 -Root "C:\path\to\real\root" -Rounds 7
```

The script first runs the Windows-only WalkDir/native correctness oracle (now including directory File IDs), then measures WalkDir versus the current native scanner and emits the `WINDOWS_NATIVE_SCAN` diagnostics.

No Windows runtime speedup percentage is claimed from the Linux host.

## Source changes in this wave

Production code changed only in:

- `bridge-core/src/windows_native_scanner.rs`

Documentation added/updated:

- this report;
- the prior batch-scanner report is marked as superseded for current implementation details.
