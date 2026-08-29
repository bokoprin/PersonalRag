# PersonalRag V2 Windows Incremental Index Specification

Date: 2026-08-29  
Status: **FROZEN — Step 4 COMPLETE**

## Purpose

Step 4 keeps the frozen Step 2 content index and Step 3 filename/path metadata index current without rebuilding a million-file base snapshot for each filesystem change.

The frozen Step 1/2/3 semantics and formats are not modified by Step 4.

## Persistent identities

Step 4 adds three independent immutable formats:

- `PRV2DEL1`, version 1 — metadata/content-change delta overlay
- `PRV2INC1`, version 1 — durable USN checkpoint and pending rename state
- `PRV2BND1`, version 1 — atomic bundle commit manifest

All use search semantic id `0x0003_0001` where applicable and CRC64-ECMA validation.

Existing identities remain unchanged:

- content: `PRV2IDX1` format 2
- metadata: `PRV2MET1` format 1

## Base + delta model

The base Step 2/3 snapshots remain immutable.

The overlay contains:

- upserts keyed by stable FileID,
- tombstones keyed by stable FileID,
- an O(1) exact-path ownership map for replacement/collision handling,
- a content-changed flag per upsert.

Rules:

- create -> add overlay upsert,
- metadata-only change -> update overlay metadata,
- rename/move -> update path only; base content index is reused with a verification-path override,
- content modification -> suppress the stale base content entry and search the changed file through the delta content cache,
- delete -> suppress metadata and content hits with a tombstone,
- same-path replacement by a new FileID -> suppress the old FileID and expose only the replacement.

Filename/path search merges overlay results with base candidates. It does not pre-scan the full base to suppress stale results; stale checks are applied only to returned base candidates, with staged candidate expansion when necessary.

Content search caches both:

- the base stable-FileID <-> content-internal-ID mapping, and
- the mini Variant-D index for changed content.

The changed-content cache is invalidated only when a content-changing overlay mutation occurs. This prevents rebuilding the mini content index for every query.

## Compaction

Default compaction is requested when either condition is true:

- delta change count >= 50,000, or
- delta change count >= 2% of base metadata records.

Compaction materializes a new immutable metadata snapshot and content generation, writes an empty successor delta/state generation, then publishes the bundle manifest last.

## Crash-safe bundle commit

`PRV2BND1` is the commit point and references four generations:

1. content generation,
2. metadata generation,
3. delta generation,
4. incremental-state generation.

A partially written newer component is an orphan until a bundle references it. `load_bundle` validates every referenced component and falls back to an older valid bundle if the newest bundle or any referenced generation is corrupt/missing.

GC retains at least two structurally valid fallback bundles and all content/metadata/delta/state generations referenced by those bundles.

Advisory component pointers may move ahead during a crash; bundle loading does not trust them as the transaction boundary.

## Durable USN state

`PRV2INC1` persists:

- USN journal ID,
- next durable USN,
- unresolved `RENAME_OLD_NAME` records.

The durable checkpoint must not advance past a pending rename-old record until its rename-new pair is observed. Restart reconstructs the normalizer from this state.

## Windows USN adapter

The Windows-only adapter implements the low-level interfaces for:

- `FSCTL_QUERY_USN_JOURNAL`,
- `FSCTL_READ_USN_JOURNAL`,
- `FSCTL_ENUM_USN_DATA`,
- strict `USN_RECORD_V2` parsing,
- MFT/FRN enumeration support.

The platform-independent normalizer handles:

- create,
- data/content modify,
- delete,
- rename OLD/NEW pairing,
- directory rename/move and descendant path reconstruction,
- journal reset/gap detection,
- hard-link ambiguity by requiring reconciliation.

If the journal ID changes, the saved USN falls outside the retained range, a rename pair cannot be resolved safely, or topology is ambiguous, the system requires full reconciliation rather than guessing.

## Reconciliation

Full reconciliation compares observed filesystem records by stable FileID to base+overlay state and repairs:

- creates,
- content/metadata modifications,
- renames/moves,
- deletes.

Stale filename/path/content results are not retained after reconciliation.

## Step 4 controlled acceptance

One-million-record base with a 30,000-change storm:

- 10,000 create: **5.789 ms**
- 10,000 rename: **8.679 ms**
- 10,000 delete: **2.323 ms**
- delta generation: **1,910,064 bytes**
- delta publish: **20.633 ms**
- delta reload: **131.341 ms**
- created-result search p50: **3.187 ms**
- renamed-result search p50: **2.816 ms**
- old renamed path p50: **2.822 ms**, 0 hits
- deleted path p50: **2.892 ms**, 0 hits
- unchanged rare base result p50: **2.863 ms**

This confirms the prior O(delta^2) path-ownership regression is removed and event application does not rebuild the million-record base.

Normative evidence:

- `evidence/step4-incremental/storm-1m-final.txt`
- `evidence/step4-incremental/crash-restart-final.txt`
- `evidence/step4-incremental/full-regression-final.txt`
- `evidence/step4-incremental/windows-usn-typecheck-final.txt`

## Environment boundary

The canonical Step 4 implementation was built and tested on the available Linux execution host. The Windows-only module was forced through Rust type checking with `cfg(windows)` enabled, and the USN parser/state machine is covered by synthetic records.

A **live NTFS/USN end-to-end run on Windows was not available in this environment and is not counted as PASS**. Actual target-Windows filesystem/USN behavior remains part of Step 7 product E2E/failure acceptance.

This limitation does not change the frozen Step 4 data/state semantics.


## Step 7 product wiring note

The frozen Step 4 formats/state semantics remain unchanged. Step 7 stabilization adds a runnable native-Windows product producer around them:

```text
personalrag-v2-indexer watch --root <indexed-root> --store <index-store>
```

The producer resumes from `PRV2INC1`, reads the live NTFS USN Journal, treats indexed FileIDs/parent FileIDs as relevance anchors, and triggers deterministic reconciliation before publishing a new `PRV2BND1`. Journal reset/gap conditions reconcile instead of guessing. A relevant journal advance that changes only the durable checkpoint can publish a state-only successor bundle.

This correctness-first producer does not alter Step 4 persistent identities and does not claim that full reconciliation is the final high-scale direct-FRN update strategy. Its live behavior remains subject to Step 7 target-Windows acceptance.
