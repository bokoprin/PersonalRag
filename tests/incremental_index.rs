use personalrag_v2::incremental::*;
use personalrag_v2::usn::*;
use personalrag_v2::windows_usn::parse_usn_records_v2;
use personalrag_v2::{
    MetadataFileKind, MetadataIndex, MetadataRecord, MetadataSearchRequest, load_generation,
    publish_generation,
};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_dir(label: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "personalrag-step4-{label}-{}-{stamp}",
        std::process::id()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn record(id: u64, path: &str, size: u64, modified: u128, searchable: bool) -> MetadataRecord {
    MetadataRecord {
        file_id: id,
        path: PathBuf::from(path),
        source_root: 0,
        size,
        modified_ns: modified,
        kind: MetadataFileKind::File,
        content_searchable: searchable,
        extractable: false,
    }
}

fn base_metadata() -> MetadataIndex {
    MetadataIndex::build(vec![
        record(100, "alpha.txt", 11, 1, true),
        record(200, "docs/bravo.txt", 11, 1, true),
        record(300, "docs/charlie.txt", 12, 1, true),
    ])
    .unwrap()
}

#[test]
fn delta_overlay_create_rename_delete_and_same_path_replacement_are_exact() {
    let base = base_metadata();
    let mut delta = DeltaOverlay::new(&base, 2, 1);
    delta.upsert(&base, record(400, "new/delta.txt", 5, 2, true), true);
    delta
        .rename(&base, 200, PathBuf::from("moved/bravo.txt"))
        .unwrap();
    delta.delete(&base, 300);
    delta.upsert(&base, record(500, "alpha.txt", 7, 3, true), true);

    let alpha = delta.metadata_search(&base, MetadataSearchRequest::filename("alpha"));
    assert_eq!(
        alpha.iter().map(|hit| hit.file_id).collect::<Vec<_>>(),
        vec![500]
    );
    let old = delta.metadata_search(&base, MetadataSearchRequest::path("docs/bravo"));
    assert!(old.is_empty());
    let new = delta.metadata_search(&base, MetadataSearchRequest::path("moved/bravo"));
    assert_eq!(new[0].file_id, 200);
    let deleted = delta.metadata_search(&base, MetadataSearchRequest::filename("charlie"));
    assert!(deleted.is_empty());
    let created = delta.metadata_search(&base, MetadataSearchRequest::filename("delta"));
    assert_eq!(created[0].file_id, 400);
}

#[test]
fn overlay_created_then_deleted_does_not_leave_tombstone() {
    let base = base_metadata();
    let mut delta = DeltaOverlay::new(&base, 2, 1);
    delta.upsert(&base, record(999, "ephemeral.txt", 1, 1, false), true);
    delta.delete(&base, 999);
    assert_eq!(delta.upsert_count(), 0);
    assert_eq!(delta.tombstone_count(), 0);
}

#[test]
fn delta_snapshot_round_trip_and_corruption_detection() {
    let root = temp_dir("delta-roundtrip");
    let base = base_metadata();
    let mut delta = DeltaOverlay::new(&base, 2, 1);
    delta
        .rename(&base, 100, PathBuf::from("renamed-alpha.txt"))
        .unwrap();
    delta.delete(&base, 300);
    let path = write_delta_generation(&root, &delta.snapshot()).unwrap();
    let loaded = load_delta_generation(&root, 2).unwrap();
    let restored = DeltaOverlay::from_snapshot(&base, loaded);
    assert_eq!(
        restored.metadata_search(&base, MetadataSearchRequest::filename("renamed"))[0].file_id,
        100
    );
    let mut bytes = fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x55;
    fs::write(&path, bytes).unwrap();
    assert!(load_delta_generation(&root, 2).is_err());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn compaction_threshold_uses_fifty_thousand_or_two_percent() {
    let records = (0..1000)
        .map(|id| record(id, &format!("f{id}.txt"), 0, 0, false))
        .collect();
    let base = MetadataIndex::build(records).unwrap();
    let mut delta = DeltaOverlay::new(&base, 2, 1);
    for id in 0..19 {
        delta.delete(&base, id);
    }
    assert!(!delta.should_compact(1000));
    delta.delete(&base, 19);
    assert!(delta.should_compact(1000));
}

#[test]
fn reconciliation_repairs_create_modify_rename_and_delete() {
    let base = base_metadata();
    let mut delta = DeltaOverlay::new(&base, 2, 1);
    let observed = vec![
        record(100, "moved-alpha.txt", 11, 1, true),
        record(200, "docs/bravo.txt", 99, 9, true),
        record(777, "new.txt", 1, 1, true),
    ];
    delta.reconcile(&base, observed);
    assert_eq!(
        delta.metadata_search(&base, MetadataSearchRequest::filename("moved-alpha"))[0].file_id,
        100
    );
    assert_eq!(
        delta.metadata_search(&base, MetadataSearchRequest::filename("new"))[0].file_id,
        777
    );
    assert!(
        delta
            .metadata_search(&base, MetadataSearchRequest::filename("charlie"))
            .is_empty()
    );
    let materialized = delta.materialize_records(&base);
    assert_eq!(
        materialized.iter().find(|r| r.file_id == 200).unwrap().size,
        99
    );
}

fn setup_bundle() -> (PathBuf, PathBuf, BundleManifest) {
    let root = temp_dir("bundle-root");
    let store = temp_dir("bundle-store");
    fs::write(root.join("alpha.txt"), b"hello alpha\n").unwrap();
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::write(root.join("docs/bravo.txt"), b"hello bravo\n").unwrap();
    let metadata = MetadataIndex::build(vec![
        record(100, "alpha.txt", 12, 0, true),
        record(200, "docs/bravo.txt", 12, 0, true),
    ])
    .unwrap();
    let content = publish_generation(&root, &store, 1, 0).unwrap();
    write_metadata_generation(&store, 1, &metadata).unwrap();
    write_delta_generation(
        &store,
        &DeltaSnapshot {
            generation: 1,
            parent_generation: 0,
            upserts: Vec::new(),
            tombstones: Vec::new(),
        },
    )
    .unwrap();
    write_state_generation(
        &store,
        &IncrementalState {
            generation: 1,
            checkpoint: UsnCheckpoint {
                journal_id: 11,
                next_usn: 100,
            },
            pending_renames: Vec::new(),
        },
    )
    .unwrap();
    let manifest = BundleManifest {
        generation: 1,
        parent_generation: 0,
        content_generation: content.generation,
        metadata_generation: 1,
        delta_generation: 1,
        state_generation: 1,
    };
    write_bundle(&store, manifest).unwrap();
    (root, store, manifest)
}

#[test]
fn bundle_is_commit_point_and_orphan_generations_do_not_replace_old_bundle() {
    let (root, store, old) = setup_bundle();
    let metadata = MetadataIndex::build(vec![
        record(100, "alpha.txt", 12, 0, true),
        record(200, "docs/bravo.txt", 12, 0, true),
    ])
    .unwrap();
    write_metadata_generation(&store, 2, &metadata).unwrap();
    publish_generation(&root, &store, 2, 1).unwrap();
    write_delta_generation(
        &store,
        &DeltaSnapshot {
            generation: 2,
            parent_generation: 1,
            upserts: Vec::new(),
            tombstones: Vec::new(),
        },
    )
    .unwrap();
    write_state_generation(
        &store,
        &IncrementalState {
            generation: 2,
            checkpoint: UsnCheckpoint {
                journal_id: 11,
                next_usn: 120,
            },
            pending_renames: Vec::new(),
        },
    )
    .unwrap();
    let loaded = load_bundle(&root, &store).unwrap();
    assert_eq!(loaded.manifest, old);
    let new = BundleManifest {
        generation: 2,
        parent_generation: 1,
        content_generation: 2,
        metadata_generation: 2,
        delta_generation: 2,
        state_generation: 2,
    };
    write_bundle(&store, new).unwrap();
    assert_eq!(load_bundle(&root, &store).unwrap().manifest, new);
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(store);
}

#[test]
fn corrupt_newest_bundle_or_reference_falls_back_to_previous_valid_bundle() {
    let (root, store, old) = setup_bundle();
    let metadata = MetadataIndex::build(vec![
        record(100, "alpha.txt", 12, 0, true),
        record(200, "docs/bravo.txt", 12, 0, true),
    ])
    .unwrap();
    write_metadata_generation(&store, 2, &metadata).unwrap();
    publish_generation(&root, &store, 2, 1).unwrap();
    write_delta_generation(
        &store,
        &DeltaSnapshot {
            generation: 2,
            parent_generation: 1,
            upserts: Vec::new(),
            tombstones: Vec::new(),
        },
    )
    .unwrap();
    write_state_generation(
        &store,
        &IncrementalState {
            generation: 2,
            checkpoint: UsnCheckpoint {
                journal_id: 11,
                next_usn: 120,
            },
            pending_renames: Vec::new(),
        },
    )
    .unwrap();
    let new = BundleManifest {
        generation: 2,
        parent_generation: 1,
        content_generation: 2,
        metadata_generation: 2,
        delta_generation: 2,
        state_generation: 2,
    };
    write_bundle(&store, new).unwrap();
    fs::write(store.join("delta-00000000000000000002.prdelta"), b"corrupt").unwrap();
    assert_eq!(load_bundle(&root, &store).unwrap().manifest, old);
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(store);
}

#[test]
fn bundle_gc_preserves_references_for_two_valid_fallback_bundles() {
    let (root, store, _) = setup_bundle();
    for generation in 2..=3 {
        let metadata = MetadataIndex::build(vec![
            record(100, "alpha.txt", 12, 0, true),
            record(200, "docs/bravo.txt", 12, 0, true),
        ])
        .unwrap();
        write_metadata_generation(&store, generation, &metadata).unwrap();
        publish_generation(&root, &store, generation, generation - 1).unwrap();
        write_delta_generation(
            &store,
            &DeltaSnapshot {
                generation,
                parent_generation: generation - 1,
                upserts: Vec::new(),
                tombstones: Vec::new(),
            },
        )
        .unwrap();
        write_state_generation(
            &store,
            &IncrementalState {
                generation,
                checkpoint: UsnCheckpoint {
                    journal_id: 11,
                    next_usn: 100 + generation as i64,
                },
                pending_renames: Vec::new(),
            },
        )
        .unwrap();
        write_bundle(
            &store,
            BundleManifest {
                generation,
                parent_generation: generation - 1,
                content_generation: generation,
                metadata_generation: generation,
                delta_generation: generation,
                state_generation: generation,
            },
        )
        .unwrap();
    }
    gc_bundles(&root, &store, 2).unwrap();
    assert!(!store.join("bundle-00000000000000000001.prbnd").exists());
    for generation in 2..=3 {
        assert!(
            store
                .join(format!("bundle-{generation:020}.prbnd"))
                .exists()
        );
        assert!(store.join(format!("gen-{generation:020}.prv2")).exists());
        assert!(
            store
                .join(format!("metadata-{generation:020}.prmet"))
                .exists()
        );
        assert!(
            store
                .join(format!("delta-{generation:020}.prdelta"))
                .exists()
        );
        assert!(store.join(format!("state-{generation:020}.princ")).exists());
    }
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(store);
}

#[test]
fn rename_reuses_base_content_and_content_modify_suppresses_old_hit() {
    let root = temp_dir("content-overlay-root");
    let store = temp_dir("content-overlay-store");
    fs::write(root.join("old.txt"), b"original needle\n").unwrap();
    let metadata = MetadataIndex::build(vec![record(42, "old.txt", 16, 0, true)]).unwrap();
    publish_generation(&root, &store, 1, 0).unwrap();
    let content = load_generation(&root, &store, 1).unwrap();
    let mut delta = DeltaOverlay::new(&metadata, 2, 1);
    fs::rename(root.join("old.txt"), root.join("renamed.txt")).unwrap();
    delta
        .rename(&metadata, 42, PathBuf::from("renamed.txt"))
        .unwrap();
    let hits = delta
        .content_search_first_batch(
            &root,
            &metadata,
            &content,
            ContentQueryKind::Literal("needle"),
            false,
        )
        .unwrap();
    assert_eq!(hits[0].file_id, 42);

    fs::write(root.join("renamed.txt"), b"replacement token\n").unwrap();
    let mut changed = record(42, "renamed.txt", 18, 2, true);
    changed.content_searchable = true;
    delta.upsert(&metadata, changed, true);
    assert!(
        delta
            .content_search_first_batch(
                &root,
                &metadata,
                &content,
                ContentQueryKind::Literal("needle"),
                false
            )
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        delta
            .content_search_first_batch(
                &root,
                &metadata,
                &content,
                ContentQueryKind::Literal("token"),
                false
            )
            .unwrap()[0]
            .file_id,
        42
    );
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(store);
}

#[test]
fn changed_content_cache_is_reused_and_invalidated_on_modify() {
    let root = temp_dir("content-cache-root");
    let store = temp_dir("content-cache-store");
    fs::write(root.join("item.txt"), b"base token\n").unwrap();
    let metadata = MetadataIndex::build(vec![record(77, "item.txt", 11, 0, true)]).unwrap();
    publish_generation(&root, &store, 1, 0).unwrap();
    let content = load_generation(&root, &store, 1).unwrap();
    let mut delta = DeltaOverlay::new(&metadata, 2, 1);

    fs::write(root.join("item.txt"), b"first_delta_token\n").unwrap();
    delta.upsert(&metadata, record(77, "item.txt", 18, 2, true), true);
    for _ in 0..2 {
        let hits = delta
            .content_search_first_batch(
                &root,
                &metadata,
                &content,
                ContentQueryKind::Literal("first_delta_token"),
                false,
            )
            .unwrap();
        assert_eq!(hits[0].file_id, 77);
    }

    fs::write(root.join("item.txt"), b"second_delta_token\n").unwrap();
    delta.upsert(&metadata, record(77, "item.txt", 19, 3, true), true);
    assert!(
        delta
            .content_search_first_batch(
                &root,
                &metadata,
                &content,
                ContentQueryKind::Literal("first_delta_token"),
                false,
            )
            .unwrap()
            .is_empty()
    );
    let hits = delta
        .content_search_first_batch(
            &root,
            &metadata,
            &content,
            ContentQueryKind::Literal("second_delta_token"),
            false,
        )
        .unwrap();
    assert_eq!(hits[0].file_id, 77);

    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(store);
}

#[test]
fn checkpoint_validation_detects_journal_reset_and_gap() {
    assert_eq!(
        validate_checkpoint(
            UsnCheckpoint {
                journal_id: 7,
                next_usn: 50
            },
            JournalBounds {
                journal_id: 7,
                first_usn: 10,
                next_usn: 100
            }
        ),
        CheckpointStatus::Valid
    );
    assert_eq!(
        validate_checkpoint(
            UsnCheckpoint {
                journal_id: 8,
                next_usn: 50
            },
            JournalBounds {
                journal_id: 7,
                first_usn: 10,
                next_usn: 100
            }
        ),
        CheckpointStatus::ReconcileRequired
    );
    assert_eq!(
        validate_checkpoint(
            UsnCheckpoint {
                journal_id: 7,
                next_usn: 9
            },
            JournalBounds {
                journal_id: 7,
                first_usn: 10,
                next_usn: 100
            }
        ),
        CheckpointStatus::ReconcileRequired
    );
}

#[test]
fn pending_rename_old_prevents_checkpoint_from_advancing_until_new_arrives() {
    let mut normalizer = UsnNormalizer::new(UsnCheckpoint {
        journal_id: 1,
        next_usn: 10,
    });
    let old = UsnRecordV2 {
        file_reference: 99,
        parent_reference: 1,
        usn: 20,
        reason: USN_REASON_RENAME_OLD_NAME,
        attributes: 0,
        name: "old.txt".into(),
    };
    assert!(normalizer.process_batch(&[old], 30).is_empty());
    assert_eq!(normalizer.checkpoint().next_usn, 20);
    let new = UsnRecordV2 {
        file_reference: 99,
        parent_reference: 2,
        usn: 21,
        reason: USN_REASON_RENAME_NEW_NAME,
        attributes: 0,
        name: "new.txt".into(),
    };
    let changes = normalizer.process_batch(&[new], 31);
    assert!(
        matches!(&changes[0], NormalizedFsChange::Rename { old_name, new_name, .. } if old_name == "old.txt" && new_name == "new.txt")
    );
    assert_eq!(normalizer.checkpoint().next_usn, 31);
}

#[test]
fn unmatched_rename_new_and_hardlink_change_require_reconciliation() {
    let mut normalizer = UsnNormalizer::new(UsnCheckpoint {
        journal_id: 1,
        next_usn: 0,
    });
    let record = UsnRecordV2 {
        file_reference: 10,
        parent_reference: 1,
        usn: 2,
        reason: USN_REASON_RENAME_NEW_NAME | USN_REASON_HARD_LINK_CHANGE,
        attributes: 0,
        name: "x".into(),
    };
    let changes = normalizer.process_batch(&[record], 3);
    assert!(
        changes
            .iter()
            .filter(|value| matches!(value, NormalizedFsChange::ReconcileRequired))
            .count()
            >= 1
    );
}

#[test]
fn frn_tree_updates_directory_descendant_paths_without_rescan() {
    let mut tree = FrnTree::default();
    tree.insert(1, 0, "root".into(), true);
    tree.insert(2, 1, "dir".into(), true);
    tree.insert(3, 2, "file.txt".into(), false);
    assert_eq!(
        tree.path_from_root(3).unwrap(),
        PathBuf::from("root/dir/file.txt")
    );
    tree.rename(2, 1, "moved".into());
    assert_eq!(
        tree.path_from_root(3).unwrap(),
        PathBuf::from("root/moved/file.txt")
    );
    assert_eq!(tree.descendant_ids(2), vec![3]);
}

fn make_usn_record(file: u64, parent: u64, usn: i64, reason: u32, name: &str) -> Vec<u8> {
    let utf16 = name.encode_utf16().collect::<Vec<_>>();
    let name_bytes = utf16.len() * 2;
    let len = 60 + name_bytes;
    let mut out = vec![0_u8; len];
    out[0..4].copy_from_slice(&(len as u32).to_le_bytes());
    out[4..6].copy_from_slice(&2_u16.to_le_bytes());
    out[8..16].copy_from_slice(&file.to_le_bytes());
    out[16..24].copy_from_slice(&parent.to_le_bytes());
    out[24..32].copy_from_slice(&usn.to_le_bytes());
    out[40..44].copy_from_slice(&reason.to_le_bytes());
    out[56..58].copy_from_slice(&(name_bytes as u16).to_le_bytes());
    out[58..60].copy_from_slice(&60_u16.to_le_bytes());
    for (index, unit) in utf16.into_iter().enumerate() {
        out[60 + index * 2..62 + index * 2].copy_from_slice(&unit.to_le_bytes());
    }
    out
}

#[test]
fn usn_record_v2_parser_is_strict_and_lossless_for_utf16_names() {
    let mut bytes = make_usn_record(7, 3, 44, USN_REASON_FILE_CREATE, "日本語.txt");
    bytes.extend(make_usn_record(
        8,
        3,
        45,
        USN_REASON_FILE_DELETE,
        "gone.txt",
    ));
    let records = parse_usn_records_v2(&bytes).unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].name, "日本語.txt");
    assert_eq!(records[1].file_reference, 8);
    let mut corrupt = bytes.clone();
    corrupt[0..4].copy_from_slice(&2_u32.to_le_bytes());
    assert!(parse_usn_records_v2(&corrupt).is_err());
}

