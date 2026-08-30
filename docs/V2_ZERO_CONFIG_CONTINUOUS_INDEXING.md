# PersonalRag V2 Zero-Config Continuous Indexing Specification

Date: 2026-08-30  
Status: **Step 8 implementation complete; final target-Windows real-machine confirmation pending**

## 1. Purpose

Step 8 turns the frozen deterministic search engine into a zero-configuration desktop index that owns its lifecycle. A normal launch discovers fixed local volumes, makes any already-published index immediately searchable, resumes incomplete work, and then keeps filename/path and content results converged with the filesystem without requiring manual index management.

Frozen Step 1–5 identities and semantics are not changed by Step 8.

## 2. Startup lifecycle

Per volume, startup chooses exactly one action:

- `FreshMetadataBuild`
- `ResumeMetadataBuild`
- `ResumeContentBuild`
- `CatchUpChanges`
- `Reconcile`
- `Ready`

A valid Step 4 checkpoint selects `CatchUpChanges`. Missing, unavailable, reset, or out-of-range USN state selects fail-safe reconciliation. A completed index is not rebuilt merely because the application started.

## 3. Metadata continuous update

Step 8 reuses the frozen Step 4 primitives:

- `PRV2DEL1` v1
- `PRV2INC1` v1
- `DeltaOverlay`
- `UsnCheckpoint`
- `UsnNormalizer`

Create, modify, rename, and delete events update the volume overlay by stable FileID. Unknown parentage, hard-link ambiguity, FileID mismatch, journal gaps/resets, and event-resolution I/O errors require reconciliation instead of guessing.

The app-layer `incremental-pair-%020u.state` file is written last and pairs one metadata generation with one delta/state generation. It is not a replacement for any frozen Step 4 identity. Newest-invalid pair state falls back to an older structurally valid pair; if none is usable the runtime reconciles.

An in-memory ephemeral USN cursor may advance over PersonalRag's own irrelevant filesystem events to avoid repeated scans. It is bound to the durable incremental generation and is never a crash-recovery authority.

## 4. Content continuous update

Step 8 adds a durable dirty-content queue:

- magic: `PRV2CDQ1`
- format version: `1`
- immutable state: `content-dirty/dirty-%020u.prcdq`
- payload: sorted stable FileIDs requiring content reindex
- integrity: CRC64-ECMA

Only FileIDs changed by the current event batch are added/removed. The queue is not reconstructed from all historical `content_changed` flags, preventing already-reindexed files from becoming dirty again on unrelated future events.

Rules:

- create searchable file -> add FileID
- content-changing modify -> add FileID
- rename without content change -> reuse existing content shard through stable FileID/path override
- delete -> remove FileID and suppress old content immediately
- change to non-searchable -> remove FileID and suppress old content

While an ID is dirty, every older shard entry for that stable FileID is excluded from content results even if size/mtime happen to match.

Dirty catch-up publishes in this order:

1. immutable `PRV2IDX1` content shard,
2. verification sidecar and content-map,
3. successor ContentSet state referencing the new shard,
4. successor `PRV2CDQ1` state with attempted IDs removed.

Therefore a crash can cause harmless repeated work, but cannot acknowledge content before a searchable shard is durably referenced.

`VolumePhase::ContentCatchUp` is used while dirty IDs remain. `Ready` requires an empty dirty queue.

## 5. Full reconciliation and content reuse

A full metadata reconciliation compares the previously materialized metadata to the new snapshot by stable FileID. If a complete ContentSet already exists, unchanged shards are carried forward to the new metadata generation and only new/changed searchable FileIDs enter the dirty queue.

A metadata generation change therefore does **not** imply a full content rebuild.

If no complete ContentSet exists, initial content build/resume remains authoritative and the dirty queue starts empty.

## 6. Compaction and garbage collection

### Metadata

The existing Step 4 compaction threshold remains authoritative:

- >= 50,000 delta changes, or
- >= 2% of base metadata records.

Compaction runs only after dirty content is drained. It materializes base+delta into a new `PRV2MET1` snapshot, carries the complete ContentSet forward, publishes an empty successor incremental generation, then garbage-collects obsolete app-layer metadata/incremental files.

### Content

Content compaction is requested when a complete clean ContentSet has more than **24 shards**. A canonical replacement set is rebuilt from current materialized metadata and published atomically by a new ContentSet state.

GC keeps at least two structurally valid ContentSet states and every content generation/map/verification sidecar referenced by those fallback states. Orphan and superseded generations are removed only after the successor state is valid.

### Maintenance cadence

Background compaction/GC runs no more frequently than every **30 seconds** in the runtime coordinator. Foreground search remains independent of the coordinator thread. Content work yields between units of background progress.

## 7. Recovery policy

Fail closed and recover rather than infer:

- corrupt latest ContentSet -> older valid ContentSet
- corrupt latest dirty queue -> older valid dirty queue
- corrupt/missing incremental pair component -> older valid pair or reconciliation
- USN journal reset/gap -> reconciliation
- partial metadata/content publish -> newest fully referenced valid state remains authoritative
- crash after shard publish but before dirty acknowledgement -> file remains dirty and may be safely reindexed
- inaccessible directory -> record count/reporting continues; whole application does not fail

The published existing index remains searchable while newer work is in progress.

## 8. Runtime observability

Per-volume runtime status exposes:

- phase
- metadata record count
- inaccessible directory count
- content indexed/total/skipped counts
- content shard count
- incremental metadata change count
- dirty content FileID count
- last error

A volume counts as content-ready only when its ContentSet is complete, its manifest phase is `Ready`, and the dirty-content count is zero.

## 9. Windows automated E2E

The repository includes `tests/windows_step8_e2e.rs`, which runs against a disposable NTFS VHD with a real USN Journal. The exact native-Windows procedure is defined in `STEP8_WINDOWS_FINAL_VERIFICATION_2026-08-30.md`.

The native test covers:

1. first metadata/content build to `Ready`,
2. live modify/create/rename/delete,
3. real USN-driven metadata delta,
4. dirty-content catch-up,
5. stale old-content exclusion,
6. shutdown,
7. filesystem changes while stopped,
8. restart from the durable checkpoint,
9. catch-up to `Ready` without manual reconciliation.

The VHD is detached in an `always()` cleanup step.

## 10. Acceptance gate

Rust 1.97.1:

```bash
cargo fmt --all -- --check
cargo clippy --offline --locked --all-targets -- -D warnings
cargo test --offline --locked
cargo build --offline --locked --release
```

Native Windows final verification additionally runs:

```text
Create disposable NTFS VHD
fsutil usn createjournal
cargo test --test windows_step8_e2e -- --ignored --nocapture --test-threads=1
Detach VHD
```

The final remaining acceptance boundary after automated gates are green is the user's real Windows machine: normal-user launch, actual fixed volumes, representative data size, long-running idle/update behavior, and human GUI usability.

## 11. Step 8 completion boundary

Step 8 is implementation-complete when all automated gates above pass. Real-machine confirmation does not redefine persistent semantics; it confirms the final product environment and user-visible behavior.
