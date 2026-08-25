# Codex Task: CQ3DIR Read-Only Capacity Analysis

## Goal

Run the ChatGPT-authored read-only CQ3DIR analyzer against the existing valid PersonalRag `warm-measured` index and publish the resulting machine-readable JSON plus a concise Markdown execution report to this same branch.

This task is analysis-only. Do not modify production Rust code, benchmark configuration, index contents, or the source corpus.

## Branch

Work on the existing branch:

`perf/autopilot-5round-20260823-192255`

Do not merge to `main`.

## Analyzer

Use exactly this repository script as the starting point:

`scripts/analyze-cq3dir-readonly.ps1`

The script is intended to:

- open `seg-*.prseg` with read access only,
- parse PRSEG005 section descriptors,
- read CQ3DIR/CQ3POST metadata without modifying the index,
- validate Prefix10 shape/order/offset monotonicity,
- measure actual count/key-gap/offset distributions,
- simulate theoretical CQ3DIR representations,
- write only the requested JSON output outside the index directory.

Before executing, inspect the script for obvious execution errors. Do not redesign its analysis model. If a small execution bug prevents running it, make the minimum repair needed and explain the repair in the report. Do not change production code.

## Existing index to analyze

Use this already-generated valid index; do not generate a new index unless this exact path no longer exists:

`C:\Users\bokop\AppData\Local\PersonalRag\perf-autopilot\autopilot-20260823-203333\profiles\iteration-2-candidate-b\output\warm-measured`

This is the accepted Iteration 2 candidate B used by the prior index-size analysis.

Expected identity/context from the previous audit:

- format: `PRSEG005`
- segment count: 14
- index total: about 5.34 GiB
- CQ3DIR total: 1,027,053,392 bytes
- CQ3DIR entries: about 102.7 million
- canonical tree SHA-256: `7389a6cf73fa99b425d1cb3b9177e13e1934f9b54752a00d7ef9f74bae22fcb8`

If the path is missing, stop with `CQ3DIR_SOURCE_INDEX_MISSING`; do not silently benchmark or build a replacement index.

## Execution

Create a timestamped output JSON under the repository report directory, for example:

`reports/cq3dir-analysis-YYYYMMDD-HHMMSS.json`

Run from the repository root with Windows PowerShell, equivalent to:

```powershell
powershell -ExecutionPolicy Bypass `
  -File .\scripts\analyze-cq3dir-readonly.ps1 `
  -IndexPath "C:\Users\bokop\AppData\Local\PersonalRag\perf-autopilot\autopilot-20260823-203333\profiles\iteration-2-candidate-b\output\warm-measured" `
  -OutputPath ".\reports\cq3dir-analysis-YYYYMMDD-HHMMSS.json"
```

Capture the terminal summary produced after `CQ3DIR_ANALYSIS_COMPLETE`.

## Required checks

After execution verify:

1. The source index tree was not modified.
2. No files were created beneath the source index directory.
3. No production Rust source changed.
4. The output JSON parses successfully.
5. `segmentCount` is 14 unless the existing source differs; if it differs, explain and stop before drawing conclusions.
6. `cq3DirBytes` is consistent with the prior 1,027,053,392-byte audit unless the source differs.
7. Candidate size calculations contain no negative sizes except a deliberately non-applicable candidate represented as null/`Applicable=false`.
8. All reported whole-index sizes equal `indexBytes - cq3DirBytes + candidateDirectoryBytes` for applicable candidates.

## Markdown report

Create a matching report:

`reports/cq3dir-analysis-YYYYMMDD-HHMMSS.md`

The report should be factual and compact. Include:

### Source

- branch / HEAD
- index path
- analyzer path
- indexBytes
- segmentCount
- entries
- cq3DirBytes
- cq3DirGiB
- cq3DirPercentOfIndex

### Actual distributions

- encoding counts and percentages for InlineU32 / DeltaVarint / Block256Bitmap / DenseBitset
- max posting count
- percentage of entries whose count fits 8 / 12 / 14 / 16 bits where derivable from the histogram
- key-gap varint byte histogram
- packed(count+encoding) varint byte histogram
- payload-offset delta varint byte histogram
- per-segment countBits / packedBits / offsetBits

### Theoretical candidate comparison

For every applicable entry in `Candidates`, show:

| Candidate | CQ3DIR bytes | CQ3DIR reduction | Whole index GiB | Whole index reduction | Lookup model |

Sort by CQ3DIR bytes ascending, but clearly distinguish:

- pure sizing lower bounds,
- variable-length models with sequential decode,
- blocked random-lookup-capable models,
- bitmap/rank models,
- the current Prefix10 representation.

Do not recommend the numerically smallest format solely because it is smallest.

### Practical shortlist for ChatGPT

Select at most 3 candidates that deserve further analysis under the product goal of a DocFetcher-like desktop search application where all of these matter:

- compact on-disk index,
- fast interactive search,
- reasonable build cost,
- deterministic/fail-closed behavior,
- compatibility/migration risk.

For each shortlisted candidate state only:

- estimated size reduction,
- expected lookup/decode complexity,
- major risk or unknown.

Do not implement any format change.

## Git rules

Allowed changes for this task:

- `scripts/analyze-cq3dir-readonly.ps1` only if a minimal execution repair is actually required,
- `reports/cq3dir-analysis-*.json`,
- `reports/cq3dir-analysis-*.md`.

Do not modify application/production Rust code.

After checks pass:

1. run `git status --short`,
2. stage only the allowed files,
3. commit with a Japanese one-line message such as `CQ3DIRの実データ分布と理論圧縮率をread-only解析`,
4. push the current branch to GitHub.

## Final response

Finish with exactly one compact summary block containing:

```text
CQ3DIR_READONLY_ANALYSIS_COMPLETE
branch=...
commit=...
indexGiB=...
cq3DirGiB=...
entries=...
maxCount=...
smallestModeledCandidate=...
smallestModeledDirectoryReductionPercent=...
recommendedCandidates=...
jsonReport=...
markdownReport=...
pushed=true
```

Do not ask the user to manually paste the JSON back into ChatGPT. The JSON and Markdown report on GitHub are the handoff artifacts; ChatGPT will read them directly from this branch.
