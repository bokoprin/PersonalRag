use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use personalrag_portable_search::{
    CatalogEntry, CatalogSnapshot, ChangeBatch, ChangeKind, DocumentChange, DocumentInput,
    IncrementalPolicy, PlannedUpsert, UpdatePlan, VNextDocumentInput, apply_update_plan,
    fold_ascii, initialize_vnext_generation_store, open_vnext_published_generation,
    plan_incremental_update, publish_vnext_incremental_generation, verify_vnext_generation_store,
};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn temp_root(label: &str) -> PathBuf {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "personalrag-vnext-durable-{label}-{}-{id}",
        std::process::id()
    ))
}

fn vdoc(id: u64, path: &str, content: &str) -> VNextDocumentInput {
    VNextDocumentInput::new(id, path, fold_ascii(content.as_bytes()))
}

fn pdoc(key: &str, path: &str, content: &str) -> DocumentInput {
    DocumentInput::new(
        key,
        path,
        fold_ascii(path.as_bytes()),
        fold_ascii(content.as_bytes()),
    )
}

fn upsert(id: u64, key: &str, path: &str, content: &str, is_insert: bool) -> PlannedUpsert {
    PlannedUpsert {
        logical_id: id,
        is_insert,
        document: pdoc(key, path, content),
    }
}

fn current_generation(root: &Path) -> u64 {
    fs::read_to_string(root.join("CURRENT"))
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("generation "))
        .unwrap()
        .parse()
        .unwrap()
}

fn current_manifest(root: &Path) -> PathBuf {
    let relative = fs::read_to_string(root.join("CURRENT"))
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("manifest "))
        .unwrap()
        .to_owned();
    root.join(relative)
}

