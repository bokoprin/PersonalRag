# PersonalRag Rolling Progress / Performance Pass 4

Date: 2026-08-15

## Scope

This pass starts from Safe Ingestion / Performance Pass 3. It keeps App Contract v1 and the portable on-disk index format unchanged.

Goals:

1. Show current processing speed as a rolling 10-second rate instead of a lifetime average.
2. Estimate remaining time from roughly the latest 30 seconds instead of the entire rebuild history.
3. Confirm the current ingestion/build bottleneck with measurements.
4. Apply only a small, byte-compatible optimization that measurably improves that bottleneck.

## Rolling progress

A small `ProgressRateTracker` was added to bridge-core and reused by the Tauri adapter.

- `files_per_second`: latest 10-second window
- `mib_per_second`: latest 10-second window
- `eta_ms`: latest 30-second file-rate window
- scanning keeps ETA unknown because total files are not known yet
- finalization phases do not invent a file-based ETA
- the frontend no longer falls back to `processed_files / total_elapsed`, so old fast sections no longer dominate the displayed ETA

The tracker keeps only a short deque of counter samples; it does not retain full rebuild history.

## Bottleneck confirmation

A 100,000-document filename-only benchmark was profiled because this matches large trees containing many images/binaries after Safe Ingestion skips their bodies.

Baseline accumulated segment build phase time:

- name gram generation: 139.508 ms (32.4%)
- name posting construction: 273.870 ms (63.6%)
- total profiled segment phase time: 430.686 ms

The dominant CPU cost was therefore the q2/q3 name-posting comparison sort, not content indexing.

## Optimization

For normal segment sizes (`doc_count <= 65535`), name posting construction now packs local doc IDs tightly and uses fixed-width radix passes:

- q2: compact `u32`, two 16-bit radix passes
- q3: compact `u64` (`24-bit key + 16-bit id`), three 16-bit radix passes
- larger-than-65535 segments retain the old comparison-sort fallback

A direct equivalence unit test compares every q1/q2/q3 offset/posting/directory vector between the radix path and the old comparison path.

The generated index was also compared at the file level against Performance Pass 3 and was byte-identical.

### A/B

100,000 filename-only documents, 5 runs each:

- baseline median: 296.794 ms
- optimized median: 277.423 ms
- reduction: 6.53%
- speedup: 1.070x
- index bytes: 73,245,376 in both cases

Profile after optimization:

- name posting: 133.233 ms (down 51.35%)
- total profiled segment phase time: 280.491 ms

A proposed `sort_unstable` change for the scanner-to-build path order was explicitly rejected: on 500,000 paths it was materially slower than the existing stable sort.

## Refactoring

The moving-rate logic was isolated in `bridge-core/src/progress_rate.rs` rather than embedding more stateful timing code directly in `src-tauri/main.rs`.

This keeps Tauri as an adapter and makes the rate policy directly unit-testable without changing App Contract v1.

## Regression

- Search Core unit: 4/4 PASS
- Search Core production: 30/30 PASS
- Search Core release: PASS
- SELF_TEST_PASS
- Bridge unit: 4/4 PASS
- Contract v1: 3/3 PASS
- Bridge integration: 6/6 PASS + 1 large scanner stress PASS when explicitly enabled
- Frontend: 16/16 PASS
- Frontend production build: PASS
- Windows GNU Search Core check/clippy: PASS
- Windows GNU Bridge check/clippy: PASS
- Windows GNU Tauri check/clippy: PASS
- Windows GNU Tauri release link: PASS
- generated executable: PE32+ Windows GUI x86-64

## Status

Implementation: 100%
Regression: 100%
Windows compile/link gate: 100%
Remaining real acceptance: run this package on the same large Windows root and observe that speed follows the current workload over ~10 seconds and ETA stabilizes over ~30 seconds.
