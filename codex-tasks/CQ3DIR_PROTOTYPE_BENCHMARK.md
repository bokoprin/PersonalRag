# Codex Task: CQ3DIR 4-way read-only prototype benchmark

## Goal

Measure the size/lookup trade-off of four CQ3DIR representations using the current real `C:\Program Files\` corpus, without modifying production Rust code or any PRSEG index bytes.

The four representations are fixed for this task:

1. `current-prefix10`
2. `fixed8-packed14`
3. `blocked-delta-64`
4. `blocked-delta-256`

This is the next-stage experiment after `reports/cq3dir-analysis-20260825-211206.*` showed that CQ3DIR is about 0.93 GiB and that all observed posting counts fit within 14 bits.

The product goal is a DocFetcher-like desktop search experience: compact on-disk index, fast interactive lookup, reasonable build cost, deterministic/fail-closed behavior, and manageable migration risk.

This task is a prototype benchmark only. Do **not** implement a new production format.

## Branch

Work on:

`perf/autopilot-5round-20260823-192255`

First fast-forward to the latest `origin/perf/autopilot-5round-20260823-192255` and read this task plus:

- `scripts/benchmark-cq3dir-prototypes.ps1`
- `scripts/cq3dir-prototype/Cq3Common.cs`
- `scripts/cq3dir-prototype/Cq3Blocked.cs`
- `scripts/cq3dir-prototype/Cq3Benchmark.cs`
- `scripts/analyze-cq3dir-readonly.ps1`
- `reports/cq3dir-analysis-20260825-211206.md`
- `reports/perf-autopilot-20260823-203333/frozen-benchmark-config.json`

Do not merge to `main`.

## Safety / prohibited actions

Do not:

- modify production Rust code,
- modify PRSEG/CQ3DIR/CQ3POST index files,
- change the frozen benchmark configuration,
- change the source corpus,
- disable Defender or change OS/power settings,
- implement a production format or migration,
- delete any pre-existing index/archive,
- merge to `main`.

The prototype source PRSEG files must be opened read-only. Candidate representations are built only in managed memory.

If the prototype code has a compile/runtime bug, make only the minimum repair needed to execute the experiment. Record every repair in the report. Do not redesign the experiment unless a correctness flaw makes the measurement invalid; in that case repair only what is necessary and explain it.

## Source index acquisition

Use the same safe source-selection contract as `codex-tasks/CQ3DIR_READONLY_ANALYSIS.md`.

### Step A: reuse a valid existing index when possible

Search known PersonalRag benchmark/profile roots and any known archive for a complete eligible `warm-measured` index.

It must be:

- complete,
- verified successfully,
- PRSEG005 / Prefix10,
- not rejected/invalidated/stale/disk-full interrupted,
- built with the exact frozen configuration,
- associated with a valid profile summary.

Do not choose an arbitrary application index merely because it exists.

### Step B: if none exists, generate exactly one temporary Warm pair

Create a task-owned root only under:

`%LOCALAPPDATA%\PersonalRag\cq3dir-prototype\<run-id>\`

Before generating, require at least 20 GiB free on the target volume.

Use:

- source root: `C:\Program Files\`
- `scripts/profile-index-build.ps1`
- mode: `Warm`
- exact `FrozenConfigPath`: `reports/perf-autopilot-20260823-203333/frozen-benchmark-config.json`

Do not recompute or alter tuning values.

The Warm run must produce one `warm-prime` and one `warm-measured` only.

Validate before using the generated source:

- profile schemaVersion = 2,
- frozen config exactly matches the JSON artifact,
- verify succeeded,
- warm-prime and warm-measured environment tuples match,
- warm-prime and warm-measured SHA-256 trees match exactly,
- segment count is 14 unless the current corpus legitimately causes otherwise; if different, record it prominently,
- sourceFiles/bytesRead may differ from the prior audit because `C:\Program Files\` changes over time; record `CORPUS_CHANGED_SINCE_PRIOR_AUDIT=true` when applicable, but do not treat that as a benchmark failure if the current Warm pair is internally stable.

The benchmark comparison is only among four representations built from this **same current source index**, so prior-corpus changes do not invalidate the relative experiment.

## Independent size-model cross-check

Before the microbenchmark, run:

`scripts/analyze-cq3dir-readonly.ps1`

against the selected `warm-measured` index and write its JSON to the task-owned artifact directory, outside the index tree.

This analyzer result is the independent size-model oracle for:

- `current-prefix10`
- `fixed8-packed14`
- `blocked-delta-64`
- `blocked-delta-256`

The prototype benchmark's actual in-memory encoded byte counts must exactly match the analyzer's modeled byte counts for all four methods.

If any size differs, stop with:

`CQ3DIR_PROTOTYPE_SIZE_MODEL_MISMATCH`

Do not benchmark a representation whose size model does not match.

## Prototype correctness contract

The benchmark code must compare every candidate against `current-prefix10` before timing.

For each validation key, the following must be identical:

- found/miss result,
- encoding,
- posting count,
- payload offset,
- payload byte length.

Validation keys must include:

- first/middle/last keys for every non-empty prefix,
- deterministic random existing keys,
- deterministic random missing keys.

If any metadata differs, stop with:

`CQ3DIR_PROTOTYPE_CORRECTNESS_MISMATCH`

Do not report timing for an incorrect candidate.

## Benchmark execution

Run from repository root using PowerShell 7 if available, otherwise a compatible Windows PowerShell capable of compiling the included C# prototype.

Use these benchmark parameters unless a minimal compatibility repair is required:

- `QueriesPerWorkload = 16384`
- `Repeats = 5`
- `BatchSize = 256`
- `Seed = 20260825`

Example:

```powershell
pwsh -NoProfile -File .\scripts\benchmark-cq3dir-prototypes.ps1 `
  -IndexPath "<selected warm-measured>" `
  -OutputPath ".\reports\cq3dir-prototype-benchmark-YYYYMMDD-HHMMSS.json" `
  -QueriesPerWorkload 16384 `
  -Repeats 5 `
  -BatchSize 256 `
  -Seed 20260825
