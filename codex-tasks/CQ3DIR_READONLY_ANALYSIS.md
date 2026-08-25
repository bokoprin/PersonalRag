# Codex Task: CQ3DIR Read-Only Capacity Analysis

## Goal

Run the ChatGPT-authored read-only CQ3DIR analyzer against one valid PersonalRag `warm-measured` index and publish the resulting machine-readable JSON plus a concise Markdown execution report to this same branch.

The preferred source is an already-existing valid index. If no suitable existing index remains, autonomously generate exactly one temporary Warm profile (`warm-prime` + `warm-measured`) using the frozen production-equivalent benchmark configuration, analyze its `warm-measured` output, persist the compact analysis artifacts, and then safely delete only the temporary index bodies created by this task.

This task is analysis-only. Do not modify production Rust code, production behavior, benchmark semantics, the source corpus, or any pre-existing index.

## Branch

Work on the existing branch:

`perf/autopilot-5round-20260823-192255`

First update it to the latest remote HEAD and confirm the local branch matches `origin/perf/autopilot-5round-20260823-192255`.

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

## Known prior audit context

The previous valid audit used:

`C:\Users\bokop\AppData\Local\PersonalRag\perf-autopilot\autopilot-20260823-203333\profiles\iteration-2-candidate-b\output\warm-measured`

That specific index may have been intentionally deleted to reclaim disk space. Its absence is not an error and must not by itself stop this task.

Prior reference values:

- format: `PRSEG005`
- segment count: 14
- index total: about 5.34 GiB
- CQ3DIR total: 1,027,053,392 bytes
- CQ3DIR entries: about 102.7 million
- canonical tree SHA-256: `7389a6cf73fa99b425d1cb3b9177e13e1934f9b54752a00d7ef9f74bae22fcb8`
- prior sourceFiles / processedFiles / indexedFiles: `66259 / 66259 / 66259`
- prior bytesRead: `3049848764`

These are reference values, not hard-coded truth for a newly generated current corpus. If `C:\Program Files` has legitimately changed, record the difference and continue with the new valid corpus rather than pretending it matches the old one.

## Frozen benchmark configuration

Read and use the repository artifact:

`reports/perf-autopilot-20260823-203333/frozen-benchmark-config.json`

Expected values from the prior run are:

- hydrationWorkers = 4
- buildWorkers = 4
- segmentDocs = 5000
- maxFileBytes = 33554432
- hydrationBatchBytes = 134217728
- scannerMode = auto
- accelerationProfile = balanced

Do not silently recompute or retune these values for the temporary analysis build. Freeze and pass them explicitly according to the current repository profiler interface.

If the profiler wrapper cannot express every frozen field explicitly, inspect `scripts/profile-index-build.ps1` and `bridge-core/examples/index_build_profile.rs` and invoke the existing profiler path that accepts all concrete frozen values. Do not change production defaults merely to run this task.

## Source index selection: automatic fallback chain

Use the following fallback chain without asking the user to restore or paste files.

### Step A — try the prior exact path

If the prior `iteration-2-candidate-b/output/warm-measured` directory still exists and validates, use it.

### Step B — search existing local artifacts

If Step A is missing, search existing PersonalRag performance-analysis locations for a complete `warm-measured` index, including the existing Autopilot artifact root and any known local archive locations referenced by the repository reports.

A candidate existing index is usable only if all of the following hold:

1. it contains a complete PRSEG005 generation and manifest,
2. its profile/run metadata identifies it as a completed `warm-measured` result rather than `warm-prime`, interrupted, invalidated, or disk-full output,
3. `verify_index` or equivalent existing repository verification succeeds,
4. it has no obvious temporary/incomplete files,
5. the analyzer can validate its CQ3DIR Prefix10 shape,
6. its source/profile metadata can be associated with the frozen benchmark configuration above, or any deviation is explicitly understood and documented.

Prefer, in order:

