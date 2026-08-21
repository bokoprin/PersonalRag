# PersonalRag Perf6/Perf7 Search Hot Path + USN Fast Sync Report

Date: 2026-08-16
Base: `PersonalRag_GUI_PortableCore_IncrementalOffice_Perf5_2026-08-16`

## Scope

This pass implements the agreed performance sequence:

1. reduce name posting finalization cost without changing index bytes;
2. keep generation search sessions open across GUI searches;
3. avoid O(N) path-order First-N scans for rare/zero-hit content queries;
4. add a Windows NTFS USN Change Journal fast-sync path with fail-closed full-scan fallback.

POS23/frontier optimization remains the next text-heavy full-build candidate and is not included in this pass.

## Perf6: name posting finalization

For the u16 document-id fast path:

- q2 now uses one stable key-only counting pass instead of sorting the already ordered document-id portion again;
- q3 is split into 256 shards by the high key byte;
- each q3 shard stores `(low16-key, doc-id)` in `u32` and performs one stable key-only counting pass;
- duplicate `(gram, doc-id)` pairs are removed after grouping;
- the legacy/fallback path remains for larger document-id ranges.

100k filename-only, 50k x 2 segments, worker=1, 5-run median:

- total: 254.768 ms -> 205.352 ms (**19.40% faster**)
- name posting: 112.178 ms -> 64.931 ms (**42.12% faster**)
- index bytes: 52,947,272 -> 52,947,272
- per-file SHA-256 manifest: **byte-identical**

## Perf6: resident generation search session

`PortableEngine` now owns a generation-aware `MergedSearchSession` cache. Searches within the same `index_dir + generation` reuse the opened generation, mmaps/sidecars, and worker pool. Index mutation invalidates the cache. Full-rebuild publication invalidates again while holding the application write lock so an old session reopened during a long build cannot remain live across publication.

Synthetic 50k-document generation, zero-hit path-ascending content query, limit=100:

- Perf5 cold median: 13.302 ms
- Perf6 cold median: 8.092 ms (**39.17% faster**)
- Perf5 repeated median: 13.056 ms
- Perf6 repeated median: 0.0165 ms in this synthetic case (session reuse removes the repeated open cost)

The repeated-query number is a microbenchmark and should not be treated as a general GUI latency promise.

## Perf6: adaptive First-N

The generation path used to preserve GUI path order by checking documents one-by-one. For rare/zero-hit content queries this could degrade to O(N). The bridge now asks Search Core whether the candidate set is sparse enough; when it is, it performs an indexed search, maps logical IDs to GUI rows, and selects the smallest path-order rows with a bounded heap. Common queries retain the existing sequential short-circuit path.

Search Core profiles on a 50k-document generation showed generation open around 10.8-14.3 ms while short indexed q3 searches were roughly 0.35-0.87 ms median, confirming that both index reopen and sequential rare First-N were worthwhile targets.

## Perf7: Windows USN fast sync

A new `bridge-core/src/change_tracker.rs` isolates Windows change tracking from Portable Search Core.

Full scan establishes:

- USN journal checkpoint (`journal_id`, `next_usn`), when available;
- NTFS directory File ID -> relative path map for the scanned subtree.

Subsequent `sync now` attempts:

`USN Journal -> changed paths only -> sparse incremental hydration -> existing generation delta publish`

before falling back to the existing full metadata scan.

Safety/fallback conditions include:

- non-Windows/non-local-drive/non-NTFS or inaccessible journal;
- journal ID mismatch, truncation/wrap, or invalid cursor;
- unsupported USN record version;
- directory rename/delete or tracked namespace changes;
- hardlink/reparse semantics;
- gitignore/custom-glob mode, because the journal path does not re-evaluate those rules;
- incomplete/invalid directory File ID map;
- >500,000 journal records in one catch-up window;
- large incremental change/compaction policy that already requires full rebuild.

USN-reported upserts are reindexed even when size and mtime are unchanged. A regression test deliberately forces the old size/mtime into the sparse update and confirms the new body becomes searchable. Delete+upsert for the same path is resolved as final upsert.

The tracker state is stored as `change-tracker-v1.json` next to the GUI catalog and is tied to root, scope signature, and generation. A zero-change USN read only advances the checkpoint and does not advance the index generation.

## Review fixes

Three review loops found and fixed:

1. an adaptive First-N branch selected the content search function even in its generic name/content branch; it is now type-correct and covered by a filename regression path;
2. a long full rebuild could allow the old search session to be reopened after the initial invalidation; publication now invalidates again under the application write lock;
3. tracked root/directory namespace changes whose parent lies outside the tracked subtree are detected by tracked File ID before parent-path resolution and force a full scan.

The sparse incremental path was also hardened so an upsert wins over a delete for the same final path, and directory File ID deduplication is deterministic.

## Final validation

Linux/offline:

- Search Core unit: **5/5 PASS**
- Search Core production: **32/32 PASS**
- Search Core clippy `--all-targets -D warnings`: **PASS**
- Search Core release + self-test: **PASS / SELF_TEST_PASS**
- Bridge unit: **16/16 PASS**
- App Contract v1: **3/3 PASS**
- Bridge integration: **8/8 PASS**, 1 large-tree stress ignored by default
- large-tree stress explicitly: **PASS**
- Bridge clippy `--all-targets -D warnings`: **PASS**
- Frontend: **16/16 PASS**
- Frontend typecheck: **PASS**
- Frontend production build: **PASS**

Windows GNU cross gate:

- Search Core check/clippy: **PASS**
- Bridge check/clippy: **PASS**
- Bridge Windows test executables link: **PASS**
- Tauri check/clippy `-D warnings`: **PASS**
- Tauri release link: **NOT CLAIMED**; two attempts reached the execution window while third-party Tauri release dependencies were still compiling.

Native Windows USN journal runtime cannot be exercised in this Linux execution environment. The source therefore deliberately fails over to the previous metadata scan if the journal cannot be opened. Windows native acceptance should run the existing `Build-And-Run.cmd`, then run the GUI elevated once, perform a full sync to establish `change-tracker-v1.json`, modify a small number of files, and run `今すぐ同期`; the status phase should enter `journal` rather than walking the whole tree.