#[test]
fn vnext_durable_initialize_is_restartable_and_spans_segments() {
    let root = temp_root("init");
    let docs = (1..=5u64)
        .map(|id| {
            vdoc(
                id,
                &format!("base/doc_{id}.txt"),
                &format!("base marker {id}"),
            )
        })
        .collect::<Vec<_>>();

    let report = initialize_vnext_generation_store(&root, &docs, 2).unwrap();
    assert_eq!(report.generation, 0);
    assert_eq!(report.live_docs, 5);
    assert_eq!(report.layer_count, 1);
    assert_eq!(report.segment_count, 3);
    assert_eq!(current_generation(&root), 0);

    // Simulate process restart: discard all in-memory state and reconstruct only from CURRENT.
    let reopened = open_vnext_published_generation(&root).unwrap();
    assert_eq!(reopened.live_logical_ids(), &[1, 2, 3, 4, 5]);
    assert_eq!(reopened.search_content(b"marker 4").unwrap(), vec![4]);
    assert_eq!(reopened.search_path(b"DOC_5.TXT").unwrap(), vec![5]);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_durable_incremental_publish_survives_restart_and_matches_newest_wins() {
    let root = temp_root("incremental");
    initialize_vnext_generation_store(
        &root,
        &[
            vdoc(1, "old/one.txt", "old one marker"),
            vdoc(2, "old/two.txt", "old two marker"),
            vdoc(3, "old/three.txt", "delete three marker"),
        ],
        2,
    )
    .unwrap();

    let plan = UpdatePlan {
        base_generation: 0,
        next_generation: 1,
        upserts: vec![
            upsert(2, "two", "new/renamed-two.txt", "new two marker", false),
            upsert(4, "four", "new/four.txt", "insert four marker", true),
        ],
        tombstones: vec![2, 3],
        live_docs_after: 3,
        compaction_recommended: false,
    };
    let report = publish_vnext_incremental_generation(&root, &plan, 1).unwrap();
    assert_eq!(report.generation, 1);
    assert_eq!(report.live_docs, 3);
    assert_eq!(report.layer_count, 2);
    assert_eq!(report.delta_count, 1);
    assert_eq!(report.segment_count, 4);
    assert_eq!(report.tombstone_events, 2);

    let reopened = open_vnext_published_generation(&root).unwrap();
    assert_eq!(reopened.live_logical_ids(), &[1, 2, 4]);
    assert!(
        reopened
            .search_content(b"old two marker")
            .unwrap()
            .is_empty()
    );
    assert!(reopened.search_content(b"delete three").unwrap().is_empty());
    assert_eq!(reopened.search_content(b"new two marker").unwrap(), vec![2]);
    assert_eq!(reopened.search_content(b"insert four").unwrap(), vec![4]);
    assert!(reopened.search_path(b"old/two.txt").unwrap().is_empty());
    assert_eq!(reopened.search_path(b"RENAMED-TWO").unwrap(), vec![2]);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_durable_tombstone_only_generation_is_published_and_reopened() {
    let root = temp_root("delete-only");
    initialize_vnext_generation_store(
        &root,
        &[
            vdoc(1, "one.txt", "keep one"),
            vdoc(2, "two.txt", "remove two"),
        ],
        10,
    )
    .unwrap();
    let plan = UpdatePlan {
        base_generation: 0,
        next_generation: 1,
        upserts: Vec::new(),
        tombstones: vec![2],
        live_docs_after: 1,
        compaction_recommended: false,
    };
    let report = publish_vnext_incremental_generation(&root, &plan, 10).unwrap();
    assert_eq!(report.segment_count, 1);
    assert_eq!(report.tombstone_events, 1);

    let reopened = open_vnext_published_generation(&root).unwrap();
    assert_eq!(reopened.live_logical_ids(), &[1]);
    assert!(reopened.search_content(b"remove two").unwrap().is_empty());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_durable_rejects_stale_publish_without_switching_current() {
    let root = temp_root("stale");
    initialize_vnext_generation_store(&root, &[vdoc(1, "one.txt", "stable marker")], 10).unwrap();
    let before = fs::read(root.join("CURRENT")).unwrap();
    let plan = UpdatePlan {
        base_generation: 9,
        next_generation: 10,
        upserts: vec![upsert(1, "one", "one.txt", "bad marker", false)],
        tombstones: vec![1],
        live_docs_after: 1,
        compaction_recommended: false,
    };
    assert!(publish_vnext_incremental_generation(&root, &plan, 10).is_err());
    assert_eq!(fs::read(root.join("CURRENT")).unwrap(), before);
    assert_eq!(
        open_vnext_published_generation(&root)
            .unwrap()
            .search_content(b"stable marker")
            .unwrap(),
        vec![1]
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_durable_current_manifest_and_tombstone_corruption_fail_closed() {
    let root = temp_root("corrupt");
    initialize_vnext_generation_store(
        &root,
        &[vdoc(1, "one.txt", "one"), vdoc(2, "two.txt", "two")],
        10,
    )
    .unwrap();
    let current_original = fs::read(root.join("CURRENT")).unwrap();
    let mut current_bad = current_original.clone();
    current_bad[0] ^= 1;
    fs::write(root.join("CURRENT"), &current_bad).unwrap();
    assert!(open_vnext_published_generation(&root).is_err());
    fs::write(root.join("CURRENT"), &current_original).unwrap();

    let manifest_path = current_manifest(&root);
    let manifest_original = fs::read(&manifest_path).unwrap();
    let mut manifest_bad = manifest_original.clone();
    manifest_bad[0] ^= 1;
    fs::write(&manifest_path, &manifest_bad).unwrap();
    assert!(open_vnext_published_generation(&root).is_err());
    fs::write(&manifest_path, &manifest_original).unwrap();

    let plan = UpdatePlan {
        base_generation: 0,
        next_generation: 1,
        upserts: Vec::new(),
        tombstones: vec![2],
        live_docs_after: 1,
        compaction_recommended: false,
    };
    publish_vnext_incremental_generation(&root, &plan, 10).unwrap();
    let tombstone_path = root.join("components/delta-g0000000000000001/tombstones.bin");
    let mut tombstones = fs::read(&tombstone_path).unwrap();
    tombstones[16] ^= 1;
    fs::write(&tombstone_path, tombstones).unwrap();
    assert!(open_vnext_published_generation(&root).is_err());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_durable_ignores_orphans_until_current_switches() {
    let root = temp_root("orphan");
    initialize_vnext_generation_store(&root, &[vdoc(1, "one.txt", "visible one")], 10).unwrap();
    fs::create_dir_all(root.join("components/delta-g0000000000000001")).unwrap();
    fs::write(
        root.join("components/delta-g0000000000000001/orphan.bin"),
        b"not published",
    )
    .unwrap();
    fs::write(
        root.join("generations/g0000000000000001-delta.manifest"),
        b"orphan manifest never referenced by CURRENT\n",
    )
    .unwrap();

    assert_eq!(current_generation(&root), 0);
    let reopened = open_vnext_published_generation(&root).unwrap();
    assert_eq!(reopened.generation(), 0);
    assert_eq!(reopened.search_content(b"visible one").unwrap(), vec![1]);
    assert_eq!(verify_vnext_generation_store(&root).unwrap().generation, 0);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_durable_pre_current_validation_failure_leaves_old_snapshot_visible() {
    let root = temp_root("pre-current-failure");
    initialize_vnext_generation_store(&root, &[vdoc(1, "one.txt", "old visible")], 10).unwrap();
    let current_before = fs::read(root.join("CURRENT")).unwrap();
    let plan = UpdatePlan {
        base_generation: 0,
        next_generation: 1,
        upserts: vec![upsert(2, "two", "two.txt", "new orphan payload", true)],
        tombstones: Vec::new(),
        // Deliberately wrong: component/manifest may be durably written, but semantic validation must
        // fail before the CURRENT visibility switch.
        live_docs_after: 999,
        compaction_recommended: false,
    };
    assert!(publish_vnext_incremental_generation(&root, &plan, 10).is_err());
    assert_eq!(fs::read(root.join("CURRENT")).unwrap(), current_before);
    assert!(root.join("components/delta-g0000000000000001").exists());
    assert!(
        root.join("generations/g0000000000000001-delta.manifest")
            .exists()
    );
    let reopened = open_vnext_published_generation(&root).unwrap();
    assert_eq!(reopened.generation(), 0);
    assert_eq!(reopened.search_content(b"old visible").unwrap(), vec![1]);
    assert!(
        reopened
            .search_content(b"new orphan payload")
            .unwrap()
            .is_empty()
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_durable_fully_written_future_snapshot_is_invisible_without_current_switch() {
    let root = temp_root("visibility-switch");
    initialize_vnext_generation_store(&root, &[vdoc(1, "one.txt", "generation zero")], 10).unwrap();
    let current_zero = fs::read(root.join("CURRENT")).unwrap();
    let plan = UpdatePlan {
        base_generation: 0,
        next_generation: 1,
        upserts: vec![upsert(1, "one", "one.txt", "generation one", false)],
        tombstones: vec![1],
        live_docs_after: 1,
        compaction_recommended: false,
    };
    publish_vnext_incremental_generation(&root, &plan, 10).unwrap();
    assert!(
        root.join("generations/g0000000000000001-delta.manifest")
            .exists()
    );

    // Emulate a crash point immediately before CURRENT rename by restoring the previous durable
    // pointer while leaving the complete future component and manifest in place.
    fs::write(root.join("CURRENT"), current_zero).unwrap();
    let reopened = open_vnext_published_generation(&root).unwrap();
    assert_eq!(reopened.generation(), 0);
    assert_eq!(
        reopened.search_content(b"generation zero").unwrap(),
        vec![1]
    );
    assert!(
        reopened
            .search_content(b"generation one")
            .unwrap()
            .is_empty()
    );
    fs::remove_dir_all(root).unwrap();
}

fn fnv1a_test(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[test]
fn vnext_durable_current_rejects_unsafe_manifest_path_even_with_valid_checksum() {
    let root = temp_root("unsafe-current");
    initialize_vnext_generation_store(&root, &[vdoc(1, "one.txt", "one")], 10).unwrap();
    let body = "PRVCU001\ngeneration 0\nmanifest ../escape.manifest\n";
    let checksum = fnv1a_test(body.as_bytes());
    fs::write(
        root.join("CURRENT"),
        format!("{body}checksum {checksum:016x}\n"),
    )
    .unwrap();
    assert!(open_vnext_published_generation(&root).is_err());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_durable_multiple_incremental_generations_restore_newest_version_only() {
    let root = temp_root("multiple-deltas");
    initialize_vnext_generation_store(
        &root,
        &[
            vdoc(1, "one-v0.txt", "version zero marker"),
            vdoc(2, "two.txt", "stable two marker"),
        ],
        10,
    )
    .unwrap();

    let plan1 = UpdatePlan {
        base_generation: 0,
        next_generation: 1,
        upserts: vec![upsert(1, "one", "one-v1.txt", "version one marker", false)],
        tombstones: vec![1],
        live_docs_after: 2,
        compaction_recommended: false,
    };
    publish_vnext_incremental_generation(&root, &plan1, 10).unwrap();

    let plan2 = UpdatePlan {
        base_generation: 1,
        next_generation: 2,
        upserts: vec![upsert(1, "one", "one-v2.txt", "version two marker", false)],
        tombstones: vec![1],
        live_docs_after: 2,
        compaction_recommended: false,
    };
    let report = publish_vnext_incremental_generation(&root, &plan2, 10).unwrap();
    assert_eq!(report.generation, 2);
    assert_eq!(report.layer_count, 3);
    assert_eq!(report.delta_count, 2);

    let reopened = open_vnext_published_generation(&root).unwrap();
    assert!(
        reopened
            .search_content(b"version zero marker")
            .unwrap()
            .is_empty()
    );
    assert!(
        reopened
            .search_content(b"version one marker")
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        reopened.search_content(b"version two marker").unwrap(),
        vec![1]
    );
    assert!(reopened.search_path(b"one-v0").unwrap().is_empty());
    assert!(reopened.search_path(b"one-v1").unwrap().is_empty());
    assert_eq!(reopened.search_path(b"ONE-V2").unwrap(), vec![1]);
    assert_eq!(reopened.search_content(b"stable two").unwrap(), vec![2]);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_durable_publish_integrates_with_catalog_incremental_planner() {
    let root = temp_root("catalog-plan");
    initialize_vnext_generation_store(
        &root,
        &[
            vdoc(1, "a.txt", "alpha old"),
            vdoc(2, "b.txt", "beta delete"),
            vdoc(3, "c.txt", "gamma stable"),
        ],
        10,
    )
    .unwrap();

    let mut catalog = CatalogSnapshot {
        generation: 0,
        next_logical_id: 4,
        ..CatalogSnapshot::default()
    };
    for (key, id) in [("a", 1), ("b", 2), ("c", 3)] {
        catalog.live.insert(
            key.to_owned(),
            CatalogEntry {
                logical_id: id,
                key: key.to_owned(),
                last_generation: 0,
            },
        );
    }
    let batch = ChangeBatch {
        expected_base_generation: 0,
        changes: vec![
            DocumentChange {
                kind: ChangeKind::Upsert,
                key: "a".into(),
                document: Some(pdoc("a", "renamed-a.txt", "alpha newest")),
            },
            DocumentChange {
                kind: ChangeKind::Delete,
                key: "b".into(),
                document: None,
            },
            DocumentChange {
                kind: ChangeKind::Upsert,
                key: "d".into(),
                document: Some(pdoc("d", "d.txt", "delta inserted")),
            },
        ],
    };
    let plan = plan_incremental_update(&catalog, &batch, IncrementalPolicy::default()).unwrap();
    assert_eq!(plan.base_generation, 0);
    assert_eq!(plan.next_generation, 1);
    assert_eq!(plan.tombstones, vec![1, 2]);
    assert_eq!(plan.live_docs_after, 3);

    publish_vnext_incremental_generation(&root, &plan, 10).unwrap();
    let next_catalog = apply_update_plan(&catalog, &plan).unwrap();
    let reopened = open_vnext_published_generation(&root).unwrap();
    let mut expected_ids = next_catalog
        .live
        .values()
        .map(|entry| entry.logical_id)
        .collect::<Vec<_>>();
    expected_ids.sort_unstable();
    assert_eq!(reopened.live_logical_ids(), expected_ids.as_slice());
    assert!(reopened.search_content(b"alpha old").unwrap().is_empty());
    assert_eq!(reopened.search_content(b"alpha newest").unwrap(), vec![1]);
    assert!(reopened.search_content(b"beta delete").unwrap().is_empty());
    assert_eq!(reopened.search_content(b"delta inserted").unwrap(), vec![4]);
    assert_eq!(reopened.search_content(b"gamma stable").unwrap(), vec![3]);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_durable_published_fast_open_still_rejects_segment_checksum_corruption() {
    let root = temp_root("published-fast-checksum");
    let docs = vec![vdoc(1, "src/alpha.txt", "timeout alpha marker")];
    initialize_vnext_generation_store(&root, &docs, 5_000).unwrap();
    let segment = root
        .join("components")
        .join("base-g0000000000000000")
        .join("segment-00000.prseg2");
    let mut bytes = fs::read(&segment).unwrap();
    let footer = u64::from_le_bytes(bytes[48..56].try_into().unwrap()) as usize;
    let corrupt_at = (footer / 2).max(512).min(footer - 1);
    bytes[corrupt_at] ^= 0x01;
    fs::write(&segment, bytes).unwrap();
    assert!(open_vnext_published_generation(&root).is_err());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_streaming_initializer_is_byte_identical_to_slice_initializer() {
    use personalrag_portable_search::initialize_vnext_generation_store_streaming;

    fn files_under(root: &Path) -> Vec<PathBuf> {
        fn walk(root: &Path, current: &Path, out: &mut Vec<PathBuf>) {
            for entry in fs::read_dir(current).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if entry.file_type().unwrap().is_dir() {
                    walk(root, &path, out);
                } else {
                    out.push(path.strip_prefix(root).unwrap().to_path_buf());
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort();
        out
    }

    let slice_root = temp_root("stream-slice");
    let stream1_root = temp_root("stream-iter-1");
    let stream4_root = temp_root("stream-iter-4");
    let docs = (1..=23u64)
        .map(|id| {
            let repeat = 17 + (id as usize % 11) * 37;
            vdoc(
                id,
                &format!("docs/{id:04}/report_{id:04}.txt"),
                &format!("stream marker {id} {}", "payload ".repeat(repeat)),
            )
        })
        .collect::<Vec<_>>();

    let slice_report = initialize_vnext_generation_store(&slice_root, &docs, 5).unwrap();
    let stream1_report =
        initialize_vnext_generation_store_streaming(&stream1_root, docs.clone(), 5, 1).unwrap();
    let stream4_report =
        initialize_vnext_generation_store_streaming(&stream4_root, docs.clone(), 5, 4).unwrap();
    assert_eq!(slice_report, stream1_report);
    assert_eq!(slice_report, stream4_report);

    let slice_files = files_under(&slice_root);
    let stream1_files = files_under(&stream1_root);
    let stream4_files = files_under(&stream4_root);
    assert_eq!(slice_files, stream1_files);
    assert_eq!(slice_files, stream4_files);
    for relative in slice_files {
        let expected = fs::read(slice_root.join(&relative)).unwrap();
        assert_eq!(
            expected,
            fs::read(stream1_root.join(&relative)).unwrap(),
            "1-worker durable bytes differ for {}",
            relative.display()
        );
        assert_eq!(
            expected,
            fs::read(stream4_root.join(&relative)).unwrap(),
            "4-worker durable bytes differ for {}",
            relative.display()
        );
    }

    fs::remove_dir_all(slice_root).unwrap();
    fs::remove_dir_all(stream1_root).unwrap();
    fs::remove_dir_all(stream4_root).unwrap();
}
