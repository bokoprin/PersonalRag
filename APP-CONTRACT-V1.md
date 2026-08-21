# PersonalRag App Contract v1

`app-contract/v1/contract.json` is the canonical GUI ↔ Tauri wire-contract manifest.

## Dependency rule

```text
frontend
   ↓ App Contract v1 (JSON/DTO)
src-tauri adapter
   ↓ SearchEngine / IndexEngine facade
bridge-core
   ↓ private portable adapter
search-core
```

Rules:

1. `src-tauri` must not depend on or import `personalrag-portable-search` directly.
2. `src-tauri` calls only the bridge-owned `SearchEngine` / `IndexEngine` facade for indexing/search.
3. Search-core public/internal changes are allowed without GUI changes while the facade contract is preserved.
4. GUI layout/presentation changes are allowed without search-core changes while App Contract v1 is preserved.
5. Wire DTO changes require an explicit App Contract version change or an additive v1-compatible change documented in the manifest.
6. Request DTOs reject unknown fields in Rust so accidental frontend/backend drift fails visibly rather than being silently ignored.
7. `contract_info` is checked by the frontend at startup. A name/version mismatch stops normal initialization and displays a contract error.

## Performance boundary

The facade is also the place for application-specific performance work that should not leak into search-core:

- batch snippet retrieval (one Tauri IPC instead of up to 100 calls, bounded parallel file reads),
- GUI sort Top-K selection when result `limit` is much smaller than candidate count,
- scan-time metadata reuse and GUI catalog handling.

Portable index formats and search semantics remain owned by `search-core`.
