# PersonalRag App Contract v1 / Facade / Performance Pass 2 Report

Date: 2026-08-15

## Result

Requested decoupling work is complete.

```text
frontend
   ↓ App Contract v1
src-tauri adapter
   ↓ SearchEngine / IndexEngine facade
bridge-core
   ↓ private Portable adapter
search-core
```

`src-tauri` no longer depends on or imports `personalrag-portable-search` directly.

## 1. App Contract v1

Canonical manifest:

- `app-contract/v1/contract.json`

Bridge-owned Rust DTOs:

- `bridge-core/src/contract_v1.rs`

Frontend DTOs:

- `frontend/src/app_contract_v1.ts`

Compatibility controls:

- `contract_info` returns contract name/version/capabilities.
- frontend verifies name/version at startup.
- Rust request DTOs use `deny_unknown_fields` to fail closed on accidental wire drift.
- canonical JSON fixtures are round-tripped by Rust tests.
- frontend tests compare Contract v1 constants and fixture field sets with the canonical manifest.
- `scripts/verify-boundaries.ps1` rejects direct Tauri→search-core dependency/import and verifies the facade/contract markers.

## 2. SearchEngine / IndexEngine facade

`bridge-core/src/engine.rs` owns the narrow application-facing boundary.

- `SearchEngine`
  - search
  - snippets
  - snippets_batch
- `IndexEngine`
  - scan
  - build

`PortableEngine` is the adapter that knows Portable Search Core APIs, sidecars and planner/index details.

Tauri owns application concerns only:

- settings
- status/cancellation
- GUI catalog persistence
- atomic publish
- file/open-parent shell integration

Portable Search Core remains unaware of Tauri/frontend.

## 3. Performance work kept outside search-core semantics

### 3.1 Top-K sort

When a non-path GUI sort has far more candidates than the requested limit, bridge-core now selects Top-K first and sorts only the selected prefix instead of sorting all candidates.

A/B: 50,000 candidates, size descending, limit=2,000.

- ProgressFix baseline median: 17.680 ms
- Contract/Facade optimized median: 13.794 ms
- Improvement: 21.98%
- Speedup: 1.282x
- result checksum: identical (`52055000`)

### 3.2 Batch snippets

The hit view previously issued up to 100 Tauri `snippets` IPC calls. It now issues one `snippets_batch` command. The bridge reads files with bounded parallelism (max 4 workers) and restores deterministic input order.

Internal A/B: 80 medium text files.

- sequential median: 83.102 ms
- batch median: 22.142 ms
- Improvement: 73.36%
- Speedup: 3.753x

This does not include the additional benefit of reducing Tauri IPC from up to 100 calls to one.

## 4. Regression / compatibility

### Portable Search Core

- rustfmt: PASS
- clippy `-D warnings`: PASS
- unit: 3/3 PASS
- production: 29/29 PASS
- release/self-test gates retained

No portable index-format or search-semantic changes were made in this pass.

### Bridge core

- rustfmt: PASS
- clippy `-D warnings`: PASS
- App Contract tests: 3/3 PASS
- normal integration tests: 6/6 PASS
- large scanner stress: PASS when explicitly enabled
- facade integration: PASS
- Top-K exact-semantics regression: PASS

### Frontend

- test files: 3/3 PASS
- tests: 15/15 PASS
- App Contract v1 compatibility tests: PASS
- TypeScript check: PASS
- Vite production build: PASS

### Windows target

Rust 1.97.1 / `x86_64-pc-windows-gnu`:

- search-core check/clippy: PASS
- bridge-core check/clippy: PASS
- Tauri check: PASS
- Tauri clippy `-D warnings`: PASS
- Tauri release link with the real built frontend assets: PASS
- output confirmed as `PE32+ executable`, Windows GUI, x86-64

The cross-linked executable is validation evidence only; the distribution continues to use `Build-And-Run.cmd` so the application is built natively on the user's Windows environment.

## 5. Change isolation after this pass

### GUI-only changes

Layout/CSS/presentation changes require no search-core change as long as App Contract v1 is preserved.

### Search-core changes

Internal data structure/planner/format implementation changes require no GUI/Tauri change as long as the bridge facade remains compatible.

### Intentional breaking changes

A wire DTO breaking change must create a new contract version. A facade breaking change must be handled in bridge/Tauri adapter code rather than leaking Portable Search Core types into the frontend boundary.

## Phase status

- App Contract v1: 100%
- Tauri→search-core direct dependency removal: 100%
- SearchEngine/IndexEngine facade: 100%
- compatibility tests: 100%
- scoped performance pass: 100%
- Windows target compile/link gate: 100%

