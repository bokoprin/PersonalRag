# PersonalRag q2 monotonic-owner fast path

Date: 2026-08-18
Baseline: `PersonalRag_GUI_PortableCore_Q3BoundedParallelism_2026-08-18`

## Scope

This wave accelerates the vNext content-q2 fixed index without changing `.prseg2` format or query semantics.

### Adopted changes

1. q2 packed pairs are emitted in monotonically non-decreasing owner/block order.
2. Replace the old two-pass 16-bit LSD radix `(owner -> key)` with one stable upper-16/key pass.
3. The per-block q2 bitset already emits each `(key, owner)` at most once, so remove the redundant post-radix global `dedup()` pass.
4. Count populated q2 keys while building the radix histogram/prefix table, removing a second full-vector key-count scan.
5. Use 32-bit radix counters. Production vNext caps a segment at `u16::MAX` owner blocks; even the full 65,536 q2 keys per owner remains below `u32::MAX` total packed pairs.

No unsafe code was added. No on-disk format, checksum, durability, generation, or query-planner semantics changed.

## Rejected experiments

The following were implemented/measured during this wave and intentionally removed from the final source because they did not improve end-to-end throughput reliably:

- hierarchical outer-segment / inner-q3 CPU budget partitioning,
- asynchronous whole-file FNV hashing overlapped with writes,
- load-balanced section-checksum scheduling,
- reduced segment concurrency,
- q1 256-bit block-bitmap emitter (q1 kernel improved, but total segment time did not).

## A/B

CPU/I/O-isolated runs used `/dev/shm`, release binaries, alternating baseline/candidate order.

### 5,000 docs / one segment / 15 runs each

| metric | baseline median | candidate median | reduction |
|---|---:|---:|---:|
| content q2 | 57.025 ms | 47.232 ms | 17.17% |
| index group | 58.827 ms | 48.834 ms | 16.99% |
| segment total | 88.652 ms | 77.547 ms | 12.53% |
| build elapsed | 147.121 ms | 135.649 ms | 7.80% |
| write | 18.265 ms | 18.288 ms | -0.13% |

### 20,000 docs / four 5k segments / 11 runs each

| metric | baseline median | candidate median | reduction |
|---|---:|---:|---:|
| content q2 (per-segment samples) | 143.028 ms | 114.820 ms | 19.72% |
| index group (per-segment samples) | 178.498 ms | 168.746 ms | 5.46% |
| segment total (per-segment samples) | 217.983 ms | 209.243 ms | 4.01% |
| full build elapsed | 311.827 ms | 302.480 ms | 3.00% |

## Correctness

- Pre-change baseline regression: 133/133 PASS.
- Final regression: 135/135 PASS (2 q2-specific tests added).
- `cargo fmt --check`: PASS.
- `cargo clippy --locked --offline --all-targets -- -D warnings`: PASS.
- `cargo build --release --locked --offline --examples --bins`: PASS.
- release `pr_portable self-test`: `SELF_TEST_PASS`.
- Baseline/candidate durable store files (`CURRENT`, generation manifest, `.prseg2`) are byte-identical.

## Review

The final source differs from the baseline only in `search-core/src/vnext_fixed.rs` plus this report. The q2 optimization depends on two existing invariants: owner IDs are emitted in document/block order, and the q2 per-block bitset prevents duplicate keys for one owner. Both invariants are covered by dedicated tests and the existing persistent/segment/oracle suite.
