# PersonalRag V2 Step 4 Incremental Index — Canonical Completion Report

Date: 2026-08-29  
Status: **COMPLETE / CANONICAL**

## Scope completed

Step 4 adds Windows-oriented incremental filesystem synchronization on top of the frozen Step 1/2/3 search formats without changing those formats.

Implemented:

- base + immutable delta/overlay model,
- O(1) overlay path ownership,
- create/modify/rename/move/delete handling,
- same-path FileID replacement suppression,
- base content reuse for rename/move,
- lazy cached mini Variant-D index for changed content,
- 50k/2% periodic compaction threshold,
- `PRV2DEL1` delta persistence,
- `PRV2INC1` USN checkpoint/pending-rename persistence,
- `PRV2BND1` four-generation atomic bundle commit,
- corrupt/new-orphan fallback and GC protection,
- USN_RECORD_V2 parsing and Windows FSCTL query/read/enumeration adapter,
- rename OLD/NEW state machine,
- journal-gap/reset detection,
- full reconciliation,
- crash/restart tests.

## Final 1M storm evidence

```text
SEARCH case=created p50_ms=3.187 max_ms=8.016 hits=1
SEARCH case=renamed_new p50_ms=2.816 max_ms=5.065 hits=1
SEARCH case=renamed_old p50_ms=2.822 max_ms=3.299 hits=0
SEARCH case=deleted p50_ms=2.892 max_ms=3.347 hits=0
SEARCH case=base_rare p50_ms=2.863 max_ms=3.221 hits=1
STORM base_records=1000000 base_bytes=135318629 build_ms=6823.645 create_count=10000 create_ms=5.789 rename_count=10000 rename_ms=8.679 delete_count=10000 delete_ms=2.323 delta_changes=30000 compact=true delta_bytes=1910064 publish_ms=20.633 reload_ms=131.341
```

The base metadata snapshot is not rebuilt during the event storm.

## Crash/restart evidence

Individually rerun and PASS:

- bundle commit-point / orphan generation safety,
- corrupt newest bundle/reference fallback,
- two-valid-bundle GC reference preservation,
- corrupt delta advisory-pointer fallback,
- durable USN checkpoint + pending rename round-trip,
- rename-old checkpoint hold,
- reconciliation create/modify/rename/delete repair,
- same-path new-FileID stale content suppression.

## Review repairs

Problems found and repaired before sealing included:

1. O(delta^2) path conflict checks -> resident O(1) path-owner map.
2. Metadata query full-base stale scan -> candidate-only stale checks with staged expansion.
3. Bundle GC initially referenced the wrong content-generation filename pattern -> corrected to real `gen-*.prv2` generations.
4. Old bundles could lose referenced component generations -> GC now protects all four component generations for retained bundles.
5. Bundle validity now includes the referenced incremental state generation.
6. Unresolved rename OLD could advance the durable checkpoint -> checkpoint is held until resolved.
7. Same-path replacement by a new FileID could expose stale old results -> old metadata/content is suppressed.
8. Content queries rebuilt stable-ID mapping and changed-content indexes on every query -> both are now lazy cached and invalidated only when required.

## Gate

Final Rust 1.97.1 gate:

- `cargo fmt -- --check`: **PASS**
- `cargo clippy --offline --locked --all-targets -- -D warnings`: **PASS**
- tests: **66/66 PASS**
- release build: **PASS**
- forced-`cfg(windows)` USN module typecheck: **PASS**

The detailed output is recorded in `evidence/step4-incremental/full-regression-final.txt` and `STATE.json`.

## Windows caveat

Windows-only USN code is type-checked and the parser/state machine is tested with synthetic USN records. A live Windows NTFS USN E2E run could not be executed on the Linux host and is explicitly deferred to Step 7 target-Windows acceptance.

## Next

Step 5: PDF / Office extraction and verification store.
