# Filename / path engine architecture

The filename phase is isolated in `src/FilenameSearch` and has four layers:

```text
FileSystemCatalog (Windows enumeration + watcher + recovery)
        |
        v
FilenameSearchEngine (stable product API)
        |
        v
Route C (adaptive short n-gram + selective compressed postings)
        |
        v
FilenameSearch.Core (NFC/full case-fold, glob, AND semantics)
```

`FileSystemCatalog` owns filesystem concerns. It enumerates the selected local root with
`IgnoreInaccessible`, skips reparse points, excludes the store directory, and retains one
`FilenameRecord` per file or directory. FileSystemWatcher events are debounced and coalesced.
A directory event, watcher overflow, or restart schedules a full reconcile; a single-file
change is applied as an overlay immediately. Reconcile also reuses an old ID for a moved file
when its metadata signature is unchanged. The persisted snapshot is written to a temporary
file and atomically replaced, so a failed write cannot publish a partial index.

`FilenameSearchEngine` is the stable boundary for the GUI and future engines. Its public
contract contains only filename/path metadata and a `SearchRequest`; there are no content
extractor, regex-content, embedding, or vector-store methods. `SaveSnapshotAtomic` lets the
background persister build a private Route C snapshot so persistence does not hold the live
search lock during a large re-index.

The GUI project `src/FilenameSearch.Gui` consumes only this boundary. It keeps the frozen
filename/path fields, scope and case controls, first 100 rows, status area, keyboard navigation,
Enter-to-open, and request-version stale-result suppression. The content field is visibly
present but disabled for this phase.

The selected route is recorded in [BAKEOFF_DECISION.md](BAKEOFF_DECISION.md). Route A and B
remain benchmark implementations only; neither is referenced by the product project.
