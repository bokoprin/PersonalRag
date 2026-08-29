> **HISTORICAL / SUPERSEDED:** This report records an earlier development wave. The normative Step 1 + Step 2 state is `STEP12_CONTENT_SEARCH_CANONICAL_2026-08-28.md`, `HANDOFF.md`, `STATE.json`, and the frozen docs.

# Step 2 persistent q4/q5 integration result

Date: 2026-08-28

## Scope

Integrated Variant D (adaptive q4/q5 Bloom presence + rare q4/q5 postings) into the production persistent-index format and verified that its candidate-reduction behavior survives publish, process-lifetime separation, reload, and source-span exact verification.

## Implemented

- `PRV2IDX1` format v1
- seven checksummed sections
- CRC64-ECMA per section + whole-file footer
- immutable generations
- generation parent linkage
- advisory `CURRENT`
- fallback when `CURRENT` is corrupt/missing
- fallback when latest generation is corrupt
- GC retaining at least two valid generations
- GC protection of a valid stale `CURRENT` generation
- collision-free next generation number after orphan generations
- exact relative paths (Unix raw / Windows UTF-16LE)
- source size / modified-time drift checks
- explicit full source CRC validation
- 64-bit persistent posting offsets
- actual-size production sparse re-selection
- Variant D q4/q5 persistent section
- persistent benchmark harness
- table-driven CRC64 acceleration

## Regression gates

Before integration (Q45 wave source):

- fmt PASS
- clippy `-D warnings` PASS
- tests 13/13 PASS
- release build PASS

After integration/review:

- existing Q45 tests: 13/13 PASS
- persistent focused tests: 8/8 PASS
- CRC standard-vector test: PASS
- total Rust tests: 22/22 PASS
- clippy `-D warnings`: PASS
- release build: PASS

## 96 MiB controlled persistent result

- selected source: 100,663,296 bytes
- index: 1,310,513 bytes
- index/source: 1.301878%
- q4/q5 section: 506,920 bytes
- blocks: 96
- publish: 3,873.689 ms
- load: 10.643 ms
- q3 `abd`: p50 1.032 ms, 1 block
- q4 `wxyz`: p50 0.862 ms, 1 block
- q5 `klmno`: p50 0.969 ms, 1 block
- adversarial `abcde`: p50 ~0 ms, 0 blocks
- Japanese first batch: p50 0.212 ms

## 256 MiB controlled persistent result

- selected source: 268,435,456 bytes
- index: 3,484,857 bytes
- index/source: 1.298210%
- q4/q5 section: 1,345,784 bytes
- blocks: 255
- publish: 12,656.132 ms
- load: 28.195 ms
- q3 `abd`: p50 1.148 ms, 1 block
- q4 `wxyz`: p50 0.760 ms, 1 block
- q5 `klmno`: p50 0.895 ms, 1 block
- adversarial `abcde`: p50 ~0 ms, 0 blocks
- Japanese first batch: p50 0.188 ms

The q4/q5 acceleration therefore survives production serialization and reload without losing the 96->1 / 255->1 rare-anchor behavior or the adversarial 96/255->0 absence shortcut.

## Review fixes

1. Capacity SLO is enforced as an acceptance gate instead of refusing tiny-corpus publication.
2. Production sparse anchors are reselected by selectivity using actual production record/posting sizes.
3. A stale but valid `CURRENT` generation is protected from GC even when newer orphan generations exist.
4. Next generation uses the current valid parent but allocates a number above every existing generation filename to avoid orphan collisions.
5. Windows `CURRENT` replacement is explicitly advisory/crash-recoverable rather than relying on unsupported overwrite-rename semantics.
6. CRC64 was changed from bit-at-a-time to a verified table-driven implementation (`123456789` check vector) to remove avoidable publish overhead.

## Source-history caveat

The recoverable real source used for this integration is the Q45 performance-wave tree. The earlier Step 1 Unicode/regex implementation source was not present in the active filesystem, although its frozen specification/report artifacts remain. Therefore this artifact proves **Q45 + production persistent-format integration**; it must not be represented as proof that the missing Step 1 implementation was magically recovered.
