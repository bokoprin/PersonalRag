# Benchmark storage archival

Reason: `os error 112` interrupted Iteration 5 Candidate A before a measured result was produced. The C: volume had only 1.23 GB free while retained regenerated index outputs occupied 158.52 GB.

The following regenerated output directories were archived from the constrained C: volume to a separate local archive to permit the remaining paired measurement and final verification. Their profile-level raw logs, profile wrappers, summaries, SHA-256 tree manifests where produced, iteration reports, `state.json`, and `history.jsonl` remained in place under the local run artifact root.

- `profiles/iteration-3-best-a-attempt-1-invalidated-harness-restore/output/warm-prime`
- `profiles/iteration-3-best-a-attempt-1-invalidated-harness-restore/output/warm-measured`
- `profiles/iteration-3-candidate-a-invalidated-harness-restore/output/warm-prime`
- `profiles/iteration-3-candidate-a-invalidated-harness-restore/output/warm-measured`
- `profiles/iteration-1-candidate-a/output/warm-prime`
- `profiles/iteration-1-candidate-a/output/warm-measured`
- `profiles/iteration-4-candidate-a/output/warm-prime`
- `profiles/iteration-4-candidate-a/output/warm-measured`
- `profiles/iteration-5-candidate-a/output/warm-prime`
- `profiles/iteration-5-candidate-a/output/warm-measured`

These are deterministic, non-canonical benchmark index outputs. The archive preserves them without data loss, while the published metadata is sufficient for the recorded comparison. The interrupted Iteration 5 Candidate A attempt is invalid and was restarted from its fresh BEST A.

The original `profiles/iteration-5-best-a-attempt-1` was also archived before collecting the replacement fresh BEST A. Its measurement was valid in isolation, but no candidate measurement could be obtained near it because the disk-full interruption required archival work.
