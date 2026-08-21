# PersonalRag Win32 Batch2 Fast Path Report

Date: 2026-08-18
Baseline: `PersonalRag_GUI_PortableCore_BuildOrderQueueFastPath_2026-08-18.zip`
Baseline SHA-256: `a565f53cefa85535f10dd296e373489e1bfb0d2251f831f292874e81ba188766`

## Goal

Further accelerate the Windows native full scanner without changing Search Core, index format, scanner semantics, exclusion behavior, or the WalkDir fallback.

## Final adopted changes

### 1. 1 MiB Win32 directory enumeration buffer

The native scanner previously allocated 256 KiB per worker for `GetFileInformationByHandleEx(FileIdBothDirectoryInfo)`.
The default is now 1024 KiB per worker, so an 8-worker scan uses at most about 8 MiB for these directory buffers.

Runtime override:

```text
PR_NATIVE_DIR_BUFFER_KIB=<64..4096>
```

Default: 1024 KiB. Values are clamped to 64..4096 KiB. The Windows benchmark script now runs the default 1024 KiB path and a 256 KiB reference path.

This change is intended to reduce Win32 enumeration calls for directories that need multiple 256 KiB batches. The actual end-to-end gain remains a Windows-runtime hard gate.

### 2. Combine child enqueue and parent completion into one queue lock

Old common path:

```text
push_many(children)  -> queue Mutex
complete_many(parent)-> queue Mutex
```

New common path:

```text
complete_many_with_children(parent, children) -> one queue Mutex
```

To avoid starving workers on very wide roots, children are still flushed early when the local child batch reaches the scanner worker count. Small/normal directories keep their children worker-local until completion, removing one queue lock.

Pending accounting remains:

```text
new_pending = old_pending + discovered_children - completed_parents
```

A new unit test fixes this invariant including underflow rejection.

### 3. Worker-local profiling counters

`batch_calls` and `opened_directories` were `AtomicUsize::fetch_add` operations on every directory/batch even though these counters are diagnostic only.

Each worker now counts them locally and performs at most one atomic accumulation per counter when that worker exits.

This does not change scan progress semantics; user-visible progress counters remain on their existing batched path.

### 4. Fast fixed-field FILE_ID_BOTH_DIR_INFO parser

The parser previously decoded each fixed field with repeated checked slice creation and `try_into` conversion.

The parser still first validates that the full 104-byte fixed header is in bounds. Only after that validation, fixed fields inside that header use little-endian `read_unaligned`. Variable-length filename and `NextEntryOffset` validation remain fail-closed and unchanged.

Fields covered by the fast read:

- `NextEntryOffset`
- `LastWriteTime`
- `EndOfFile`
- `FileAttributes`
- `FileNameLength`
- `FileId`

## CPU-side A/B measurements

These are mechanistic Linux synthetic measurements. They isolate CPU/synchronization work and are **not** claims about total Windows scan speed.

### Queue synchronization

5 threads, synthetic enqueue+completion workload:

```text
old separate locks : 84.132 ms
combined lock      : 37.693 ms
speedup            : 2.23x
```

### Diagnostic counter synchronization

5 threads, synthetic per-event atomic updates versus worker-local accumulation:

```text
per-event atomic   : 4.653 ms
worker-local batch : 0.215 ms
speedup            : 21.63x
```

### Directory record parser

Same synthetic record bytes, 500 parse loops:

```text
256 KiB buffer: 2.288 -> 2.034 ms, 1.125x
1 MiB buffer  : 10.434 -> 9.446 ms, 1.105x
```

The parser optimization therefore reduced this isolated hot loop by about 9.5-11.1% in the final rerun.

## Experiments rejected in this wave

### Worker-local file lists + worker-local sort

This removed the scanner file-list Mutex but moved sorting into per-worker runs and required a final merge.
A fair synthetic end-to-end finalize benchmark showed regression:

```text
100k files: old 42.667 ms, candidate 45.805 ms, 0.931x
300k files: old 171.392 ms, candidate 217.286 ms, 0.789x
```

Rejected and fully reverted.

### Directory-tree ordering instead of global path sort

A prototype rebuilt output order through directory listings and DFS. Hash-map/tree reconstruction cost exceeded global sorting in the tested synthetic workloads. Rejected.

### Known-size manual hydration

Scanner-known file size was tested as a replacement for `fs::read` allocation behavior. The isolated tmpfs result was effectively neutral, so Search Core hydration was not changed.

### NTFS MFT enumeration

The existing frontend contract contains `ntfs_mft_benchmark`, but MFT/USN enumeration does not directly provide the current file size required by `ScannedFile`. A naive MFT implementation would therefore add per-file metadata work and lose an important advantage of the current directory batch API. It was not promoted into the normal scanner in this wave.

## Correctness / regression

Baseline before changes:

- Search Core: 138/138 PASS
- fmt: PASS
- clippy `-D warnings`: PASS
- CGU16 release build: PASS
- `SELF_TEST_PASS`

Final working tree:

- Search Core: 138/138 PASS
- Search Core source: unchanged from baseline
- fmt: PASS
- clippy `-D warnings`: PASS
- CGU16 release examples/bins: PASS
- release `SELF_TEST_PASS`
- native parser/fallback/accounting tests: 7/7 PASS
- baseline build-order tests: 2/2 PASS
- Windows cfg-lift rustc `-D warnings`: PASS
- Windows cfg-lift clippy `-D warnings`: PASS

Bridge Cargo full offline check remains blocked before source compilation by the pre-existing missing `ignore` crate in this Linux session. This is unchanged from the baseline environment.

## Final code scope

Code changes are intentionally restricted to:

```text
bridge-core/src/windows_native_scanner.rs
scripts/benchmark-windows-native-scanner.ps1
```

`search-core/` and `bridge-core/src/build_order.rs` are byte-identical to the baseline source.

## Windows hard gate

Run on Windows 11:

```powershell
.\scripts\benchmark-windows-native-scanner.ps1 -Root "C:\path\to\large\tree" -Rounds 7
```

The updated script profiles the default 1024 KiB buffer and also runs a 256 KiB reference. `PR_PROFILE_SCANNER=1` now reports `buffer_kib` together with directory handles and batch-call counts.

## Phase

Win32 native scanner Batch2 fast-path wave: implementation complete.
Windows native runtime A/B: pending external hard gate.