```

The benchmark has four workloads:

- `hit-random`
- `miss-random`
- `mixed-random-50`
- `hit-sorted-locality`

The benchmark warms each representation first, rotates representation order across repeats, and reports batch-derived p50/p95/p99 plus run-level ns/op.

Do not change workload definitions merely to favor one representation.

## What the prototype numbers mean

This is a managed C# prototype microbenchmark. It is **not** final evidence of Rust/mmap production latency.

Use it to:

- confirm the compact representations can return exactly equivalent CQ3 metadata,
- confirm their encoded sizes on real data,
- identify obviously unacceptable decode cost,
- rank candidates for a later Rust prototype.

Do not claim that a C# result directly predicts final production nanoseconds.

## Required result checks

After the benchmark JSON is produced, verify all of the following:

1. JSON parses successfully.
2. `SchemaVersion == 1`.
3. `Format == PRSEG005` and `DirectoryKind == Prefix10` for the source.
4. Four representation names are present exactly once.
5. Four workload names are present.
6. Every representation has timing results for every workload.
7. `CorrectnessValidationKeys > 0` and the benchmark exited normally only after equivalence validation.
8. For each representation:
   - `EstimatedWholeIndexBytes == IndexBytes - CurrentCq3DirBytes + DirectoryBytes`.
9. `current-prefix10` has zero directory reduction.
10. `fixed8-packed14`, `blocked-delta-64`, and `blocked-delta-256` byte totals exactly match the independent analyzer candidate sizes.
11. No source index file size/mtime changed during the experiment and no files were created under the selected index directory.
12. No production Rust source changed.

Also calculate and report for every candidate/workload:

- median ns/op,
- ratio vs current,
- batch p50/p95/p99 ns/op,
- million lookups/sec,
- size reduction of CQ3DIR,
- estimated whole-index reduction.

## Interpretation rules

Do not select the numerically smallest representation solely because it is smallest.

Focus on these questions:

### fixed8-packed14

- Does the 20% CQ3DIR reduction come at effectively no lookup penalty?
- Does smaller fixed-stride memory improve locality relative to Prefix10?

### blocked-delta-64

- Is the ~69% CQ3DIR reduction retained by the actual prototype encoder?
- What is the random-hit and mixed lookup penalty versus current?
- Does locality improve enough to offset varint decode work?

### blocked-delta-256

- It is only slightly smaller than blocked64 in the previous model. Does its larger decode window cause materially worse hit latency?
- Is there any workload where it clearly earns its extra complexity?

### Product framing

For a DocFetcher-like desktop search application, a candidate is interesting only if the capacity gain is large enough to justify lookup/build/format complexity.

Do not establish a permanent threshold in product code. In the report, present the trade-off so ChatGPT can decide the next Rust experiment.

## Markdown report

Create:

`reports/cq3dir-prototype-benchmark-YYYYMMDD-HHMMSS.md`

with these sections:

### Executive summary

- source method (`existing` or `generated-temporary`)
- current corpus tuple
- current CQ3DIR size
- representation sizes
- correctness validation count
- one-sentence description of whether blocked64 still looks promising after lookup measurements

### Provenance

- branch and HEAD used for source/profile
- selected/generated index path
- frozen config
- warm-prime/measured tree SHA if generated
- corpus comparison to prior audit

### Correctness

- number and classes of validation keys
- exact metadata fields compared
- result for each candidate

### Size cross-check

Table:

| Representation | Analyzer bytes | Prototype bytes | Exact match | CQ3DIR reduction | Whole-index reduction |

### Lookup benchmark

For each workload, table:

| Representation | median ns/op | ratio vs current | batch p50 | batch p95 | batch p99 | M lookups/s |

### Prototype encode cost

Table with prototype encode milliseconds for fixed8/blocked64/blocked256. State clearly that this is conversion-from-current-directory cost, not integrated production builder cost.

### Trade-off matrix

Compare:

- capacity saving,
- random-hit ratio,
- miss ratio,
- mixed ratio,
- locality ratio,
- implementation complexity,
- likely migration/format risk.

### Recommendation for next experiment

Choose one of:

- `RUST_PROTOTYPE_BLOCKED64`
- `RUST_PROTOTYPE_FIXED8`
- `EXPAND_BLOCK_SIZE_STUDY`
- `NO_CQ3DIR_FORMAT_CHANGE_YET`

This recommendation is only for the **next experiment**. Do not implement it.

Explain the evidence in 3-6 bullets.

### Limitations

Explicitly say that managed C# timing is not final Rust/mmap production evidence.

## Temporary-index cleanup

If and only if this task generated the source index itself:

1. Save benchmark JSON/Markdown and profile/tree metadata first.
2. Verify the benchmark/report artifacts parse and are outside the index directory.
3. Delete only the task-owned `warm-prime` and `warm-measured` directories beneath `%LOCALAPPDATA%\PersonalRag\cq3dir-prototype\<run-id>\`.
4. Keep small profile summary/tree/log metadata if useful for audit.
5. Do not delete any pre-existing index or archive.
6. Record `temporaryIndexesDeleted=true` in the report.

If path ownership cannot be proven, do not delete and report the reason.

## Git rules

Allowed changes:

- `scripts/benchmark-cq3dir-prototypes.ps1` only for a minimal execution repair,
- `scripts/cq3dir-prototype/*.cs` only for a minimal execution/correctness repair,
- `reports/cq3dir-prototype-benchmark-*.json`,
- `reports/cq3dir-prototype-benchmark-*.md`.

Do not stage unrelated existing untracked files such as earlier `reports/index-size-analysis-*` artifacts unless they are already tracked and directly required.

After all checks pass:

1. `git status --short`
2. stage only allowed task files
3. commit with a Japanese one-line message, e.g. `CQ3DIR 4方式の容量とlookup特性をread-only比較`
4. push the current branch
5. confirm local HEAD equals GitHub branch HEAD

## Final response

Finish with one compact block:

```text
CQ3DIR_PROTOTYPE_BENCHMARK_COMPLETE
branch=...
commit=...
sourceMethod=...
corpusChangedSincePriorAudit=...
indexGiB=...
currentCq3DirGiB=...
fixed8Cq3DirReductionPercent=...
blocked64Cq3DirReductionPercent=...
blocked256Cq3DirReductionPercent=...
fixed8MixedRatio=...
blocked64MixedRatio=...
blocked256MixedRatio=...
blocked64RandomHitRatio=...
blocked256RandomHitRatio=...
correctnessValidationKeys=...
sizeModelExactMatch=true
nextExperiment=...
temporaryIndexesDeleted=...
jsonReport=...
markdownReport=...
pushed=true
```

Do not ask the user to paste the report back into ChatGPT. GitHub is the handoff artifact; ChatGPT will read the branch directly.
