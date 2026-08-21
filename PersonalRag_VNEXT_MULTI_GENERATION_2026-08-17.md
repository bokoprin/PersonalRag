# PersonalRag Segment vNext Multi-Segment Generation Report

Date: 2026-08-17

## Scope

This change promotes the vNext `.prseg2` prototype from a single-segment search unit to a
read-only multi-segment logical generation snapshot.

Implemented:

- one base layer + ordered delta layers
- multiple `.prseg2` files inside one layer
- logical IDs independent from per-segment u16 local IDs
- newest-generation-wins visibility
- tombstone deletion semantics
- tombstone-then-upsert semantics inside the same delta
- content/path/name query merge across all physical segments
- stable logical-ID sorted results
- live snapshot materialization for later compaction/full-rebuild work

The existing Perf12 production format and existing `.prseg2` serialized format are unchanged.

## Public API

New module: `search-core/src/vnext_generation.rs`

Main types:

- `VNextGenerationLayerKind::{Base, Delta}`
- `VNextGenerationLayerSpec`
- `VNextGenerationIndex`

Key methods:

- `VNextGenerationIndex::open(generation, layers)`
- `search_content`
- `search_path`
- `search_name`
- `live_logical_ids`
- `contains_logical_id`
- `materialize_live_documents`

## Visibility semantics

Layers are applied oldest to newest.

For each delta layer:

1. remove logical IDs listed in tombstones
2. apply every upsert in all `.prseg2` files belonging to the layer
3. an upsert replaces any older physical location for the same logical ID

After all layers are opened, only the final physical location for each live logical ID is marked
visible. Querying an older immutable segment can still produce a physical hit, but the generation
layer discards it unless that physical document is the final live location.

Therefore:

- explicit tombstone only -> document is deleted
- same-layer tombstone + upsert -> new upsert is live
- newer upsert without a redundant tombstone -> newer physical version wins
- update/rename hides the old content and old path

## Validation / fail-closed rules

`VNextGenerationIndex::open` rejects:

- missing base layer
- a non-base first layer
- multiple base layers
- non-increasing generation numbers
- layer generation newer than the published generation
- published generation not equal to the newest layer generation
- base-layer tombstones
- unsorted/duplicate/zero tombstones
- logical ID zero in a generation segment
- duplicate logical IDs inside the same generation layer, including duplicates across two
  different `.prseg2` files in that layer

## Correctness hard gate

New `tests/vnext_generation.rs`: 7 tests.

Covered:

- newest upsert hides old physical content and old filename/path
- newest wins even when no redundant update tombstone is supplied
- tombstone-only delta deletion
- multiple segments in one layer with overlapping local u16 IDs
- malformed/ambiguous layer rejection
- multi-generation vNext == materialized vNext full rebuild == naive oracle
- vNext multi-generation == Perf12 `MergedIndex` for newest-wins/tombstone queries

Final expected regression set:

- existing unit: 5/5
- production: 35/35
- vNext generation: 7/7
- vNext persistent: 3/3
- vNext query: 9/9
- vNext segment: 17/17
- doc tests: PASS
- fmt: PASS
- Clippy `-D warnings`: PASS
- release `pr_portable`: PASS
- `SELF_TEST_PASS`

## Realistic 20k smoke

Reusable example added:

`search-core/examples/vnext_generation_bench.rs`

Run used:

```text
vnext_generation_bench 20000 100
```

Shape:

- 20,000 base documents
- base split across 2 `.prseg2` files
- 100 updated logical IDs
- 50 deleted logical IDs
- 50 inserted logical IDs
- 1 delta `.prseg2`
- 150 tombstone events total
- 3 physical segments total
- 20,000 final live logical documents

Observed in the current Linux container:

```text
open_ms=53.853
common `timeout common`: 20,000 hits, ~1.787 ms
old updated-version marker: 0 hits, ~0.081 ms
new updated-version marker: 1 hit, ~0.013 ms
deleted-version marker: 0 hits, ~0.008 ms
untouched rare marker: 1 hit, ~0.014 ms
inserted rare marker: 1 hit, ~0.008 ms
zero hit: 0 hits, ~0.010 ms
peak RSS for the whole build/open/full-rebuild smoke: ~53 MiB
```

These timings are environment-specific and are not a production benchmark. The hard result is that
every generation query matched a materialized full-rebuild segment while stale physical versions
and tombstoned documents remained invisible.

## Not implemented in this step

This step deliberately does not yet add the durable vNext update/publish pipeline.

Remaining production work:

- durable vNext generation manifest / CURRENT atomic publish
- incremental delta segment builder for 1/10/100/1000 changed docs
- crash-safe tombstone sidecar/manifest persistence
- compaction from multiple layers into a new optimized base generation
- final Gate 5 A/B including incremental build and compaction

`materialize_live_documents()` was added specifically as a correctness bridge for the coming
compaction implementation.

## Status

Multi-Segment Generation + newest-wins/tombstone semantics: COMPLETE.

Production switch: NOT YET. The next major gate is durable incremental generation publishing and
compaction semantics.