1. exact prior accepted candidate output,
2. another valid output for the same current-best/accepted production code and frozen config,
3. another valid canonical output from the same Autopilot run.

Do not use an arbitrary index merely because it exists.

Do not delete, move, rename, or mutate any existing index selected in Step A or B.

### Step C — generate one temporary analysis index if none exists

If no suitable existing index remains, generate one temporary Warm profile under a dedicated task-owned directory outside the repository, for example:

`$env:LOCALAPPDATA\PersonalRag\cq3dir-readonly\<run-id>\`

The task-owned root must be newly created for this task and recorded before any generation begins.

Use:

- source root: `C:\Program Files`
- the frozen benchmark configuration above,
- current accepted/current-best production code on this branch,
- existing production-equivalent profiling path,
- Warm mode, which must perform exactly one `warm-prime` followed by one `warm-measured`.

Do not generate additional BEST/CANDIDATE pairs. Do not run the five-round Autopilot. This fallback exists only to obtain one valid `warm-measured` index for CQ3DIR analysis.

Before generation, check free space on the destination volume. The prior complete index was about 5.34 GiB and Warm temporarily requires both `warm-prime` and `warm-measured`, plus build overhead. Require a conservative minimum of 20 GiB free before starting. If less than 20 GiB is available, stop with:

`CQ3DIR_INSUFFICIENT_TEMP_SPACE`

Do not delete unrelated files to make space.

## Temporary generated-index validation

If Step C is used, do not analyze the generated output until all of the following pass:

1. Warm profile completes successfully.
2. `warm-measured` exists and is complete.
3. existing `verify_index` or equivalent verification passes.
4. `warm-prime` and `warm-measured` output trees are byte-identical if the current profiling harness already produces tree manifests; otherwise compute a read-only relative-path + size + SHA-256 tree for both and require equality.
5. profile JSON parses successfully.
6. sourceFiles, processedFiles, indexedFiles, and bytesRead are recorded.
7. frozen configuration in the measured profile equals the repository frozen benchmark configuration.

If the current `C:\Program Files` corpus differs from the 2026-08-23 reference counts, record `CORPUS_CHANGED_SINCE_PRIOR_AUDIT` in the Markdown report, but continue if the new Warm pair is internally consistent and valid.

If validation fails, do not analyze the incomplete output. Preserve compact logs/metadata needed to explain the failure, delete only task-owned incomplete generated index bodies if safe, and stop with a specific reason.

## CQ3DIR analyzer execution

Create a timestamped output JSON under the repository report directory:

`reports/cq3dir-analysis-YYYYMMDD-HHMMSS.json`

Run the analyzer against the selected or generated `warm-measured` index from the repository root, equivalent to:

```powershell
powershell -ExecutionPolicy Bypass `
  -File .\scripts\analyze-cq3dir-readonly.ps1 `
  -IndexPath "<selected-warm-measured-index>" `
  -OutputPath ".\reports\cq3dir-analysis-YYYYMMDD-HHMMSS.json"
```

Capture the terminal summary produced after `CQ3DIR_ANALYSIS_COMPLETE`.

## Required checks after analysis

Verify:

1. The analyzed source index tree was not modified by the analyzer.
2. No files were created beneath the analyzed source index directory by the analyzer.
3. No production Rust source changed.
4. The output JSON parses successfully.
5. `segmentCount > 0`.
6. `cq3DirBytes > 0` and `entries > 0`.
7. Candidate size calculations contain no negative sizes except a deliberately non-applicable candidate represented as null/`Applicable=false`.
8. All reported whole-index sizes equal `indexBytes - cq3DirBytes + candidateDirectoryBytes` for applicable candidates.
9. If the analyzed index matches the prior canonical source, compare its CQ3DIR totals with the prior `1,027,053,392` bytes / ~102.7M-entry audit and explain any mismatch.
10. If a newly generated current corpus differs from the old reference, do not fail merely because segment count or CQ3DIR size changed; report the new actual values.

