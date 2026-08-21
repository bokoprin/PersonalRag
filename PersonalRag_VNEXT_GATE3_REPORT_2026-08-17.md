# PersonalRag Segment Format vNext Gate 3 Report

Date: 2026-08-17

## Scope

Gate 3 adds posting specialization to the Gate 2 block-level q3 prototype while keeping Perf12 production/oracle paths unchanged.

Implemented encodings:

- Singleton: local block ID stored inline in posting metadata; zero posting-section bytes.
- RawU16: strictly increasing u16 local block IDs.
- DenseBitmap: one bit per local block.

The format is advanced from `.prseg2` version 2 (`PRSEG2A2`) to version 3 (`PRSEG2A3`).

## Encoding policy

No arbitrary density threshold is used.

For each present q3 key:

1. cardinality == 1 -> Singleton.
2. Otherwise calculate `raw_bytes = cardinality * 2`.
3. Calculate `bitmap_bytes = ceil(segment_block_count / 8)`.
4. Use DenseBitmap only when `raw_bytes > bitmap_bytes`.
5. On equal encoded size, keep RawU16 to avoid bitmap scanning overhead.

This means the specialization decision is derived directly from encoded size.

## 20k posting distribution

Same deterministic text-heavy 20k corpus used by the Gate 2 benchmark:

- documents: 20,000
- blocks: 20,000
- source bytes: 24,054,644
- q3 keys: 34,801
- q3 posting IDs: 14,033,692
- active first-byte shards: 49

Gate 3 encoding distribution:

- Singleton keys: 433
- RawU16 keys: 33,476
- DenseBitmap keys: 892

The 20k segment bitmap is 2,500 bytes per dense posting. Therefore DenseBitmap becomes smaller than RawU16 above 1,250 block IDs.

## Correctness / fail-closed tests

Gate 3 vNext tests: 17/17 PASS.

Coverage includes:

- Gate 1 roundtrip/mmap/deterministic serialization/little-endian/fail-closed tests.
- Gate 2 block boundary ownership and two-byte look-ahead.
- No q3 generation across document boundaries.
- q3-to-naive-block-oracle equality including Japanese UTF-8 bytes.
- Singleton inline encoding and zero posting bytes.
- RawU16 selection when equal in size to DenseBitmap.
- DenseBitmap selection only when smaller.
- DenseBitmap iteration/get semantics.
- Unknown posting encoding rejected even after checksums are repaired.
- DenseBitmap cardinality corruption rejected even after checksums are repaired.

Full Search Core regression after Gate 3:

- existing unit: 5/5 PASS
- production: 35/35 PASS
- vNext: 17/17 PASS
- doc tests: PASS
- `cargo fmt --check`: PASS
- `cargo clippy --all-targets --locked --offline -- -D warnings`: PASS
- release build: PASS
- `SELF_TEST_PASS`: PASS

## Gate 2 vs Gate 3 A/B

To reduce CPU-load bias, the final build comparison used fresh Gate 2 and current Gate 3 release binaries and ran seven interleaved Gate2->Gate3 pairs on the same Linux environment and Rust 1.97.1 toolchain.

### Build elapsed_ms

Gate 2:

- 635.728
- 642.168
- 636.562
- 655.163
- 624.447
- 651.094
- 630.197

Median: **636.562 ms**

Gate 3:

- 570.183
- 530.890
- 548.092
- 571.089
- 563.415
- 563.931
- 567.822

Median: **563.931 ms**

Result:

- Gate 3 / Gate 2 speed ratio: about **1.129x**
- build elapsed reduction: about **11.4%**

## Size

Gate 2 total `.prseg2` bytes:

- 53,876,384

Gate 3 total `.prseg2` bytes:

- 43,417,320

Reduction: about **19.4%**.

Gate 2 q3 posting-section bytes:

- 28,067,384

Gate 3 q3 posting-section bytes:

- 17,329,908

Reduction: about **38.3%**.

The posting cardinality is unchanged at 14,033,692 IDs.

## Peak RSS sample

Same 20k benchmark process samples:

- Gate 2 median-like sample: 184,128 KiB
- Gate 3 median-like sample: 173,896 KiB

Reduction: about **5.6%**.

RSS is environment-sensitive and is supporting evidence rather than a hard performance claim.

## Important interpretation

Gate 3 is an improvement over Gate 2 for the current block-q3 prototype, but this is not yet a production-switch result.

The prototype still lacks the full Gate 4 query surface:

- q1/q2 short-query path
- rarest-trigram query planning
- exact block verification
- filename/path query path
- full Perf12/vNext/naive query oracle comparison

Therefore no claim is made yet that vNext as a complete search engine is faster than Perf12.

## Next gate

Gate 4: Query prototype.

Required core work:

- q1/q2 short queries
- q3+ candidate generation
- rarest trigram anchor
- exact block/document verification
- filename/path semantics
- `Perf12 result == vNext result == naive substring oracle`