#[test]
fn same_path_new_file_id_suppresses_old_content_and_exposes_new_content() {
    let root = temp_dir("same-path-content-root");
    let store = temp_dir("same-path-content-store");
    fs::write(root.join("same.txt"), b"old_unique_token\n").unwrap();
    let metadata = MetadataIndex::build(vec![record(10, "same.txt", 17, 0, true)]).unwrap();
    publish_generation(&root, &store, 1, 0).unwrap();
    let content = load_generation(&root, &store, 1).unwrap();
    fs::write(root.join("same.txt"), b"new_unique_token\n").unwrap();
    let mut delta = DeltaOverlay::new(&metadata, 2, 1);
    delta.upsert(&metadata, record(20, "same.txt", 17, 2, true), true);
    assert!(
        delta
            .content_search_first_batch(
                &root,
                &metadata,
                &content,
                ContentQueryKind::Literal("old_unique_token"),
                false
            )
            .unwrap()
            .is_empty()
    );
    let hits = delta
        .content_search_first_batch(
            &root,
            &metadata,
            &content,
            ContentQueryKind::Literal("new_unique_token"),
            false,
        )
        .unwrap();
    assert_eq!(hits[0].file_id, 20);
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(store);
}

#[test]
fn latest_delta_falls_back_when_advisory_current_points_to_corruption() {
    let store = temp_dir("delta-fallback");
    write_delta_generation(
        &store,
        &DeltaSnapshot {
            generation: 1,
            parent_generation: 0,
            upserts: Vec::new(),
            tombstones: vec![7],
        },
    )
    .unwrap();
    write_delta_generation(
        &store,
        &DeltaSnapshot {
            generation: 2,
            parent_generation: 1,
            upserts: Vec::new(),
            tombstones: vec![8],
        },
    )
    .unwrap();
    fs::write(store.join("delta-00000000000000000002.prdelta"), b"broken").unwrap();
    let loaded = load_latest_delta(&store).unwrap();
    assert_eq!(loaded.generation, 1);
    assert_eq!(loaded.tombstones, vec![7]);
    let _ = fs::remove_dir_all(store);
}