## Markdown report

Create a matching report:

`reports/cq3dir-analysis-YYYYMMDD-HHMMSS.md`

The report should be factual and compact. Include:

### Source

- branch / HEAD
- source selection method: `existing-exact`, `existing-fallback`, or `generated-temporary`
- analyzed index path
- analyzer path
- whether corpus changed since prior audit
- sourceFiles / processedFiles / indexedFiles / bytesRead when available
- frozen benchmark config
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

### Temporary generation / cleanup

If Step C was used, include:

- task-owned temporary root
- free space before generation
- generated `warm-prime` path
- generated `warm-measured` path
- Warm validation result
- tree identity result
- cleanup result

If an existing index was used, explicitly state that no existing index was deleted.

## Cleanup policy — critical

After the analysis JSON and Markdown report have been fully written and validated:

### If Step A or B was used

Do not delete anything from the selected existing index or its parent artifact/archive location.

### If Step C was used

Delete only the large task-owned generated index bodies under the dedicated task root created by this task.

At minimum remove the generated:

- `warm-prime` index body,
- `warm-measured` index body.

Before deleting:

1. confirm both paths are descendants of the exact recorded task-owned root,
2. confirm neither path is inside the repository,
3. confirm neither path is an existing pre-task index/archive,
4. confirm the CQ3DIR JSON and Markdown report are already safely written outside those index directories,
5. retain compact profile JSON/log metadata needed for audit when practical.

Never delete:

- `C:\Program Files`,
- the repository,
- user documents,
- pre-existing PersonalRag indexes,
- pre-existing Autopilot artifacts or archives,
- any path that cannot be proven to be task-owned.

If cleanup path ownership is ambiguous, do not delete it. Report `TEMP_CLEANUP_SKIPPED_UNSAFE_PATH` instead.

The desired steady state is that no newly generated multi-GiB index body remains after a successful Step C analysis.

## Git rules

Allowed repository changes for this task:

- `scripts/analyze-cq3dir-readonly.ps1` only if a minimal execution repair is actually required,
- `reports/cq3dir-analysis-*.json`,
- `reports/cq3dir-analysis-*.md`.

Do not commit generated index bodies, raw multi-GiB artifacts, temporary worktrees, or application/production Rust changes.

After checks and cleanup pass:

1. run `git status --short`,
2. stage only the allowed files,
3. commit with a Japanese one-line message such as `CQ3DIRの実データ分布と理論圧縮率をread-only解析`,
4. push the current branch to GitHub.

## Failure behavior

Do not ask the user to manually restore the deleted old index.

Expected safe stop reasons include:

- `CQ3DIR_INSUFFICIENT_TEMP_SPACE`
- `CQ3DIR_TEMP_PROFILE_FAILED`
- `CQ3DIR_TEMP_PROFILE_INVALID`
- `CQ3DIR_ANALYZER_FAILED`
- `TEMP_CLEANUP_SKIPPED_UNSAFE_PATH`

A missing prior exact index is no longer a stop reason by itself.

If a safe autonomous fallback is possible under this document, perform it rather than asking the user for intervention.

## Final response

On success, finish with exactly one compact summary block containing:

```text
CQ3DIR_READONLY_ANALYSIS_COMPLETE
branch=...
commit=...
sourceMethod=existing-exact|existing-fallback|generated-temporary
corpusChangedSincePriorAudit=true|false|unknown
indexGiB=...
cq3DirGiB=...
entries=...
maxCount=...
smallestModeledCandidate=...
smallestModeledDirectoryReductionPercent=...
recommendedCandidates=...
temporaryIndexesDeleted=true|false|not-created
jsonReport=...
markdownReport=...
pushed=true
```

Do not ask the user to manually paste the JSON back into ChatGPT. The JSON and Markdown report on GitHub are the handoff artifacts; ChatGPT will read them directly from this branch.
