# PersonalRag vNext Production Rollout Report

Date: 2026-08-17

## Decision

The Search Core rollout plumbing is implemented. Perf12 remains the persisted default and rollback/correctness backend until the **current source** passes Windows native acceptance. Linux Search Core validation passes and the vNext ProductionCandidate Gate 5 profile is preserved.

Current rollout state:

- Linux Search Core rollout implementation: PASS
- Perf12 rollback/oracle retained: PASS
- vNext shadow dual-write and generation fallback: implemented
- Runtime `perf12` / `shadow` / `vnext` switch: implemented
- Shadow result comparison telemetry: implemented
- Common-result telemetry: implemented
- Windows native validation harness: implemented
- Windows native validation of this exact source: **NOT RUN in this Linux environment**
- Persisted default: **Perf12**

## Backend modes

### `perf12`

Stable default. Perf12 serves search results. A vNext shadow is not required.

### `shadow`

Perf12 remains authoritative and serves the user-visible result. vNext is opened at the same GUI catalog generation and the complete result vector is compared against Perf12. The engine records:

- total searches
- Perf12/vNext served searches
- vNext fallbacks
- shadow comparisons
- shadow mismatches
- common-result searches (`>= 8192` results)
- common-result total latency
- common-result maximum latency
- last search latency

The GUI shows requested backend, active backend, vNext readiness, fallbacks, shadow mismatch count, and common-result average/max latency.

### `vnext`

vNext serves results only when its durable published generation matches the GUI catalog generation. If the vNext store is absent, stale, corrupt, or otherwise cannot be opened, the read path automatically falls back to Perf12.

## Full rebuild rollout behavior

In `shadow` or `vnext` mode the normal Perf12 build completes first inside the existing temporary build directory. The vNext durable shadow is then built from `MergedIndex::live_documents()` from that **already built and verified Perf12 generation**.

This deliberately avoids rereading source files and guarantees that the two backends start from the same logical IDs, display paths, and normalized content bytes.

The top-level existing atomic build directory publication therefore moves Perf12 and the vNext shadow together on a successful full rebuild.

## Incremental rollout behavior

Perf12 remains authoritative during rollout:

1. calculate one `UpdatePlan`
2. publish the Perf12 generation
3. run the existing Perf12 compaction policy when required
4. best-effort publish the exact same `UpdatePlan` into the vNext durable shadow
5. apply vNext compaction/GC when the Perf12 policy compacted
6. verify vNext when the follower succeeds

A vNext follower failure does not roll back or fail an already valid Perf12 publication. Instead, vNext becomes stale and generation matching on the read path causes an automatic Perf12 fallback.

This is intentional rollout fail-safe behavior.

## Old Perf12-only indexes

When the requested Search Core mode is `shadow` or `vnext`, incremental eligibility requires `vnext-store/CURRENT`. Therefore switching an existing Perf12-only installation to a vNext rollout mode causes a full rebuild rather than allowing the installation to remain indefinitely without a vNext shadow.

`PERSONALRAG_SEARCH_CORE_BACKEND=perf12|shadow|vnext` can override the persisted setting for one process. The in-memory setting is aligned to the effective override so background incremental eligibility uses the actual requested mode. The environment override is not written back to `settings.json`.

## Search Core validation

Rust 1.97.1, Linux x86-64, offline.

Search Core final regression after rollout changes:

- unit: 5/5 PASS
- production oracle: 35/35 PASS
- production shadow equivalence: 1/1 PASS
- durable compaction: 6/6 PASS
- durable GC: 5/5 PASS
- durable generation: 12/12 PASS
- vNext generation: 8/8 PASS
- persistent index: 5/5 PASS
- vNext query: 9/9 PASS
- vNext segment: 17/17 PASS
- total: **103/103 PASS**
- Clippy `-D warnings`: PASS
- release examples build: PASS
- `pr_portable self-test`: `SELF_TEST_PASS`

The new shadow-equivalence test creates matching Perf12 and vNext generations, publishes the same incremental `UpdatePlan`, and checks generation/live-doc/query equality including deletion semantics.

## Representative Linux Gate 5 smoke after rollout plumbing

20,000 documents, 7 query rounds:

```text
GATE5_BUILD perf_ms=674.659 vnext_ms=651.042
GATE5_OPEN  perf_p50_ms=19.272 vnext_p50_ms=19.410

base common:
  Perf12 0.203381 ms
  vNext  0.808300 ms

base rare:
  Perf12 0.018117 ms
  vNext  0.004827 ms

base filename:
  Perf12 0.152416 ms
  vNext  0.006229 ms

Delta 1:
  Perf12 35.432 ms
  vNext   9.291 ms

Delta 10:
  Perf12 36.836 ms
  vNext  11.103 ms

Delta 100:
  Perf12 34.385 ms
  vNext  22.756 ms

Delta 1000:
  Perf12 44.234 ms
  vNext  22.570 ms

Compaction:
  Perf12 2603.096 ms
  vNext   798.484 ms
```

The known high-true-hit common-query tradeoff remains; the rollout plumbing did not introduce a new Search Core performance regression.

## Bridge / Tauri / Frontend validation status in this environment

The current Linux container does not contain the complete external Cargo vendor used by Bridge/Tauri. Offline Cargo therefore stops **before source compilation**:

- Bridge: missing `ignore`
- Tauri: missing `serde`

This matches the previously documented validation-environment limitation and is not reported as a source PASS.

What was validated here:

- modified Bridge/Tauri Rust files parse and pass `rustfmt --check`
- frontend source passes a validation-only strict TypeScript check with a minimal stub for `@tauri-apps/api/core`
- normal frontend `tsc` is blocked because saved `node_modules` / type packages are not present in this container

No validation-only stubs/configuration are included in the distribution ZIP.

## Windows native hard gate

Run on Windows 11 with the normal project dependencies available:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\validate-vnext-production-switch-windows.ps1 -LaunchShadow
```

The script runs:

1. existing full Windows GUI regression/build
2. Search Core Perf12-vNext shadow equivalence test
3. durable restart / compaction / GC tests
4. Bridge tests and Clippy
5. Tauri check and Clippy
6. Windows-native 20k Gate 5 smoke
7. evidence summary creation
8. optional GUI launch with `PERSONALRAG_SEARCH_CORE_BACKEND=shadow`

The Windows script writes evidence under `windows-vnext-validation/`.

## Promotion procedure

Do not change the persisted default from Perf12 yet.

After the Windows script passes for this exact source:

1. launch in `shadow`
2. perform the intended real-data burn-in
3. confirm `shadow_mismatches == 0`
4. confirm fallbacks are understood/zero under normal operation
5. review common-result average/max telemetry
6. change Search Core backend to `vnext` from the GUI
7. keep Perf12 available as rollback/oracle during the initial production window
8. demote/remove Perf12 only after the production telemetry window is accepted

## Files changed from the ProductionCandidate baseline

- `bridge-core/src/engine.rs`
- `bridge-core/src/lib.rs`
- `bridge-core/src/contract_v1.rs`
- `src-tauri/src/main.rs`
- `frontend/src/app_contract_v1.ts`
- `frontend/src/main.ts`
- `search-core/tests/production_switch_shadow.rs` (new)
- `scripts/validate-vnext-production-switch-windows.ps1` (new)
- `README.md`
- this report

The vNext `.prseg2` format, Gate 5 indexing/query algorithms, and Perf12 Search Core format are not redesigned by this rollout change.
