# PersonalRag V2 Step 6 Windows GUI

Status: **FROZEN — Step 6**  
Date: 2026-08-29

## 1. Scope

Step 6 provides the Everything-style desktop search boundary over the frozen Step 1–5 backend. It is deliberately deterministic and local. It does not introduce semantic search, embeddings, LLM ranking, a new persistent index format, or a compatibility copy of the removed legacy implementation.

The Windows executable is `personalrag-v2-gui`.

## 2. Launch contract

```text
personalrag-v2-gui --root <indexed-root> --store <index-store> \
  [--pdftotext <path>] [--unzip <path>] [--zstd <path>]
```

`PERSONALRAG_ROOT` and `PERSONALRAG_STORE` may replace the required root/store arguments.

The GUI opens only after `GuiSearchSession::load` successfully loads the current `PRV2BND1` bundle with the Step 5 verification checks. Loading and reload are fail-closed; the GUI does not invent an unverified in-memory substitute.

## 2.1 Fresh-user prerequisite

Step 7 stabilization adds the supported lifecycle binary `personalrag-v2-indexer`. A fresh root/store is created with:

```text
personalrag-v2-indexer init --root <indexed-root> --store <index-store>
```

After filesystem updates, either `update` or native-Windows `watch` publishes a successor bundle. The GUI's `Reload index` deliberately reloads a published bundle; it is not itself a filesystem crawler. See `docs/V2_PRODUCT_LIFECYCLE.md`.

## 3. Search controls

The window exposes:

- **File / path**: Everything-style filename substring search by default.
- **Full path**: switches the first field to normalized full relative-path substring matching; `/` and `\\` are equivalent for this UI filter.
- **Content**: independent content query field.
- **Literal / Regex / Wildcard**: selects the frozen content semantics.
- **Case sensitive**: explicit opt-in; default is Unicode 15.1 NFC/full-fold case-insensitive behavior inherited from the backend.
- **Open**: opens the selected source file through Windows ShellExecute.
- **Show in Explorer**: selects the source path in Explorer.
- **Reload index**: atomically reloads the published Step 1–5 bundle.
- **More**: expands the current enumeration limit from the 100-file first batch by powers of two, up to the current metadata+delta visible upper bound.

Typing is debounced for 140 ms. Search is not executed on the UI thread.

## 4. Query composition

- File/path only -> Step 3/4 metadata fast path.
- Content only -> Step 2/4/5 content path.
- Both fields -> content enumeration plus filename/path predicate, i.e. deterministic AND.

The first content enumeration uses the architecture batch target of up to 100 files, up to 500 matches observed, and up to three snippets per file. `More` is Step 6's equivalent incremental-enumeration control. For a restrictive file/path predicate combined with a high-frequency content query, a matching file may lie outside the current enumeration window; `More` expands that window without changing query semantics.

Exact global hit count is intentionally not computed before the first batch.

## 5. Result contract

Each result row contains:

- stable FileID,
- file name,
- relative path,
- absolute source path,
- file size,
- modified timestamp rendered as UTC,
- file kind,
- up to three visible content matches.

The list view shows Name, Path, Hits, Location, Size, and Modified UTC. Selection updates a bounded UTF-8-safe preview.

Plain text locations are `Line N · byte X`. PDF/Office locations are `Unit N · byte X`, where `Unit N` is the Step 5 deterministic logical-unit ordinal. Step 7 may improve format-specific display labels (page/sheet/cell/slide) only if it preserves the frozen logical-unit identity and exact match location.

## 6. Concurrency / stale-result rule

`GuiSearchSession` is moved to a dedicated worker thread. The UI sends search/reload commands by channel and receives completion messages through `WM_APP`.

Every request receives a monotonically increasing request ID. Responses older than the latest request are ignored. Closing the window drops the UI state and asks the worker to quit. If posting a worker result to the window fails, the heap-owned response is reclaimed by the worker side.

This is a correctness rule: a slow prior query must never overwrite a newer query result.

## 7. Backend compatibility

Step 6 changes no durable identity:

- `PRV2IDX1` v2 unchanged,
- `PRV2MET1` v1 unchanged,
- `PRV2DEL1` v1 unchanged,
- `PRV2INC1` v1 unchanged,
- `PRV2BND1` v1 unchanged,
- `PRV2VER1` v1 unchanged,
- semantic identity `0x0003_0001` unchanged.

The limits APIs introduced for GUI continuation are additive query APIs. Existing first-batch APIs remain present and are regression-tested.

## 8. Step 6 acceptance boundary

Accepted on the Linux development host:

- platform-independent GUI search-session E2E against a real published Step 1–5 test bundle,
- metadata-only, content-only, AND, literal, regex, wildcard, case mode, path-separator normalization, preview, absolute path, and bounded enumeration tests,
- additive limits regression proving the old first-batch API remains unchanged,
- normal Rust fmt/clippy/full regression/release build,
- Windows GUI binary source forced-`cfg(windows)` metadata type-check with warnings denied.

Not accepted in Step 6 and therefore **must not be reported as PASS**:

- actual Win32 window launch/rendering on Windows,
- real keyboard/mouse/DPI/accessibility behavior,
- ShellExecute and Explorer integration on Windows,
- live NTFS/USN end-to-end indexing,
- pinned Windows packaging of `pdftotext`/`unzip`/`zstd`,
- target-Windows first-batch latency, memory, and full persistent-footprint SLO,
- crash/recovery behavior exercised as a complete Windows desktop application.

Those are Step 7 acceptance items.
