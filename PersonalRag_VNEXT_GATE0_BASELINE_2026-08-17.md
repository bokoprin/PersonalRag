# PersonalRag Segment Format vNext - Gate 0 Baseline

Date: 2026-08-17
Environment: ChatGPT Linux x86_64 / Rust 1.97.1
Source oracle SHA-256: `6c646b04fcd97c4870393d6eefa362eb9656952a29606fc16504ee2a6a3a4ba3`

## Correctness baseline

- Search Core unit tests: 5/5 PASS
- Search Core production tests: 35/35 PASS
- Doc tests: PASS
- Clippy `-D warnings`: PASS
- Release `pr_portable`: PASS
- `SELF_TEST_PASS`: PASS

## Build baseline

### filename-only 100k

Command:

```bash
./target/release/examples/name_only_bench 100000 <OUT> 50000 4
```

Result:

```text
NAME_ONLY_BENCH docs=100000 segments=2 elapsed_ms=141.810 index_bytes=58777904
peak_rss_kb=202212
```

### text-heavy 20k / Perf12 Unified Full

Command:

```bash
./target/release/examples/unified_full_bench unified 20000 <OUT> text
```

Initial baseline:

```text
mode=unified docs=20000 elapsed_ms=1016.886 bytes=76733278
peak_rss_kb=294776
```

Post-Gate1 production-path recheck:

```text
mode=unified docs=20000 elapsed_ms=922.102 bytes=76733278
peak_rss_kb=300796
```

The elapsed/RSS variation is treated as run-to-run/environment noise. Index bytes are identical and Gate1 is not connected to the production build/query path.

## Query baseline - text-heavy 20k

30 rounds / 4 workers:

```text
AUTO_QUERY query=timeout hits=19672 rounds=30 workers=4 p50_ms=0.139735 p95_ms=0.205483
AUTO_QUERY query=unique_marker_970 hits=2 rounds=30 workers=4 p50_ms=0.007756 p95_ms=0.008334
AUTO_QUERY query=zzzz_no_such_marker_20260817 hits=0 rounds=30 workers=4 p50_ms=0.008101 p95_ms=0.008797
```

Post-Gate1 recheck:

```text
AUTO_QUERY query=timeout hits=19672 rounds=30 workers=4 p50_ms=0.157280 p95_ms=0.208064
AUTO_QUERY query=unique_marker_970 hits=2 rounds=30 workers=4 p50_ms=0.007465 p95_ms=0.008207
AUTO_QUERY query=zzzz_no_such_marker_20260817 hits=0 rounds=30 workers=4 p50_ms=0.007941 p95_ms=0.008674
```

## Gate 1 status

Gate 1 adds a parallel `.prseg2` prototype only. Perf12 remains production/correctness oracle.

Implemented:

- fixed little-endian header
- section directory
- document SoA
- path blob
- 8 KiB default block table
- uncompressed normalized content blob
- section checksums
- whole-file footer checksum
- mmap reader
- deterministic serialization
- fail-closed structural validation

Dedicated Gate1 tests: 7/7 PASS.
