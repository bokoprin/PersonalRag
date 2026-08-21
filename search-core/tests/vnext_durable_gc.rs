use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use personalrag_portable_search::{
    DocumentInput, PlannedUpsert, UpdatePlan, VNextDocumentInput, compact_vnext_generation_store,
    fold_ascii, gc_vnext_generation_store, initialize_vnext_generation_store,
    open_vnext_published_generation, publish_vnext_incremental_generation,
};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn temp_root(label: &str) -> PathBuf {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "personalrag-vnext-gc-{label}-{}-{id}",
        std::process::id()
    ))
}

fn vdoc(id: u64, path: &str, content: &str) -> VNextDocumentInput {
    VNextDocumentInput::new(id, path, fold_ascii(content.as_bytes()))
}

fn upsert(id: u64, path: &str, content: &str, is_insert: bool) -> PlannedUpsert {
    PlannedUpsert {
        logical_id: id,
        is_insert,
        document: DocumentInput::new(
            format!("key-{id}"),
            path,
            fold_ascii(path.as_bytes()),
            fold_ascii(content.as_bytes()),
        ),
    }
}

fn make_compacted_store(label: &str) -> PathBuf {
    let root = temp_root(label);
    initialize_vnext_generation_store(
        &root,
        &[
            vdoc(1, "base/one.txt", "base one"),
            vdoc(2, "base/two.txt", "base two"),
            vdoc(3, "base/three.txt", "base three"),
        ],
        2,
    )
    .unwrap();
    publish_vnext_incremental_generation(
        &root,
        &UpdatePlan {
            base_generation: 0,
            next_generation: 1,
            upserts: vec![
                upsert(1, "delta/one.txt", "new one", false),
                upsert(4, "delta/four.txt", "new four", true),
            ],
            tombstones: vec![1, 2],
            live_docs_after: 3,
            compaction_recommended: true,
        },
        2,
    )
    .unwrap();
    compact_vnext_generation_store(&root, 2).unwrap();
    root
}

#[test]
fn vnext_gc_removes_only_unreachable_pre_compaction_files_and_preserves_restart() {
    let root = make_compacted_store("reclaim");
    let expected = open_vnext_published_generation(&root)
        .unwrap()
        .materialize_live_documents()
        .unwrap();

    assert!(root.join("components/base-g0000000000000000").exists());
    assert!(root.join("components/delta-g0000000000000001").exists());
    assert!(root.join("components/base-g0000000000000002").exists());

    let report = gc_vnext_generation_store(&root, Duration::ZERO).unwrap();
    assert_eq!(report.current_generation, 2);
    assert_eq!(report.reachable_component_dirs, 1);
    assert_eq!(report.removed_component_dirs, 2);
    assert_eq!(report.removed_manifest_files, 2);
    assert!(report.reclaimed_bytes > 0);
    assert_eq!(report.deferred_by_grace, 0);
    assert_eq!(report.deferred_in_use, 0);

    assert!(!root.join("components/base-g0000000000000000").exists());
    assert!(!root.join("components/delta-g0000000000000001").exists());
    assert!(root.join("components/base-g0000000000000002").exists());
    assert!(
        !root
            .join("generations/g0000000000000000-base.manifest")
            .exists()
    );
    assert!(
        !root
            .join("generations/g0000000000000001-delta.manifest")
            .exists()
    );
    assert!(
        root.join("generations/g0000000000000002-compact.manifest")
            .exists()
    );

    let reopened = open_vnext_published_generation(&root).unwrap();
    assert_eq!(reopened.materialize_live_documents().unwrap(), expected);
    assert!(reopened.search_content(b"base two").unwrap().is_empty());
    assert_eq!(reopened.search_content(b"new one").unwrap(), vec![1]);
    assert_eq!(reopened.search_content(b"new four").unwrap(), vec![4]);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_gc_keeps_components_reachable_from_cumulative_current_manifest() {
    let root = temp_root("reachable-delta");
    initialize_vnext_generation_store(
        &root,
        &[vdoc(1, "one.txt", "old one"), vdoc(2, "two.txt", "two")],
        10,
    )
    .unwrap();
    publish_vnext_incremental_generation(
        &root,
        &UpdatePlan {
            base_generation: 0,
            next_generation: 1,
            upserts: vec![upsert(1, "one.txt", "new one", false)],
            tombstones: vec![1],
            live_docs_after: 2,
            compaction_recommended: false,
        },
        10,
    )
    .unwrap();

    let report = gc_vnext_generation_store(&root, Duration::ZERO).unwrap();
    assert_eq!(report.current_generation, 1);
    assert_eq!(report.reachable_component_dirs, 2);
    assert_eq!(report.removed_component_dirs, 0);
    assert_eq!(report.removed_manifest_files, 1);
    assert!(root.join("components/base-g0000000000000000").exists());
    assert!(root.join("components/delta-g0000000000000001").exists());

    let reopened = open_vnext_published_generation(&root).unwrap();
    assert_eq!(reopened.search_content(b"new one").unwrap(), vec![1]);
    assert_eq!(reopened.search_content(b"two").unwrap(), vec![2]);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_gc_respects_grace_period() {
    let root = make_compacted_store("grace");
    let report = gc_vnext_generation_store(&root, Duration::from_secs(3600)).unwrap();
    assert_eq!(report.removed_component_dirs, 0);
    assert_eq!(report.removed_manifest_files, 0);
    assert!(report.deferred_by_grace >= 4);
    assert!(root.join("components/base-g0000000000000000").exists());
    assert!(root.join("components/delta-g0000000000000001").exists());
    assert_eq!(
        open_vnext_published_generation(&root)
            .unwrap()
            .search_content(b"new one")
            .unwrap(),
        vec![1]
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_gc_never_touches_unknown_staging_or_future_entries() {
    let root = make_compacted_store("unknown");
    let staging = root.join("components/.publish-active.tmp");
    let unknown = root.join("components/user-owned-data");
    let future = root.join("components/delta-g0000000000000099");
    fs::create_dir_all(&staging).unwrap();
    fs::create_dir_all(&unknown).unwrap();
    fs::create_dir_all(&future).unwrap();
    fs::write(staging.join("partial"), b"partial").unwrap();
    fs::write(unknown.join("keep"), b"keep").unwrap();
    fs::write(future.join("future"), b"future").unwrap();
    let unknown_manifest = root.join("generations/not-a-generation.manifest");
    let future_manifest = root.join("generations/g0000000000000099-delta.manifest");
    fs::write(&unknown_manifest, b"unknown").unwrap();
    fs::write(&future_manifest, b"future").unwrap();

    gc_vnext_generation_store(&root, Duration::ZERO).unwrap();

    assert!(staging.exists());
    assert!(unknown.exists());
    assert!(future.exists());
    assert!(unknown_manifest.exists());
    assert!(future_manifest.exists());
    assert_eq!(
        open_vnext_published_generation(&root)
            .unwrap()
            .search_content(b"new four")
            .unwrap(),
        vec![4]
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_gc_is_idempotent_after_reclaim() {
    let root = make_compacted_store("idempotent");
    let first = gc_vnext_generation_store(&root, Duration::ZERO).unwrap();
    assert_eq!(first.removed_component_dirs, 2);
    assert_eq!(first.removed_manifest_files, 2);
    let second = gc_vnext_generation_store(&root, Duration::ZERO).unwrap();
    assert_eq!(second.removed_component_dirs, 0);
    assert_eq!(second.removed_manifest_files, 0);
    assert_eq!(second.reclaimed_bytes, 0);
    fs::remove_dir_all(root).unwrap();
}
