# PersonalRag Segment vNext Durable Compaction Report

Date: 2026-08-17

## 1. Scope

This change adds durable generation compaction / generation collapse on top of the existing vNext durable incremental generation store.

The existing `.prseg2` format, query implementation, and Perf12 production/oracle path are unchanged.

New public API:

```rust
compact_vnext_generation_store(root, segment_docs)
    -> VNextDurableCompactionReport
```

## 2. Compaction semantics

Given a CURRENT snapshot such as:

```text
base g0
 + delta g1
 + delta g2
 + delta g3
 + delta g4
```

compaction performs:

```text
CURRENT(g4)
  -> open/validate cumulative snapshot
  -> materialize live logical documents in logical-ID order
  -> build immutable compact base component at g5
  -> durable compact manifest at g5
  -> reopen exact persisted compact snapshot
  -> compare every live logical ID/path/content byte with source live snapshot
  -> re-check source CURRENT
  -> atomic CURRENT switch to g5
```

After the switch, CURRENT references one base layer only:

```text
compact base g5
```

Generation numbers remain monotonic. A compacted base does not reset the generation to zero.
A future delta can therefore publish as g6, and a later compaction can collapse it again as g7.

## 3. Newest-wins / tombstone result

Compaction materializes only the live logical snapshot resolved by the existing multi-generation newest-wins/tombstone layer.

After compaction:

- old physical versions are no longer referenced by CURRENT
- tombstoned documents are absent from the compact base
- compact manifest has one base layer
- delta count becomes zero
- tombstone event count becomes zero
- paths and normalized content are byte-identical to the pre-compaction live snapshot

## 4. Durability / crash boundary

CURRENT remains the single visibility switch.

The compact base component and compact manifest are fully written and fsync'd before CURRENT is changed. The persisted compact snapshot is reopened and every live path/content payload is compared with the source live snapshot while the old CURRENT is still authoritative.

Immediately before the visibility switch, the source CURRENT is re-read and checked again. This protects the supported single-writer workflow from intentionally publishing a stale background compaction.

A dedicated test confirms that even when the compact component and manifest are fully present, restoring/retaining the previous valid CURRENT exposes only the old snapshot.

Old immutable components/manifests are intentionally retained after CURRENT switches. They are safe GC candidates but are not deleted by compaction itself, so existing readers are not invalidated and Windows deletion semantics are not assumed.

## 5. Correctness tests

New durable compaction tests: 6/6 PASS.

Covered cases:

1. Multiple base/delta layers collapse to one compact base.
2. Live logical documents match source path/content bytes exactly.
3. Deleted/tombstoned documents do not reappear.
4. A compacted non-zero-generation base accepts a future delta.
5. A later second compaction succeeds again.
6. An empty live snapshot compacts to a zero-segment base.
7. Base-only compaction is rejected without changing CURRENT.
8. Target collision fails before CURRENT changes.
9. Fully written compact component/manifest remain invisible while CURRENT points to the source generation.

## 6. Full regression

After implementation:

```text
existing unit                 5 / 5   PASS
production oracle            35 / 35  PASS
vNext durable compaction      6 / 6   PASS
vNext durable generation     11 / 11  PASS
vNext generation              7 / 7   PASS
persistent index              3 / 3   PASS
vNext query                    9 / 9   PASS
vNext segment                 17 / 17  PASS
------------------------------------------------
total                         93 tests PASS

cargo fmt --check                      PASS
cargo clippy --all-targets -D warnings PASS
release build                          PASS
SELF_TEST_PASS                         PASS
```

## 7. 20k durable compaction benchmark

Corpus:

- 20,000 live documents
- base split into 4 segments
- delta generations with 1 / 10 / 100 / 1000 updated docs
- pre-compaction generation: g4
- 5 layers
- 8 physical referenced segments
- 1,111 cumulative tombstone events

Three release runs:

```text
compaction_ms
297.679
302.234
297.205

median = 297.679 ms
```

Pre/post shape:

```text
before: 5 layers / 8 segments / 1111 tombstone events
 after: 1 layer  / 4 segments / 0 tombstone events
live docs: 20,000 -> 20,000
```

Restart/open time:

```text
before_open_ms: 84.855 / 86.917 / 85.226
 after_open_ms: 57.585 / 55.818 / 61.682

median: 85.226 ms -> 57.585 ms
improvement: about 32.4%
```

CURRENT-referenced component footprint:

```text
before = 28,129,240 bytes
after  = 17,161,856 bytes
reduction = about 39.0%
```

This is referenced-snapshot footprint, not total on-disk store usage. Old immutable components remain until a separate safe GC step.

Representative queries were identical before and after compaction:

```text
common timeout:            20,000 hits
latest generation marker:  1,000 hits
rare marker:                   1 hit
updated path marker:        1,000 hits
```

One `/usr/bin/time -v` sample reported peak RSS around 96,208 KiB for the whole benchmark process.

## 8. Compatibility

No changes were made to:

- `.prseg2` segment format
- vNext segment writer/query format
- Perf12 production/oracle code

Existing durable manifests with generation-0 bases remain valid. Manifest validation was generalized so a compacted base may start at generation > 0 while generation order remains strictly increasing.

## 9. Remaining work before production switch

The next remaining durability item is safe garbage collection of obsolete/unreferenced components and manifests. It should not be coupled to the CURRENT switch because older mapped readers may still hold files, especially on Windows.

After safe GC, the final Gate 5 should run end-to-end A/B for:

- full build
- durable incremental 1 / 10 / 100 / 1000
- compaction
- restart/open
- common/medium/rare/zero-hit
- q1/q2
- Japanese / long substring / block boundary
- filename/path
- RSS / index bytes
- correctness mismatch = 0

Windows-native crash/power-loss durability remains a separate validation item; Linux directory fsync behavior is tested here and must not be presented as Windows-native PASS.
