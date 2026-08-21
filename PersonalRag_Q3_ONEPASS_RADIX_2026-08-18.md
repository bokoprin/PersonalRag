# PersonalRag q3 Sub-Profiler + Monotonic-Owner 1-Pass Radix

Date: 2026-08-18

## Scope

This wave starts from `PersonalRag_GUI_PortableCore_ThreeFastPaths_Profiled_2026-08-18.zip` (SHA-256 `2354d5e26a4265fcf00be740c1df948428e1295fd13066c83f21fc71031a2d0e`) and changes only the vNext q3 build path plus profiling documentation.

No `.prseg2` format, query semantics, posting encoding, generation semantics, or durability semantics are changed.

## Design

A packed q3 occurrence is `upper16=suffix`, `lower16=owner` (content block ID or path document ID). Both emitters traverse owners in monotonically non-decreasing order inside every first-byte shard.

The previous implementation used two stable 16-bit radix passes: lower16 owner first, then upper16 suffix. Because owner order is already monotonic before sorting, a single stable upper16 pass preserves owner order within each suffix and produces the same `(suffix, owner)` ordering as the old two-pass radix.

The resulting sorted/deduplicated pairs and serialized segment bytes are therefore unchanged while one complete radix pass is removed.

## q3 sub-profiler

`PR_PROFILE_BUILD=1` keeps the existing build-stage profiler.

Add `PR_PROFILE_Q3=1` to opt into q3-specific measurements:

- emitted occurrence count
- unique `(suffix, owner)` pair count
- emit time
- radix scratch preparation time
- radix count time
- radix prefix time
- radix scatter time
- dedup time
- dictionary/posting encode time

The detailed timers are disabled unless both environment variables are present, so ordinary production builds and the normal build profiler do not pay the extra timing overhead.

## Correctness gates

Pre-change Search Core regression: 122/122 PASS.

New tests:

1. monotonic-owner upper16 radix matches a full packed-pair sort.
2. duplicates remain adjacent and dedup output matches the full-sort oracle.
3. q3 sub-profiler occurrence/unique-pair accounting is correct on a deterministic fixture.

Final Search Core regression: 125/125 PASS.

Additional gates:

- `cargo fmt --check`: PASS
- `cargo clippy --locked --offline --all-targets -- -D warnings`: PASS
- `cargo build --release --locked --offline --examples --bins`: PASS
- release `pr_portable self-test`: `SELF_TEST_PASS`
- existing vNext segment tests: 17/17 PASS

## Byte identity

A deterministic 5k-document q3 benchmark segment was generated before and after the change.

Both files have SHA-256:

`5cb9a9ca8df364b51ced6c6353f56d1b168896aaa434b06036d97f6d1069e43d`

`cmp` reports byte identity.

The same byte identity was also confirmed for representative release A/B output.

## Release A/B

Workload: deterministic `vnext_q3_bench`, 5,000 documents, about 6.0 MB normalized content. Baseline and candidate binaries were run alternately for 9 rounds. `PR_PROFILE_BUILD=1` was enabled and q3 sub-profiling was disabled for this comparison.

| Metric | Two-pass baseline median | 1-pass median | Improvement |
|---|---:|---:|---:|
| content q3 | 70.014 ms | 65.145 ms | 6.95% lower / 1.075x |
| path q3 | 5.910 ms | 4.593 ms | 22.28% lower / 1.287x |
| concurrent index group | 70.434 ms | 65.737 ms | 6.67% lower / 1.071x |
| segment total | 148.281 ms | 141.560 ms | 4.53% lower / 1.047x |
| benchmark wall | 148.907 ms | 142.169 ms | 4.52% lower / 1.047x |

Write timing varied independently (69.239 ms baseline median vs 71.434 ms candidate median), so the q3/index-group reductions are the more direct signal for this wave.

## Final release q3 sub-profile

7 runs with `PR_PROFILE_BUILD=1 PR_PROFILE_Q3=1` on the same 5k workload:

- occurrences: 5,998,238
- unique pairs: 3,504,322
- duplicate-pair reduction after sort: 41.58%
- emit median: 22.929 ms
- radix prepare median: 3.349 ms
- radix count median: 4.829 ms
- radix prefix median: 0.928 ms
- radix scatter median: 7.429 ms
- radix component sum: about 16.535 ms
- dedup median: 9.571 ms
- encode median: 7.255 ms
- content q3 median with detailed profiling enabled: 56.568 ms

The 41.58% duplicate rate provides direct evidence for evaluating adaptive block-local q3 dedup in a later wave, but that optimization is deliberately not included here.

## Review

One review/fix loop was required: Clippy found test-module placement and an `Option<&mut T>` style warning. Both were fixed, targeted tests were repeated, and the full regression/release gates were rerun.

Final review found no additional correctness, format, test, or profiling-gating issue in this wave.
