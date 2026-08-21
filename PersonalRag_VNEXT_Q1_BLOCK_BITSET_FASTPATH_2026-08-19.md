# PersonalRag vNext Q1 Block-Bitset Fast Path Report

Date: 2026-08-19

## Summary

The current vNext full-build bottleneck was profiled after the Balanced Perf12 acceleration work. On the representative 20,000-document synthetic corpus, vNext segment index construction remained the dominant CPU stage. With four 5,000-document segments built concurrently on the current 5-vCPU environment, each segment receives `cpu_budget=1`, and the content q1/q2/q3 builders dominate `index_group_ms`.

This change optimizes **content q1 construction** without changing the `.prseg2` format, query semantics, segment layout, or durable bytes.

## Baseline gate before implementation

Executed on the exact input source before modification:

- `cargo fmt -- --check`: PASS
- `cargo clippy --offline --all-targets -- -D warnings`: PASS
- `cargo test --offline`: **151 / 151 PASS**

Representative pre-change medians, 20,000 docs, 5,000 docs/segment, five runs each:

| Payload / doc | Baseline median |
|---:|---:|
| 512 B | 99.507 ms |
| 1 KiB | 136.063 ms |
| 2 KiB | 251.817 ms |
| 4 KiB | 395.670 ms |

A segment-count A/B on 20,000 × 4 KiB confirmed that reducing segment concurrency is not a win in this environment:

- 4 segments: ~390 ms median-class performance
- 2 segments: ~647 ms median
- 1 segment: ~830 ms median

Therefore the existing four-segment publication strategy was retained.

## Bottleneck

Before this change, content q1 walked every content byte and performed, in the hot loop:

1. `start / block_size` to recover the block owner,
2. conversion/range checks for the owner,
3. a per-key stamp comparison,
4. a posting append on the first occurrence of the byte in the block.

For an 8 KiB block universe, q1 only needs to know whether each of the 256 possible byte values occurs at least once in each block. Recomputing the owner and consulting a stamp for every byte is unnecessary work.

## Implemented fast path

`search-core/src/vnext_fixed.rs` now builds content q1 block-by-block:

1. iterate `normalized_content.chunks(block_size)`,
2. compute the block owner once,
3. collect byte presence into a stack-local `[u64; 4]` 256-bit set,
4. enumerate set bits,
5. append the owner once to each present q1 key.

The output posting order remains ascending by owner, exactly as before. No on-disk format field or encoding rule changed.

## Correctness guards

A dedicated unit test was added:

`block_local_q1_bitset_is_byte_identical_to_stamp_reference`

It runs the optimized implementation and the previous stamp-based reference implementation over deterministic mixed/random data crossing many block boundaries and verifies:

- encoded q1 bytes are identical,
- q1 statistics are identical.

In addition, the full pre-change source was separately release-built and compared against the optimized source using the same 20,000 × 4 KiB benchmark input. Every durable file was hashed recursively.

Result:

`DURABLE_BYTE_IDENTITY=PASS`

This covers segment files, manifests, and `CURRENT`.

## Performance result

Post-change five-run medians:

| Payload / doc | Before | After | Improvement | Speedup |
|---:|---:|---:|---:|---:|
| 512 B | 99.507 ms | 86.415 ms | 13.16% | 1.15x |
| 1 KiB | 136.063 ms | 123.740 ms | 9.06% | 1.10x |
| 2 KiB | 251.817 ms | 197.390 ms | 21.61% | 1.28x |
| 4 KiB | 395.670 ms | 336.576 ms | 14.94% | 1.18x |

The generated store byte count remained unchanged for every corresponding benchmark case.

## Post-change profile, 20,000 × 4 KiB

A representative profiled run after the change showed:

- whole build: ~355 ms in the profiled run,
- per-segment `index_group_ms`: ~98–159 ms,
- per-segment content q1: ~43–53 ms under concurrent load,
- per-segment content q2: ~23–44 ms,
- per-segment content q3: ~29–57 ms,
- checksum: ~27–30 ms,
- durable write: ~80–91 ms, including filesystem sync.

The next CPU-side candidates are therefore content q2/q3. Durable checksum/write/fsync is also a material wall-time component, but it must not be weakened because the generation publication contract is durability-sensitive.

## Final validation

After implementation:

- `cargo fmt -- --check`: PASS
- `cargo clippy --offline --all-targets -- -D warnings`: PASS
- `cargo test --offline`: **152 / 152 PASS**
- `cargo build --offline --release`: PASS
- `cargo run --offline --release --bin pr_portable -- self-test`: `SELF_TEST_PASS`
- direct pre/post durable SHA-256 comparison: PASS

`bridge-core` offline compile was attempted but remains blocked by the pre-existing environment dependency issue:

`no matching package named 'ignore' found`

This is unrelated to the q1 change; the modified search-core itself is fully validated offline.

## Source-package rule

Starting with this package, `PersonalRag_DEV_ENV_BOOTSTRAP_2026-08-19.md` is included at the source root. Future source ZIPs should continue to include the current environment-bootstrap document so a new chat can recreate the measured Rust environment before optimization work begins.
