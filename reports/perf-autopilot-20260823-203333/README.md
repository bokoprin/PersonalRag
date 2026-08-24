# PersonalRag performance autopilot report bundle

This directory publishes the human-readable evidence from the five-round performance autopilot run `autopilot-20260823-203333`.

## Result at a glance

- Completed learning loops: 5
- Accepted candidates: 1
- Rejected candidates: 4
- Current-best commit: `fec46b51e7c1820304fcd2aed7178b2ce17b635c`
- Accepted performance commit: `ca23e39205b0718130d0e06e520f68e4eab49c36`
- Canonical output-tree SHA-256: `7389a6cf73fa99b425d1cb3b9177e13e1934f9b54752a00d7ef9f74bae22fcb8`

The accepted candidate in iteration 2 improved the adjacent paired representative by 4.192% while preserving environment values and byte-identical output. The final end-of-run measurement was slower than the initial baseline because the machine exhibited large Windows storage/cache variation. It is retained as an observation, not treated as a causal code-regression verdict.

## Reading order

1. [final-report.md](final-report.md) — methodology, anomalies, aggregate result, and decisions.
2. [run-summary.json](run-summary.json) — compact machine-readable summary for tools and assistants.
3. [iteration-1.md](iteration-1.md) through [iteration-5.md](iteration-5.md) — fresh paired comparisons and learning for each loop.
4. [harness-self-test.md](harness-self-test.md) — bootstrap harness self-test result.
5. [frozen-benchmark-config.json](frozen-benchmark-config.json) — fixed benchmark configuration used in all five loops.
6. [storage-archival.md](storage-archival.md) — non-lossy archival record for the disk-full interruption.

## Published scope

This bundle contains report text and compact metadata only. It intentionally excludes regenerated index bodies, raw profile logs, executables, local worktrees, and the large local `state.json` / `history.jsonl` files. No secret-value patterns were found in the published source material before staging.
