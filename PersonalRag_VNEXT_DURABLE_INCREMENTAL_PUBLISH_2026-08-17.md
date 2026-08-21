# PersonalRag Segment Format vNext - Durable Incremental Generation Publish

Date: 2026-08-17

## Scope

This change promotes the previously in-memory vNext multi-segment generation model into a durable on-disk generation store.

The existing Perf12 production/oracle implementation and the `.prseg2` format/writer/query code are unchanged. The durable layer is implemented in a new module around immutable `.prseg2` components.

## New public API

- `initialize_vnext_generation_store(root, documents, segment_docs)`
- `publish_vnext_incremental_generation(root, plan, segment_docs)`
- `open_vnext_published_generation(root)`
- `verify_vnext_generation_store(root)`
- `VNextDurableGenerationReport`

`publish_vnext_incremental_generation` consumes the existing `UpdatePlan`, so it integrates directly with `plan_incremental_update` / `apply_update_plan`.

## On-disk layout

Example after generation 4:

```text
STORE/
├ CURRENT
├ generations/
│  ├ g0000000000000000-base.manifest
│  ├ g0000000000000001-delta.manifest
│  ├ g0000000000000002-delta.manifest
│  ├ g0000000000000003-delta.manifest
│  └ g0000000000000004-delta.manifest
└ components/
   ├ base-g0000000000000000/
   │  ├ segment-00000.prseg2
   │  └ ...
   ├ delta-g0000000000000001/
   │  ├ segment-00000.prseg2
   │  └ tombstones.bin
   └ ...
```

The generation manifest is cumulative. Each layer records its generation, immutable `.prseg2` files, and the delta tombstone file.

## Durable publish protocol

```text
1. Read and checksum-validate CURRENT
2. Read and checksum/semantic-validate current generation manifest
3. Validate UpdatePlan generation and payload
4. Build delta in components/.publish-*.tmp/
5. fsync each segment/tombstone file
6. fsync staging component directory
7. rename staging directory -> immutable final component
8. fsync components/
9. write + fsync checksum-protected cumulative generation manifest
10. rename manifest to final name
11. fsync generations/
12. re-read persisted manifest
13. fully open/validate all referenced layers and newest-wins/tombstone semantics
14. verify `live_docs_after`
15. write + fsync temporary CURRENT
16. atomic rename temporary CURRENT -> CURRENT
17. fsync store root (Unix)
18. re-read only CURRENT and verify the published pointer
```

`CURRENT` is the only visibility switch. A crash before step 16 can leave an orphan component and/or manifest, but readers continue to use the previous valid CURRENT snapshot.

A full cumulative segment validation is performed once before CURRENT changes. It is intentionally not repeated after the visibility switch, because doing so made one-document delta latency scale with the entire base generation without adding a new crash-safety guarantee.

## Integrity / fail-closed behavior

### CURRENT

Text format magic: `PRVCU001`

Contains:

- generation
- relative generation-manifest path
- FNV-1a checksum over the body

Unsafe absolute/parent paths are rejected even when the checksum is valid.

### Generation manifest

Text format magic: `PRVGM001`

Contains cumulative:

- published generation
- layer count
- base/delta layer type
- layer generation
- tombstone file
- segment count and paths
- FNV-1a checksum

Validation includes:

- exactly one generation-0 base first
- subsequent layers are delta only
- strict generation ordering
- newest layer generation equals published generation
- component paths must stay below `components/`
- segment paths must end in `.prseg2`
- delta layers must reference tombstones
- duplicate physical paths rejected

### Tombstones

Binary magic: `PRVTMB01`

Contains:

- count
- sorted unique non-zero logical IDs as LE u64
- FNV-1a checksum

Corrupt/truncated/checksum-mismatched tombstones fail closed.

## Segment bounding

Durable publication keeps local IDs segment-local.

Segments are split by both:

- requested `segment_docs` (1..=65535)
- estimated 8 KiB block count <= 65535

A single document that would exceed the local block bound is rejected before publication.

## Tests

New durable tests: **11/11 PASS**

Coverage:

1. initialization survives restart and spans multiple segments
2. update + rename + insert + delete survives restart
3. tombstone-only generation
4. stale UpdatePlan does not switch CURRENT
5. CURRENT corruption fails closed
6. manifest corruption fails closed
7. tombstone corruption fails closed
8. unsafe `../` CURRENT manifest path rejected even with a valid checksum
9. orphan files are ignored while CURRENT remains old
10. fully written future component/manifest remains invisible without CURRENT switch
11. semantic validation failure before CURRENT leaves old snapshot visible
12. multiple durable generations restore only newest version
13. integration with `CatalogSnapshot -> plan_incremental_update -> publish -> apply_update_plan`

(Some behaviors are combined within the 11 test functions.)

## Full regression

Post-change Search Core:

```text
existing unit             5 / 5  PASS
production oracle        35 / 35 PASS
vNext durable            11 / 11 PASS
vNext generation          7 / 7  PASS
vNext persistent          3 / 3  PASS
vNext query               9 / 9  PASS
vNext segment            17 / 17 PASS
--------------------------------------
total                    87 tests PASS
cargo fmt --check                 PASS
Clippy -D warnings                PASS
release pr_portable               PASS
SELF_TEST_PASS                    PASS
```

## 20k durable incremental smoke

Base:

```text
20,000 logical docs
4 base segments
segment_docs = 5,000
```

Then sequential durable delta generations:

```text
g1:    1 updated document
g2:   10 updated documents
g3:  100 updated documents
g4: 1000 updated documents
```

After every publish, a fresh `open_vnext_published_generation()` was constructed only from CURRENT and durable files, and the generation-specific marker hit count was checked.

Three-run publish measurements (Linux sandbox; fsync included):

| changed docs | runs (ms) | median (ms) |
|---:|---|---:|
| 1 | 109.751 / 330.097 / 136.518 | 136.518 |
| 10 | 127.876 / 133.537 / 129.868 | 129.868 |
| 100 | 164.492 / 174.949 / 126.083 | 164.492 |
| 1000 | 156.747 / 164.528 / 163.180 | 163.180 |

The 1-document path was initially roughly 300 ms because the first implementation opened/validated the full cumulative snapshot three times per publish. The final implementation validates it once before CURRENT and reduced normal small-delta runs to roughly 110-165 ms in this environment. The 330 ms sample demonstrates storage/fsync variability and is retained rather than discarded.

Example final CURRENT:

```text
PRVCU001
generation 4
manifest generations/g0000000000000004-delta.manifest
checksum 5dd531a27b1638f6
```

Final state:

```text
generation=4
layers=5
segments=8
live_docs=20000
```

## Known limitations / next work

1. Orphan components/manifests can remain after a crash before CURRENT. They are safely ignored, but automatic orphan garbage collection is not implemented yet.
2. Cross-process concurrent writers are not a supported contract yet. The application should serialize durable publishes.
3. Unix directory `fsync` is exercised in this Linux validation. Native Windows power-loss durability has **not** been proven and must not be reported as PASS from this run.
4. Generation manifests grow with delta count; compaction is the next mechanism that should collapse old layers/tombstones into a new base.
5. Final production decision still requires durable compaction plus final incremental/compaction A/B.

## Current phase

```text
vNext format/query optimization            DONE
multi-segment newest-wins/tombstones        DONE
durable incremental generation publish      DONE
restart recovery from CURRENT               DONE
1/10/100/1000 durable publish smoke         DONE

next: durable compaction / generation collapse
then: final Gate 5 production A/B
```
