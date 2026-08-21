# Integration result — Performance Pass 1

- GUI visual layout/CSS: unchanged
- Portable Search Core index/query semantics: unchanged
- Q2/POS formats: unchanged
- GUI search/index controls: unchanged and connected
- Scanner normal-case relative-path allocation: reduced
- Parallel scanner progress Mutex contention: reduced
- Scan-time size/modified metadata: retained
- Scan → build duplicate metadata/canonicalize work: removed through `DiskPathInput`
- skipped-source → document-ID alignment: preserved through `source_indices`
- Result size/modified metadata restat: removed when new catalog metadata is present
- Old catalog compatibility: filesystem metadata fallback retained
- Case-insensitive plain/whole-word post-filter: allocation-free ASCII byte comparison
- Existing old path build API: retained
- 20k build A/B index bytes: identical
- Search Core full regression: 28/28 PASS
- Bridge normal tests: 4 PASS
- 50k scanner stress: PASS
- Windows search/bridge check + clippy + link: PASS
- USN/MFT/incremental watcher: not added

## App Contract v1 / Facade finalization (2026-08-15)

- `src-tauri -> search-core` direct dependency/import: removed
- bridge `SearchEngine` / `IndexEngine`: added
- App Contract v1 canonical manifest + Rust/TypeScript compatibility tests: added
- frontend startup contract version check: added
- hit snippets: one batched IPC + bounded parallel read
- non-path sort: exact Top-K fast path
- Windows target Tauri check/clippy/release link: PASS
- frontend: 15/15 tests + production build PASS