#[test]
fn incremental_state_persists_checkpoint_and_pending_rename() {
    let store = temp_dir("state-roundtrip");
    let state = IncrementalState {
        generation: 7,
        checkpoint: UsnCheckpoint {
            journal_id: 99,
            next_usn: 1234,
        },
        pending_renames: vec![PendingRenameState {
            file_id: 42,
            parent_id: 7,
            name: "old-name.txt".into(),
            usn: 1233,
        }],
    };
    write_state_generation(&store, &state).unwrap();
    let loaded = load_state_generation(&store, 7).unwrap();
    assert_eq!(loaded, state);
    let normalizer =
        UsnNormalizer::from_persisted(loaded.checkpoint, loaded.pending_renames.clone());
    assert_eq!(normalizer.checkpoint(), state.checkpoint);
    assert_eq!(normalizer.pending_state(), state.pending_renames);
    let _ = fs::remove_dir_all(store);
}

#[test]
fn additive_content_limits_bound_file_enumeration_without_changing_first_batch_api() {
    let (root, store, _) = setup_bundle();
    let loaded = load_bundle(&root, &store).unwrap();
    let first_batch = loaded
        .delta
        .content_search_first_batch(
            &root,
            &loaded.metadata,
            &loaded.content,
            ContentQueryKind::Literal("hello"),
            false,
        )
        .unwrap();
    let limited = loaded
        .delta
        .content_search_with_limits(
            &root,
            &loaded.metadata,
            &loaded.content,
            personalrag_v2::incremental::ContentSearchOptions {
                query: ContentQueryKind::Literal("hello"),
                case_sensitive: false,
                limits: personalrag_v2::SearchLimits {
                    max_files: 1,
                    max_matches_seen: 32,
                    max_snippets_per_file: 3,
                },
            },
        )
        .unwrap();
    let first_batch_files = first_batch
        .iter()
        .map(|hit| hit.file_id)
        .collect::<std::collections::HashSet<_>>();
    let limited_files = limited
        .iter()
        .map(|hit| hit.file_id)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(first_batch_files.len(), 2);
    assert_eq!(limited_files.len(), 1);
    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(store).unwrap();
}
