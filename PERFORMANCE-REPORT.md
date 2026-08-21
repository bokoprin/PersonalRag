# PersonalRag Performance Pass 1 Report

Date: 2026-08-15

## Scope

Baseline: `PersonalRag_GUI_PortableCore_FullConnected_1M_2026-08-15`

Goal: find and implement evidence-backed optimizations that improve the current GUI/bridge path for large Windows roots without changing the Portable Search Core persistent format or query semantics.

## Pre-change gate

Portable Search Core before modification:

- `cargo fmt -- --check`: PASS
- `cargo clippy --offline --all-targets -- -D warnings`: PASS
- unit: 3/3 PASS
- production: 28/28 PASS
- release build: PASS
- `SELF_TEST_PASS`

Bridge validation required vendoring `ignore`/`regex` because the ChatGPT Linux environment had no crates.io network. A temporary GitHub Actions vendor bridge was used only to create an offline local dependency cache. During this step two pre-existing bridge gate issues were found: Rust 1.97 clippy debt and an incorrect Regex test expectation. Both were corrected before the optimization gate was frozen.

## Implemented optimization 1 — scanner hot path

### Problem

The million-file parallel scanner still did unnecessary work on every filesystem entry:

- relative-path String construction even when no custom relative-path pruning was configured
- Mutex activity for GUI current-path reporting at very high frequency

### Change

- skip relative-path construction in `filter_entry` unless `custom_relative_paths` is non-empty
- update `current_path` only at progress sampling intervals
- preserve worker-local 4096-entry batches before shared list merge
- retain scan-time size and modified timestamp in `ScannedFile`

### A/B

100,000-file synthetic tree; 80,000 selected, 20,000 below excluded directories; parallel scanner; 9 runs.

- before median: 50.250 ms
- after median: 46.615 ms
- speedup: 1.078x
- reduction: 7.2%

## Implemented optimization 2 — scan→build zero-rework handoff

### Problem

The GUI scanner already had path, display path and size, but the Portable build path repeated:

- `metadata()`
- per-file canonicalization / relative-display-path work

before reading file content.

### Change

Added `DiskPathInput` and `build_disk_path_inputs_index_pipelined`.

The old `build_disk_paths_index_pipelined` API remains unchanged. The GUI uses the new application-scanner boundary and passes known metadata directly into the same bounded hydration / immutable segment pipeline.

`DiskPathBuildReport::source_indices` maps successful documents back to scan-list entries so read/skipped files cannot misalign GUI metadata with document IDs.

### Correctness

The production test builds the same corpus through the old path API and new metadata-aware API and checks all generated index files byte-for-byte. A/B benchmark runs also compared paired output directories with no differences.

### A/B

20,000 files, 4 segments, 7 runs.

- old API median: 58.554 ms
- fast API median: 41.965 ms
- speedup: 1.395x
- reduction: 28.3%
- output index size: 5,123,904 bytes on both paths
- byte-identical: YES

## Implemented optimization 3 — GUI result metadata cache and allocation-free case folding

### Problem

For non-path sorting such as size/modified, GUI result construction performed `fs::metadata()` for candidate hits even though the scanner had already collected those fields at index time.

Case-insensitive plain/whole-word post-filtering also allocated lowercased copies of candidate text and query.

### Change

- GUI catalog now stores aligned `size_bytes` and `modified_ns`
- new `search_catalog_with_metadata` avoids candidate-by-candidate filesystem stat when metadata arrays match catalog length
- legacy catalogs automatically fall back to filesystem metadata
- case-insensitive plain / Whole Words matcher now performs ASCII-folded byte comparison without allocating folded Strings

### A/B

20,000-hit content query, sort=size descending, limit=100, 9 runs.

- old metadata restat median: 25.446 ms
- catalog metadata median: 7.177 ms
- speedup: 3.545x
- reduction: 71.8%

## Post-change regression

Portable Search Core:

- fmt: PASS
- clippy `-D warnings`: PASS
- unit: 3/3 PASS
- production: 28/28 PASS
- release: PASS
- self-test: PASS

GUI bridge:

- fmt: PASS
- clippy `-D warnings`: PASS
- normal tests: 4 PASS / 0 FAIL / 1 ignored stress
- explicit 50k-file release stress: PASS

Tauri source:

- rustfmt: PASS

Frontend:

- entire `frontend/` tree byte-identical to baseline: PASS

## Windows gate

Rust 1.97.1 target: `x86_64-pc-windows-gnu`

- search-core all-targets check: PASS
- search-core all-targets clippy `-D warnings`: PASS
- search-core Windows test executable link: PASS
- bridge-core all-targets check: PASS
- bridge-core all-targets clippy `-D warnings`: PASS
- bridge-core Windows test executable link: PASS

The linked test binaries are PE32+ x86-64 executables. The immediately preceding FullConnected Tauri baseline was also built and run on the user's Windows machine, including real index creation and GUI searches. The final package includes `Build-And-Run.cmd` to rebuild/check the optimized Tauri layer natively on that Windows environment.

## Compatibility / trade-offs

- Portable on-disk index format: unchanged
- Q2/POS sidecar formats: unchanged
- query semantics: unchanged
- old path build API: retained
- old GUI catalog: load-compatible via `serde(default)` metadata arrays and filesystem fallback
- new catalog adds 16 bytes of numeric metadata per indexed document before serialization overhead; this trades modest catalog memory/disk growth for removal of repeated per-result stat calls
- metadata shown/sorted is the metadata captured by the index snapshot, consistent with the current full-rebuild model

## Recommendation

Adopt all three optimizations. They target the application bridge around the already-mature search engine and showed measurable improvements without changing the frozen persistent search formats.
