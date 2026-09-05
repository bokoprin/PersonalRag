# Filename/Path Search Bake-off Decision

- Frozen official corpus: `1,000,000` records, `107374182400` logical bytes, seed `0x505241475F314D31`.
- Corpus manifest SHA-256: `ad8c7abcd08916a39e588781e5bd478b1a834a75fa47ce59111d7f4a30398bd7`.
- Records SHA-256: `4d14896b236bffdf5a0f07b248e42720944c056a67121fceaa93a965400fa9e0`.
- Query set SHA-256: `c49032724de077c27334d70d984d8daea39bcaaa323b79162edbd1f0beb208dc`.
- Timing: 20 rounds, first 2 warmup, 18 measured rounds; all queries and oracle output were shared by A/B/C.

## Fixed hard limits and score

The fixed score is lower-is-better and uses measured value divided by the hard limit. Weights are: query-class p95 35%, global p99 20%, ready private memory 15%, persistent bytes 15%, load 5%, build 5%, and update p95 5%. A route with any hard-gate failure is excluded before scoring.

| Route | FP/FN | p50 / p95 / p99 (ms) | Worst class p95 (ms) | Persistent | Ready private | Build (s) | Load (s) | Update p95 (ms) | Gate | Score |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|---:|
| A | 0/0 | 82.673 / 133.124 / 305.189 | 325.086 | 442965507 | 1806721024 | 2.724 | 2.858 | 0.0036 | **FAIL** | excluded |
| B | 0/0 | 8.005 / 33.393 / 59.389 | 183.600 | 417279961 | 939847680 | 14.222 | 2.226 | 0.0038 | **FAIL** | excluded |
| C | 0/0 | 6.585 / 35.882 / 43.392 | 49.687 | 477165162 | 552587264 | 26.247 | 1.486 | 0.0036 | **PASS** | 0.649858 |

## Decision

Route C (`29870c91322675b2d3232225d05528af7dff2f88`) is the Champion. It is the only route that passes every official hard gate: exact oracle agreement, latency limits including every query class, persistent size, ready memory, build, fresh load, and update p95. Route A fails latency and ready-memory gates. Route B fails fresh-load and wildcard/path-wildcard class latency gates. The raw reports are retained under the ignored `bench-data/official-20260905/reports` directory; their SHA-256 values and all measured fields are copied into `bench/BAKEOFF_RESULT.json`.

No official result was used to tune A or B. Champion-only product work starts from this frozen decision.