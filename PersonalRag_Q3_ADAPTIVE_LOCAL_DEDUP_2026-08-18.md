# PersonalRag q3 Adaptive Owner-Local Dedup — 2026-08-18

## Scope

This wave starts from `PersonalRag_GUI_PortableCore_Q3OnePassRadix_2026-08-18` and adds an adaptive owner-local deduplication fast path before the existing monotonic-owner one-pass q3 radix.

No `.prseg2` format, query semantics, generation semantics, or posting encoding changed.

## Motivation

The q3 sub-profiler showed that many packed `(q3 suffix, owner)` occurrences can disappear only after the global radix/dedup. Removing duplicates before the global shard radix can reduce temporary traffic, but unconditional owner-local hashing regresses ordinary code/text. The final implementation therefore gates the local hash path once per segment.

## Final design

1. Inspect at most 16 evenly spaced documents in the segment.
2. For each sampled document, measure one representative content block exactly with a reusable open-addressing q3 set.
3. Require at least 1,024 sampled q3 occurrences.
4. Enable owner-local dedup only when sampled duplicate ratio is at least 65%.
5. If disabled, use a dedicated direct emitter matching the previous hot loop.
6. If enabled, use a reusable open-addressing table at <=50% load for each block and emit each q3 once per owner.
7. Keep the existing global stable upper16 radix and final `dedup()` as the correctness backstop.

The serialized output is unchanged because the removed local duplicates are exactly the `(suffix, owner)` duplicates that the global sorted `dedup()` removed before this wave.

## Profiler extension

`PR_PROFILE_BUILD=1 PR_PROFILE_Q3=1` now also reports:

- `radix_occurrences`
- `local_saved`
- `local_blocks`
- `direct_blocks`
- `sample_occurrences`
- `sample_duplicates`

`occurrences` remains the logical pre-local-dedup q3 occurrence count.

## Release A/B

Method: baseline/candidate order alternated across 10 rounds to reduce thermal/cache ordering bias.

### Code-like 5k corpus

- sample duplicate ratio: 39.53% -> local dedup OFF
- content q3 median: 62.4485 ms -> 61.3490 ms
- q3 change: 1.76% faster
- radix occurrences unchanged: 5,998,238
- serialized `.prseg2`: byte-identical

This is the no-regression path: ordinary code/text remains on the direct emitter.

### Strongly repetitive 5k x 512-byte corpus

- sample duplicate ratio: 83.06% -> local dedup ON
- logical occurrences: 2,550,000
- radix occurrences: 435,739
- occurrences removed before radix: 2,114,261 (82.91%)
- content q3 median: 19.2445 ms -> 10.1845 ms
- content q3 reduction: 47.08%
- index-group median: 19.3935 ms -> 10.3375 ms
- segment total median: 48.0695 ms -> 38.8250 ms
- segment total reduction: 19.23%

## Correctness gates

Added/extended unit coverage verifies:

- adaptive high-repetition path is byte-equivalent to global-only q3 dedup
- sufficient high-entropy segment samples remain on the direct path
- small/low-repetition content remains direct
- local hash sentinel handling for q3 key 0 and 0xFFFFFF
- monotonic-owner one-pass radix equivalence remains intact
- q3 sub-profile occurrence accounting remains consistent

Final Search Core gate:

- 129/129 tests PASS
- `cargo fmt --check` PASS
- `cargo clippy --locked --offline --all-targets -- -D warnings` PASS
- release bin PASS
- release examples PASS
- release `pr_portable self-test` -> `SELF_TEST_PASS`

## Review notes

Two rejected prototypes are intentionally not retained:

1. per-block 32/64-point adaptive sampling: good on highly repetitive content but polluted the direct hot path
2. reusable full 24-bit bitmap: reduced radix traffic but caused cache/memory pressure and did not beat the final hash approach

The final segment-level gate is deliberately conservative: it captures the large win where duplicate density is extreme while avoiding hash work for normal code/text.
