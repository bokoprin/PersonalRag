# PersonalRag performance autopilot final report

## Result

Initial baseline score: 31,601.690 ms
Final best score: 53,719.135 ms
Overall improvement: -69.988%
Accepted: 1
Rejected: 4
Best commit: fec46b51e7c1820304fcd2aed7178b2ce17b635c

## Measurement methodology

- Frozen benchmark config: `hydrationWorkers=4`, `buildWorkers=4`, `segmentDocs=5000`, `maxFileBytes=33554432`, `hydrationBatchBytes=134217728`, `scannerMode=auto`, `accelerationProfile=balanced`.
- Warm-prime then warm-measured was used for every sample; their SHA-256 trees had to match before the sample was eligible.
- Each iteration used fresh BEST A, CANDIDATE A, CANDIDATE B, then fresh BEST B from a detached current-best worktree.
- The primary score is callWallMs + verifyWallMs; the two-sample representative is the unrounded arithmetic mean (A + B) / 2.
- ACCEPT threshold: candidate representative at least 3% lower than current-best and both individual pairs must win.
- sourceFiles, processedFiles, indexedFiles, bytesRead, relative paths, file sizes, and SHA-256 output trees were checked on every comparison.
- Profiling instrumentation was enabled only for the `PR_PROFILE_BUILD` profile executable. It reports the unchanged filesystem primitive as `combined_read` rather than inventing separate open/read/allocation timings; normal GUI/release execution does not retain the collector.

## Measurement anomalies

- Iteration 3 `INVALIDATED_HARNESS_RESTORE`: a 37,084.457 ms Candidate A was discarded after Windows Git retained stale worktree stat data following an explicit restore. The content blob and diff matched current-best; the harness was repaired to really-refresh only declared paths before judging rollback cleanliness.
- Iteration 5 `INVALID_ENVIRONMENT`: Candidate A produced no warm-measured result because C: reported `os error 112` (disk full). The incomplete evidence was preserved, fresh BEST A was recollected, and the candidate was restarted from that new paired baseline.
- Finalization setup: a fresh detached best worktree has no Git-ignored `frontend/dist`; Tauri's generated-context macro therefore failed before the frontend build. `npm ci` and `npm run build` were run inside that worktree, then the complete final gate was rerun successfully. No tracked application source was changed for this setup step.
- I/O/cache variation was substantial: Iteration 5 Candidate A warm-prime hydration was 268,532.308 ms and final warm-prime hydration was 286,241.508 ms, while their subsequent warm-measured hydration values were 6,820.487 ms and 1,963.865 ms respectively. These prime values are not used as primary scores.
- Corpus consistency held for all valid samples: `sourceFiles=66259`, `processedFiles=66259`, `indexedFiles=66259`, and `bytesRead=3049848764`. No corpus-change stop was triggered.

## Storage archival exception

The local-only output rule was temporarily relaxed after C: fell to 1.23 GB free and Iteration 5 hit `os error 112`. No benchmark evidence was deleted: noncanonical regenerated index-body directories were moved, with file-count and byte-count verification, to a separate local archive. Local state, reports, raw logs, wrappers, summaries, and SHA-256 tree manifests remained under the run directory. The exact inventory is recorded in [storage-archival.md](storage-archival.md).

## Iteration decisions

- Iteration 1: REJECT — REJECT_PRIMARY_SCORE_A=-23.966% (< 2.5% clear-reject boundary)
- Iteration 2: ACCEPT — Both fresh pairs won, the arithmetic representative improved by at least 3%, and every required gate passed.
- Iteration 3: REJECT — REJECT_PRIMARY_SCORE_A=-3.558% (< 2.5% clear-reject boundary)
- Iteration 4: REJECT — REJECT_PRIMARY_SCORE_A=-55.043% (< 2.5% clear-reject boundary)
- Iteration 5: REJECT — REJECT_PAIRED: representative=9.048% pairAWin=True pairBWin=False
