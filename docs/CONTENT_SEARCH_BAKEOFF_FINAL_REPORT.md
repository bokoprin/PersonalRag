# Content Search Bakeoff Formal Report

- Series: `content-bakeoff-93c82c0ffd73`
- Frozen source: `93c82c0ffd73fb29986bdeae0aead345e3b5af7f`
- Gate 0: PASS (restore, Release build, smoke across all four backends)
- Formal status: BLOCKED_INCOMPLETE_FORMAL_SERIES

The real filesystem corpus and independent oracle were generated. The formal lock fixed 20 rounds, 2 warmup rounds, 18 measured rounds, seed 123456, block size 65536, and overlap 256. The first isolated `scan` child was started against 500,000 source/config files and remained active for roughly 18 minutes without producing its report. It was stopped to avoid an unbounded interactive run.

No unmeasured backend or corpus is presented as PASS, REJECTED, or an allowed NOT_RUN. The summary therefore has `evaluation_complete=false` and `pass=false`. Resume with the exact frozen lock; do not alter the corpus, query set, oracle, thresholds, or round definition.

## Corpus evidence

| corpus | files | content bytes |
|---|---:|---:|
| SOURCE_CONFIG | 500000 | 5502926848 |
| LOG | 20000 | 21609054208 |
| HUGE | 20 | 10871635968 |

## Gate 0 evidence

See `logs/content-bakeoff-gate0-final/REGRESSION.json` for command, exit code, elapsed time, and log paths.

## Formal run evidence

- `reports/content-search-bakeoff/FORMAL_BAKEOFF_LOCK.json` records the frozen source, remote, corpus manifest, query, oracle, and thresholds.
- `reports/content-search-bakeoff/FORMAL_BAKEOFF_INVALIDATIONS.json` records the incomplete child run and exact resume condition.
- No performance value is inferred from the stopped process.
