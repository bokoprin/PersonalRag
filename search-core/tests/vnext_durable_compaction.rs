use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use personalrag_portable_search::{
    DocumentInput, PlannedUpsert, UpdatePlan, VNextDocumentInput, compact_vnext_generation_store,
    fold_ascii, initialize_vnext_generation_store, open_vnext_published_generation,
    publish_vnext_incremental_generation, verify_vnext_generation_store,
};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn temp_root(label: &str) -> PathBuf {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "personalrag-vnext-compact-{label}-{}-{id}",
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

fn current_bytes(root: &Path) -> Vec<u8> {
    fs::read(root.join("CURRENT")).unwrap()
}

#[test]
fn vnext_durable_compaction_collapses_layers_and_matches_live_snapshot_exactly() {
    let root = temp_root("collapse");
    let base = (1..=8u64)
        .map(|id| {
            vdoc(
                id,
                &format!("base/doc_{id}.txt"),
                &format!("base marker {id}"),
            )
        })
        .collect::<Vec<_>>();
    initialize_vnext_generation_store(&root, &base, 3).unwrap();

    publish_vnext_incremental_generation(
        &root,
        &UpdatePlan {
            base_generation: 0,
            next_generation: 1,
            upserts: vec![
                upsert(2, "delta1/two.txt", "new two marker", false),
                upsert(9, "delta1/nine.txt", "insert nine marker", true),
            ],
            tombstones: vec![2, 3],
            live_docs_after: 8,
            compaction_recommended: false,
        },
        2,
    )
    .unwrap();
    publish_vnext_incremental_generation(
        &root,
        &UpdatePlan {
            base_generation: 1,
            next_generation: 2,
            upserts: vec![upsert(1, "delta2/one.txt", "newest one marker", false)],
            tombstones: vec![1, 4],
            live_docs_after: 7,
            compaction_recommended: false,
        },
        2,
    )
    .unwrap();

    let before = open_vnext_published_generation(&root).unwrap();
    let expected = before.materialize_live_documents().unwrap();
    assert_eq!(before.layer_count(), 3);
    assert_eq!(before.live_logical_ids(), &[1, 2, 5, 6, 7, 8, 9]);

    let report = compact_vnext_generation_store(&root, 3).unwrap();
    assert_eq!(report.source_generation, 2);
    assert_eq!(report.compacted_generation, 3);
    assert_eq!(report.live_docs, 7);
    assert_eq!(report.source_layer_count, 3);
    assert!(report.source_segment_count > report.compacted_segment_count);
    assert_eq!(report.source_tombstone_events, 4);
    assert_eq!(report.compacted_segment_count, 3);

    let durable = verify_vnext_generation_store(&root).unwrap();
    assert_eq!(durable.generation, 3);
    assert_eq!(durable.layer_count, 1);
    assert_eq!(durable.delta_count, 0);
    assert_eq!(durable.tombstone_events, 0);
    assert_eq!(durable.segment_count, 3);

    let reopened = open_vnext_published_generation(&root).unwrap();
    assert_eq!(reopened.materialize_live_documents().unwrap(), expected);
    assert!(
        reopened
            .search_content(b"base marker 3")
            .unwrap()
            .is_empty()
    );
    assert!(
        reopened
            .search_content(b"base marker 4")
            .unwrap()
            .is_empty()
    );
    assert_eq!(reopened.search_content(b"new two marker").unwrap(), vec![2]);
    assert_eq!(
        reopened.search_content(b"newest one marker").unwrap(),
        vec![1]
    );
    assert_eq!(
        reopened.search_content(b"insert nine marker").unwrap(),
        vec![9]
    );

    // Old immutable generations are intentionally retained for later safe GC, but are no longer
    // referenced by CURRENT after the compact manifest becomes visible.
    assert!(root.join("components/base-g0000000000000000").exists());
    assert!(root.join("components/delta-g0000000000000001").exists());
    assert!(root.join("components/delta-g0000000000000002").exists());
    assert!(root.join("components/base-g0000000000000003").exists());
    let current = String::from_utf8(current_bytes(&root)).unwrap();
    assert!(current.contains("generation 3"));
    assert!(current.contains("g0000000000000003-compact.manifest"));

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_durable_compacted_base_accepts_future_incremental_delta() {
    let root = temp_root("future-delta");
    initialize_vnext_generation_store(
        &root,
        &[
            vdoc(1, "one.txt", "version zero"),
            vdoc(2, "two.txt", "two"),
        ],
        10,
    )
    .unwrap();
    publish_vnext_incremental_generation(
        &root,
        &UpdatePlan {
            base_generation: 0,
            next_generation: 1,
            upserts: vec![upsert(1, "one.txt", "version one", false)],
            tombstones: vec![1],
            live_docs_after: 2,
            compaction_recommended: true,
        },
        10,
    )
    .unwrap();

    let compacted = compact_vnext_generation_store(&root, 10).unwrap();
    assert_eq!(compacted.compacted_generation, 2);

    publish_vnext_incremental_generation(
        &root,
        &UpdatePlan {
            base_generation: 2,
            next_generation: 3,
            upserts: vec![
                upsert(1, "renamed/one.txt", "version three", false),
                upsert(3, "three.txt", "insert three", true),
            ],
            tombstones: vec![1],
            live_docs_after: 3,
            compaction_recommended: false,
        },
        10,
    )
    .unwrap();

    let reopened = open_vnext_published_generation(&root).unwrap();
    assert_eq!(reopened.generation(), 3);
    assert_eq!(reopened.layer_count(), 2);
    assert!(reopened.search_content(b"version zero").unwrap().is_empty());
    assert!(reopened.search_content(b"version one").unwrap().is_empty());
    assert_eq!(reopened.search_content(b"version three").unwrap(), vec![1]);
    assert_eq!(reopened.search_path(b"renamed/one").unwrap(), vec![1]);
    assert_eq!(reopened.search_content(b"insert three").unwrap(), vec![3]);

    // A compacted non-zero-generation base can itself be compacted again after newer deltas.
    let second = compact_vnext_generation_store(&root, 10).unwrap();
    assert_eq!(second.source_generation, 3);
    assert_eq!(second.compacted_generation, 4);
    let reopened = open_vnext_published_generation(&root).unwrap();
    assert_eq!(reopened.generation(), 4);
    assert_eq!(reopened.layer_count(), 1);
    assert_eq!(reopened.search_content(b"version three").unwrap(), vec![1]);
    assert_eq!(reopened.search_content(b"insert three").unwrap(), vec![3]);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_durable_compaction_supports_empty_live_snapshot() {
    let root = temp_root("empty");
    initialize_vnext_generation_store(
        &root,
        &[vdoc(1, "one.txt", "one"), vdoc(2, "two.txt", "two")],
        10,
    )
    .unwrap();
    publish_vnext_incremental_generation(
        &root,
        &UpdatePlan {
            base_generation: 0,
            next_generation: 1,
            upserts: Vec::new(),
            tombstones: vec![1, 2],
            live_docs_after: 0,
            compaction_recommended: true,
        },
        10,
    )
    .unwrap();

    let report = compact_vnext_generation_store(&root, 10).unwrap();
    assert_eq!(report.compacted_generation, 2);
    assert_eq!(report.live_docs, 0);
    assert_eq!(report.compacted_segment_count, 0);
    let reopened = open_vnext_published_generation(&root).unwrap();
    assert_eq!(reopened.layer_count(), 1);
    assert_eq!(reopened.segment_count(), 0);
    assert!(reopened.search_content(b"one").unwrap().is_empty());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_durable_completed_compaction_is_invisible_until_current_switch() {
    let root = temp_root("current-switch");
    initialize_vnext_generation_store(
        &root,
        &[
            vdoc(1, "one.txt", "base old"),
            vdoc(2, "two.txt", "base two"),
        ],
        10,
    )
    .unwrap();
    publish_vnext_incremental_generation(
        &root,
        &UpdatePlan {
            base_generation: 0,
            next_generation: 1,
            upserts: vec![upsert(1, "one.txt", "delta newest", false)],
            tombstones: vec![1],
            live_docs_after: 2,
            compaction_recommended: true,
        },
        10,
    )
    .unwrap();
    let current_before_compaction = current_bytes(&root);

    compact_vnext_generation_store(&root, 10).unwrap();
    assert!(root.join("components/base-g0000000000000002").exists());
    assert!(
        root.join("generations/g0000000000000002-compact.manifest")
            .exists()
    );

    // Emulate the crash boundary immediately before CURRENT rename: all compacted durable files
    // exist, but the old valid pointer remains the only visible snapshot.
    fs::write(root.join("CURRENT"), current_before_compaction).unwrap();
    let reopened = open_vnext_published_generation(&root).unwrap();
    assert_eq!(reopened.generation(), 1);
    assert_eq!(reopened.layer_count(), 2);
    assert_eq!(reopened.search_content(b"delta newest").unwrap(), vec![1]);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_durable_compaction_rejects_base_only_without_switching_current() {
    let root = temp_root("base-only");
    initialize_vnext_generation_store(&root, &[vdoc(1, "one.txt", "stable")], 10).unwrap();
    let current = current_bytes(&root);
    assert!(compact_vnext_generation_store(&root, 10).is_err());
    assert_eq!(current_bytes(&root), current);
    assert_eq!(
        open_vnext_published_generation(&root)
            .unwrap()
            .search_content(b"stable")
            .unwrap(),
        vec![1]
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_durable_compaction_target_collision_fails_before_current_switch() {
    let root = temp_root("collision");
    initialize_vnext_generation_store(&root, &[vdoc(1, "one.txt", "old visible")], 10).unwrap();
    publish_vnext_incremental_generation(
        &root,
        &UpdatePlan {
            base_generation: 0,
            next_generation: 1,
            upserts: vec![upsert(1, "one.txt", "new visible", false)],
            tombstones: vec![1],
            live_docs_after: 1,
            compaction_recommended: true,
        },
        10,
    )
    .unwrap();
    let current = current_bytes(&root);
    fs::create_dir_all(root.join("components/base-g0000000000000002")).unwrap();

    assert!(compact_vnext_generation_store(&root, 10).is_err());
    assert_eq!(current_bytes(&root), current);
    let reopened = open_vnext_published_generation(&root).unwrap();
    assert_eq!(reopened.generation(), 1);
    assert_eq!(reopened.search_content(b"new visible").unwrap(), vec![1]);

    fs::remove_dir_all(root).unwrap();
}
