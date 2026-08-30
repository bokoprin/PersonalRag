# Step 8 target-Windows final confirmation

Run only after the final Rust regression is green. Use `STEP8_WINDOWS_FINAL_VERIFICATION_2026-08-30.md` for the exact commands and disposable-NTFS/USN procedure.

## Preconditions

- checkout the final Step 8 commit
- Rust 1.97.1
- normal desktop user token first; elevation only as a diagnostic comparison
- preserve the existing `%LOCALAPPDATA%\PersonalRag` store if testing restart/reuse

## Required confirmation

1. Launch PersonalRag with no index-management arguments; GUI appears immediately.
2. Fixed local volumes are discovered automatically.
3. Existing published filename/content results are searchable immediately after restart.
4. Fresh volume build reaches filename search before full content completion.
5. Kill the process during metadata build; restart resumes/recovers and converges.
6. Kill during content build/catch-up; restart keeps completed shards searchable and converges.
7. Create a searchable file while running; filename and content results appear automatically.
8. Modify a file; old content disappears before/while replacement content is indexed, then new content appears.
9. Rename a file without changing contents; filename/path updates and content remains searchable without unnecessary full rebuild.
10. Delete a file; filename and content results disappear.
11. Exit PersonalRag, change files while stopped, restart; durable catch-up converges automatically.
12. Leave the app running through at least one maintenance interval; no update loop, runaway CPU/I/O, or unbounded visible shard growth occurs.
13. Verify more than one actual fixed local volume if available.
14. Verify Access Denied/inaccessible directories do not stop other volumes/search.
15. Confirm GUI search remains responsive during background indexing and catch-up.

## Record

Capture:

- final commit SHA
- Windows version
- normal/elevated token mode
- discovered volumes
- startup-to-first filename result
- startup-to-content Ready
- live modify convergence time
- stopped-then-restart convergence time
- approximate idle CPU and disk activity
- final PASS/FAIL for items 1–15

If all items pass, record `STEP8_TARGET_WINDOWS_COMPLETE`.
