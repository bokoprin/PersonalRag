use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use personalrag_portable_search::{
    AccelerationProfile, BuildMode, BuildOptions, CatalogEntry, CatalogSnapshot, ChangeBatch,
    ChangeKind, CompactionAutoPolicy, ContentPlanMode, DiskPathBuildConfig, DiskPathInput,
    DocumentChange, DocumentInput, IncrementalPolicy, LazyPersistentIndex, LogicalDocument,
    LogicalDocumentIdentity, MergedIndex, MergedSearchSession, PersistentIndex,
    PooledLazyPersistentIndex, Pos3Policy, PosCodec, Positional2Index, Positional3Index,
    PositionalIndex, Q3Encoding, SearchSession, SegmentReader, apply_update_plan,
    build_disk_path_inputs_index_pipelined, build_disk_paths_index_pipelined, build_index,
    build_index_benchmark, build_index_unified_benchmark, build_positional_sidecars,
    build_positional2_sidecars, build_positional3_sidecars, build_positional23_sidecars,
    build_q2_sidecars, compact_generation, compact_generation_unified, fold_ascii,
    initialize_generation, initialize_generation_from_built_index, plan_incremental_update,
    publish_incremental_update, publish_incremental_update_unified, recommend_build_tuning,
    verify_generation, verify_positional_sidecars, verify_positional2_sidecars,
    verify_positional3_sidecars,
};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "personalrag-rust-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = make_writable_tree(&self.0);
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn make_writable_tree(root: &Path) -> std::io::Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            make_writable_tree(&entry.path())?;
        } else {
            make_writable(&entry.path())?;
        }
    }
    Ok(())
}

fn make_writable(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o644))
    }
    #[cfg(not(unix))]
    {
        let mut permissions = fs::metadata(path)?.permissions();
        // On Windows this toggles the DOS read-only flag rather than Unix write bits.
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions)
    }
}

fn document(id: usize, content: String) -> DocumentInput {
    let name = format!("dir/module_{id:04}.rs");
    DocumentInput::new(
        name.clone(),
        name.clone(),
        fold_ascii(name.as_bytes()),
        fold_ascii(content.as_bytes()),
    )
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty()
        || haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn naive(docs: &[DocumentInput], query: &[u8], names: bool) -> Vec<u32> {
    let query = fold_ascii(query);
    docs.iter()
        .enumerate()
        .filter_map(|(index, doc)| {
            let haystack = if names {
                &doc.normalized_name
            } else {
                &doc.normalized_content
            };
            contains(haystack, &query).then_some(index as u32)
        })
        .collect()
}

fn build_fixture(docs: &[DocumentInput], mode: BuildMode, segment_docs: usize) -> TempDir {
    let temp = TempDir::new("index");
    build_index(
        docs,
        temp.path(),
        &BuildOptions {
            mode,
            segment_docs,
            workers: 3,
        },
    )
    .unwrap();
    temp
}

#[test]
fn production_queries_match_naive_oracle() {
    let mut docs = Vec::new();
    let japanese = [
        "検索システムのインデックスを高速化します。",
        "ファイル名から対象ファイルを検索します。",
        "日本語の文章と障害情報を確認します。",
    ];
    let mut state = 0x1234_5678_u64;
    for id in 0..600usize {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let word = match state % 5 {
            0 => "return",
            1 => "timeout",
            2 => "metadata",
            3 => "personalrag",
            _ => "worker",
        };
        let mut text = format!("fn module_{id}() {{ {word} alpha beta gamma; }}\n");
        if id % 17 == 0 {
            text.push_str(japanese[id % japanese.len()]);
        }
        if id % 41 == 0 {
            text.push_str(&format!(" unique_marker_{id}::deep_timeout_path "));
        }
        docs.push(document(id, text));
    }
    let temp = build_fixture(&docs, BuildMode::Adaptive, 137);
    let index = PersistentIndex::open(temp.path(), true).unwrap();

    let content_queries: &[&[u8]] = &[
        b"a",
        b"re",
        b"ret",
        b"return",
        b"TIMEOUT",
        b"unique_marker_41",
        "検索".as_bytes(),
        "日本語".as_bytes(),
        b"not-present-xyz",
    ];
    for &query in content_queries {
        assert_eq!(
            index.search_content(query).unwrap(),
            naive(&docs, query, false)
        );
        let expected = naive(&docs, query, false)
            .into_iter()
            .take(23)
            .collect::<Vec<_>>();
        assert_eq!(index.first_n(query, false, 23).unwrap(), expected);
    }
    for query in [b"m".as_slice(), b"mo", b"module_", b".rs", b"MODULE_0041"] {
        assert_eq!(index.search_name(query).unwrap(), naive(&docs, query, true));
    }
}

#[test]
fn adaptive_dedup_preserves_document_hits() {
    let common = "shared duplicated content timeout abcdefghijklmnopqrstuvwxyz";
    let docs = (0..160)
        .map(|id| {
            let text = if id % 8 == 0 {
                format!("{common} patch_{id}")
            } else {
                common.to_owned()
            };
            document(id, text)
        })
        .collect::<Vec<_>>();
    let temp = build_fixture(&docs, BuildMode::Adaptive, 160);
    let segment = SegmentReader::open(temp.path().join("seg-00000.prseg"), true).unwrap();
    assert_eq!(
        segment.builder_kind(),
        personalrag_portable_search::BuilderKind::Dedup
    );
    assert!(segment.unit_count() < segment.doc_count());
    let index = PersistentIndex::open(temp.path(), true).unwrap();
    for query in [b"timeout".as_slice(), b"pa", b"patch_80", b"xyz"] {
        assert_eq!(
            index.search_content(query).unwrap(),
            naive(&docs, query, false)
        );
    }
}

#[test]
fn all_q3_encoding_paths_are_exercised() {
    let docs = (0..1_000usize)
        .map(|id| {
            let mut text = format!("base_{id:04}_abcdefghijklmnopqrstuvwxyz ");
            if id < 10 {
                text.push_str("!i! ");
            }
            if id % 23 == 0 {
                text.push_str("@d@ ");
            }
            if (300..350).contains(&id) {
                text.push_str("#b# ");
            }
            if id < 250 {
                text.push_str("$n$ ");
            }
            document(id, text)
        })
        .collect::<Vec<_>>();
    let temp = build_fixture(&docs, BuildMode::Direct, 1_000);
    let segment = SegmentReader::open(temp.path().join("seg-00000.prseg"), true).unwrap();
    for encoding in [
        Q3Encoding::InlineU32,
        Q3Encoding::DeltaVarint,
        Q3Encoding::Block256Bitmap,
        Q3Encoding::DenseBitset,
    ] {
        assert!(
            segment.q3_encoding_count(encoding).unwrap() > 0,
            "missing {encoding:?}"
        );
    }
    let index = PersistentIndex::open(temp.path(), true).unwrap();
    for query in [b"!i!".as_slice(), b"@d@", b"#b#", b"$n$"] {
        assert_eq!(
            index.search_content(query).unwrap(),
            naive(&docs, query, false)
        );
    }
}

#[test]
fn checksum_and_truncation_corruption_are_rejected() {
    let docs = (0..40)
        .map(|id| document(id, format!("document {id} timeout return metadata")))
        .collect::<Vec<_>>();
    let temp = build_fixture(&docs, BuildMode::Direct, 40);
    let source = temp.path().join("seg-00000.prseg");

    let corrupt = temp.path().join("corrupt.prseg");
    fs::copy(&source, &corrupt).unwrap();
    make_writable(&corrupt).unwrap();
    let mut bytes = fs::read(&corrupt).unwrap();
    bytes[520] ^= 0x80;
    fs::write(&corrupt, bytes).unwrap();
    assert!(SegmentReader::open(&corrupt, true).is_err());

    let truncated = temp.path().join("truncated.prseg");
    let bytes = fs::read(&source).unwrap();
    fs::write(&truncated, &bytes[..bytes.len() - 7]).unwrap();
    assert!(SegmentReader::open(&truncated, true).is_err());
}

#[test]
fn incremental_planner_is_last_write_wins_and_preserves_logical_id() {
    let mut base = CatalogSnapshot {
        generation: 7,
        next_logical_id: 12,
        ..CatalogSnapshot::default()
    };
    base.live.insert(
        "a".into(),
        CatalogEntry {
            logical_id: 11,
            key: "a".into(),
            last_generation: 7,
        },
    );
    let doc_a = DocumentInput::new("a", "a", b"a".to_vec(), b"updated".to_vec());
    let doc_b = DocumentInput::new("b", "b", b"b".to_vec(), b"inserted".to_vec());
    let batch = ChangeBatch {
        expected_base_generation: 7,
        changes: vec![
            DocumentChange {
                kind: ChangeKind::Delete,
                key: "a".into(),
                document: None,
            },
            DocumentChange {
                kind: ChangeKind::Upsert,
                key: "a".into(),
                document: Some(doc_a.clone()),
            },
            DocumentChange {
                kind: ChangeKind::Delete,
                key: "missing".into(),
                document: None,
            },
            DocumentChange {
                kind: ChangeKind::Upsert,
                key: "b".into(),
                document: Some(doc_b.clone()),
            },
        ],
    };
    let plan = plan_incremental_update(&base, &batch, IncrementalPolicy::default()).unwrap();
    assert_eq!(plan.base_generation, 7);
    assert_eq!(plan.next_generation, 8);
    assert_eq!(plan.tombstones, vec![11]);
    assert_eq!(plan.upserts.len(), 2);
    assert_eq!(plan.upserts[0].logical_id, 11);
    assert!(!plan.upserts[0].is_insert);
    assert_eq!(plan.upserts[1].logical_id, 12);
    assert!(plan.upserts[1].is_insert);
    let next = apply_update_plan(&base, &plan).unwrap();
    assert_eq!(next.generation, 8);
    assert_eq!(next.live["a"].logical_id, 11);
    assert_eq!(next.live["b"].logical_id, 12);
}

#[test]
fn manifest_and_generation_boundaries_fail_closed() {
    let docs = (0..12)
        .map(|id| document(id, format!("document {id} return timeout")))
        .collect::<Vec<_>>();
    let temp = build_fixture(&docs, BuildMode::Direct, 6);
    let manifest = temp.path().join("manifest.txt");
    make_writable(&manifest).unwrap();
    let original = fs::read_to_string(&manifest).unwrap();
    let unsafe_manifest =
        original.replacen("segment seg-00000.prseg", "segment ../escape.prseg", 1);
    fs::write(&manifest, unsafe_manifest).unwrap();
    assert!(PersistentIndex::open(temp.path(), true).is_err());
    fs::write(&manifest, &original).unwrap();

    let base = CatalogSnapshot {
        generation: u64::MAX,
        next_logical_id: 1,
        ..CatalogSnapshot::default()
    };
    let batch = ChangeBatch {
        expected_base_generation: u64::MAX,
        changes: Vec::new(),
    };
    assert!(plan_incremental_update(&base, &batch, IncrementalPolicy::default()).is_err());

    let invalid_policy = IncrementalPolicy {
        compact_after_delta_docs: 1,
        compact_after_tombstone_ratio: f64::NAN,
    };
    let base = CatalogSnapshot::default();
    let batch = ChangeBatch {
        expected_base_generation: 0,
        changes: Vec::new(),
    };
    assert!(plan_incremental_update(&base, &batch, invalid_policy).is_err());
}

fn naive_logical(
    live: &std::collections::HashMap<String, (u64, DocumentInput)>,
    query: &[u8],
    names: bool,
) -> Vec<u64> {
    let folded = fold_ascii(query);
    let mut hits = live
        .values()
        .filter_map(|(logical_id, document)| {
            let bytes = if names {
                &document.normalized_name
            } else {
                &document.normalized_content
            };
            contains(bytes, &folded).then_some(*logical_id)
        })
        .collect::<Vec<_>>();
    hits.sort_unstable();
    hits
}

fn assert_merged_matches_naive(
    index: &MergedIndex,
    live: &std::collections::HashMap<String, (u64, DocumentInput)>,
) {
    for query in [
        b"a".as_slice(),
        b"re",
        b"ret",
        b"return",
        b"timeout",
        b"renamed",
        "検索".as_bytes(),
        b"not-present",
    ] {
        assert_eq!(
            index.search_content(query).unwrap(),
            naive_logical(live, query, false)
        );
        let expected = naive_logical(live, query, false)
            .into_iter()
            .take(7)
            .collect::<Vec<_>>();
        assert_eq!(index.first_n(query, false, 7).unwrap(), expected);
    }
    for query in [b"module".as_slice(), b"renamed", b".rs", b"new_file"] {
        assert_eq!(
            index.search_name(query).unwrap(),
            naive_logical(live, query, true)
        );
    }
}

#[test]
fn incremental_generation_merge_matches_full_rebuild_and_compacts() {
    use std::collections::HashMap;

    let temp = TempDir::new("generation-merge");
    let store = temp.path().join("store");
    let options = BuildOptions {
        mode: BuildMode::Adaptive,
        segment_docs: 11,
        workers: 3,
    };
    let mut base_docs = Vec::new();
    let mut catalog = CatalogSnapshot {
        generation: 0,
        next_logical_id: 51,
        ..CatalogSnapshot::default()
    };
    let mut live = HashMap::<String, (u64, DocumentInput)>::new();
    for id in 1..=50u64 {
        let key = format!("key-{id:03}");
        let mut doc = document(
            id as usize,
            format!("document {id} return timeout metadata"),
        );
        doc.key = key.clone();
        if id % 13 == 0 {
            doc.normalized_content
                .extend_from_slice(" 日本語検索 ".as_bytes());
        }
        catalog.live.insert(
            key.clone(),
            CatalogEntry {
                logical_id: id,
                key: key.clone(),
                last_generation: 0,
            },
        );
        live.insert(key.clone(), (id, doc.clone()));
        base_docs.push(LogicalDocument::new(id, doc));
    }
    let report = initialize_generation(&store, &base_docs, &options).unwrap();
    assert_eq!(report.generation, 0);
    verify_generation(&store).unwrap();
    let index = MergedIndex::open(&store, true).unwrap();
    assert_merged_matches_naive(&index, &live);

    let update_doc = |key: &str, name: &[u8], content: &[u8]| {
        DocumentInput::new(key, key, fold_ascii(name), fold_ascii(content))
    };
    let batch1 = ChangeBatch {
        expected_base_generation: 0,
        changes: vec![
            DocumentChange {
                kind: ChangeKind::Upsert,
                key: "key-003".into(),
                document: Some(update_doc(
                    "key-003",
                    b"dir/module_0003.rs",
                    b"second version return changed unique_three",
                )),
            },
            DocumentChange {
                kind: ChangeKind::Delete,
                key: "key-005".into(),
                document: None,
            },
            DocumentChange {
                kind: ChangeKind::Upsert,
                key: "key-007".into(),
                document: Some(update_doc(
                    "key-007",
                    b"dir/renamed_module_0007.rs",
                    b"renamed path still contains timeout",
                )),
            },
            DocumentChange {
                kind: ChangeKind::Upsert,
                key: "new-key".into(),
                document: Some(update_doc(
                    "new-key",
                    b"dir/new_file.rs",
                    "新規ファイル 日本語検索 return".as_bytes(),
                )),
            },
        ],
    };
    let plan1 = plan_incremental_update(&catalog, &batch1, IncrementalPolicy::default()).unwrap();
    publish_incremental_update(&store, &plan1, &options).unwrap();
    catalog = apply_update_plan(&catalog, &plan1).unwrap();
    for tombstone in &plan1.tombstones {
        live.retain(|_, (id, _)| id != tombstone);
    }
    for upsert in &plan1.upserts {
        live.insert(
            upsert.document.key.clone(),
            (upsert.logical_id, upsert.document.clone()),
        );
    }
    let index1 = MergedIndex::open(&store, true).unwrap();
    assert_eq!(index1.generation(), 1);
    assert_eq!(index1.delta_count(), 1);
    assert_merged_matches_naive(&index1, &live);

    // Update the same logical id again, delete another id, then create an orphan component.
    let batch2 = ChangeBatch {
        expected_base_generation: 1,
        changes: vec![
            DocumentChange {
                kind: ChangeKind::Upsert,
                key: "key-003".into(),
                document: Some(update_doc(
                    "key-003",
                    b"dir/module_0003.rs",
                    b"third version no timeout but contains return final_three",
                )),
            },
            DocumentChange {
                kind: ChangeKind::Delete,
                key: "key-009".into(),
                document: None,
            },
            DocumentChange {
                kind: ChangeKind::Delete,
                key: "new-key".into(),
                document: None,
            },
        ],
    };
    let plan2 = plan_incremental_update(&catalog, &batch2, IncrementalPolicy::default()).unwrap();
    let id3 = catalog.live["key-003"].logical_id;
    assert_eq!(plan2.upserts[0].logical_id, id3);
    publish_incremental_update(&store, &plan2, &options).unwrap();
    catalog = apply_update_plan(&catalog, &plan2).unwrap();
    for tombstone in &plan2.tombstones {
        live.retain(|_, (id, _)| id != tombstone);
    }
    for upsert in &plan2.upserts {
        live.insert(
            upsert.document.key.clone(),
            (upsert.logical_id, upsert.document.clone()),
        );
    }
    fs::create_dir_all(store.join("components/delta-g9999999999999999-orphan")).unwrap();
    fs::write(
        store.join("components/delta-g9999999999999999-orphan/garbage"),
        b"not published",
    )
    .unwrap();
    let index2 = MergedIndex::open(&store, true).unwrap();
    assert_eq!(index2.generation(), 2);
    assert_eq!(index2.delta_count(), 2);
    assert_merged_matches_naive(&index2, &live);

    // Recreate a deleted key in a later generation. It must receive a new logical id.
    let batch3 = ChangeBatch {
        expected_base_generation: 2,
        changes: vec![DocumentChange {
            kind: ChangeKind::Upsert,
            key: "key-009".into(),
            document: Some(update_doc(
                "key-009",
                b"dir/renamed_recreated_0009.rs",
                b"recreated document return timeout",
            )),
        }],
    };
    let plan3 = plan_incremental_update(&catalog, &batch3, IncrementalPolicy::default()).unwrap();
    assert_ne!(plan3.upserts[0].logical_id, 9);
    publish_incremental_update(&store, &plan3, &options).unwrap();
    let _next_catalog = apply_update_plan(&catalog, &plan3).unwrap();
    for upsert in &plan3.upserts {
        live.insert(
            upsert.document.key.clone(),
            (upsert.logical_id, upsert.document.clone()),
        );
    }
    let before_compact = MergedIndex::open(&store, true).unwrap();
    assert_merged_matches_naive(&before_compact, &live);
    let live_before = before_compact.live_documents().unwrap();

    let compact = compact_generation(&store, &options).unwrap();
    assert!(compact.compacted);
    assert_eq!(compact.generation, 3);
    assert_eq!(compact.delta_count, 0);
    let after_compact = MergedIndex::open(&store, true).unwrap();
    assert_eq!(after_compact.generation(), 3);
    assert_eq!(after_compact.delta_count(), 0);
    assert_eq!(after_compact.live_documents().unwrap(), live_before);
    assert_merged_matches_naive(&after_compact, &live);
}

#[test]
fn incremental_sidecar_corruption_fails_closed() {
    let temp = TempDir::new("generation-corruption");
    let store = temp.path().join("store");
    let options = BuildOptions {
        mode: BuildMode::Direct,
        segment_docs: 8,
        workers: 1,
    };
    let docs = (1..=8u64)
        .map(|id| {
            let mut doc = document(id as usize, format!("return document {id}"));
            doc.key = format!("key-{id}");
            LogicalDocument::new(id, doc)
        })
        .collect::<Vec<_>>();
    initialize_generation(&store, &docs, &options).unwrap();
    let map = store.join("components/base-g0000000000000000/logical-map.bin");
    make_writable(&map).unwrap();
    let mut bytes = fs::read(&map).unwrap();
    bytes[20] ^= 0x40;
    fs::write(&map, bytes).unwrap();
    assert!(MergedIndex::open(&store, true).is_err());
}

#[test]
fn lazy_open_defers_segments_and_matches_eager_results() {
    let docs = (0..120)
        .map(|id| document(id, format!("document {id} return timeout metadata")))
        .collect::<Vec<_>>();
    let temp = build_fixture(&docs, BuildMode::Direct, 20);
    let eager = PersistentIndex::open(temp.path(), true).unwrap();
    let lazy = LazyPersistentIndex::open(temp.path()).unwrap();
    assert_eq!(lazy.opened_segments(), 0);
    assert_eq!(
        lazy.first_n(b"return", false, 7).unwrap(),
        eager.first_n(b"return", false, 7).unwrap()
    );
    assert_eq!(lazy.opened_segments(), 1);
    assert_eq!(
        lazy.search_content(b"timeout").unwrap(),
        eager.search_content(b"timeout").unwrap()
    );
    assert_eq!(lazy.opened_segments(), 6);
    assert_eq!(
        lazy.search_name(b"module_00").unwrap(),
        eager.search_name(b"module_00").unwrap()
    );
}

#[test]
fn benchmark_builder_is_byte_identical_to_durable_builder() {
    let docs = (0..73)
        .map(|id| {
            document(
                id,
                format!("document {id} return timeout error module_{id:04}"),
            )
        })
        .collect::<Vec<_>>();
    let durable = TempDir::new("durable-builder");
    let benchmark = TempDir::new("benchmark-builder");
    let options = BuildOptions {
        mode: BuildMode::Adaptive,
        segment_docs: 17,
        workers: 3,
    };
    build_index(&docs, durable.path(), &options).unwrap();
    build_index_benchmark(&docs, benchmark.path(), &options).unwrap();

    let mut durable_files = fs::read_dir(durable.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    let mut benchmark_files = fs::read_dir(benchmark.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    durable_files.sort();
    benchmark_files.sort();
    assert_eq!(durable_files, benchmark_files);
    for file in durable_files {
        assert_eq!(
            fs::read(durable.path().join(&file)).unwrap(),
            fs::read(benchmark.path().join(&file)).unwrap(),
            "mismatch in {}",
            file.to_string_lossy()
        );
    }
}

#[test]
fn first_n_planner_scales_long_query_threshold_with_corpus_size() {
    let docs = (0..25_000)
        .map(|id| {
            let body = if id % 10 == 0 {
                // Every q3 from "namespace" is present in one content unit, but the exact
                // literal is absent. Segment-zero sampling therefore estimates ~2,500
                // candidates: above the old fixed 1,200 cutoff, but below the corpus-aware
                // long-query threshold (~2,781 for First100 over 25k docs).
                format!("planner_{id} nam ame mes esp spa pac ace")
            } else {
                format!("planner_{id} return error common")
            };
            document(id, body)
        })
        .collect::<Vec<_>>();
    let fixture = build_fixture(&docs, BuildMode::Direct, 500);
    let index = LazyPersistentIndex::open(fixture.path()).unwrap();

    let false_positive_heavy = index.plan_content_query(b"namespace", Some(100)).unwrap();
    assert_eq!(false_positive_heavy.mode, ContentPlanMode::CandidateDriven);
    assert!(false_positive_heavy.estimated_candidates > 1_200);
    assert!(false_positive_heavy.estimated_candidates < 2_781);
    assert!(index.first_n(b"namespace", false, 100).unwrap().is_empty());

    let common = index.plan_content_query(b"return", Some(100)).unwrap();
    assert_eq!(common.mode, ContentPlanMode::OrderDriven);
    assert!(common.estimated_candidates >= 20_000);
}

#[test]
fn adaptive_planner_v2_separates_rare_first_n_from_common_order_driven() {
    let docs = (0..25_000)
        .map(|id| document(id, format!("planner_{id} return namespace error common")))
        .collect::<Vec<_>>();
    let fixture = build_fixture(&docs, BuildMode::Direct, 500);
    let index = LazyPersistentIndex::open(fixture.path()).unwrap();

    let rare = index
        .plan_content_query(b"__missing_rare_literal__", Some(100))
        .unwrap();
    assert_eq!(rare.mode, ContentPlanMode::CandidateDriven);
    assert_eq!(rare.estimated_candidates, 0);
    assert_eq!(rare.workers, 1);

    let common = index.plan_content_query(b"return", Some(100)).unwrap();
    assert_eq!(common.mode, ContentPlanMode::OrderDriven);
    assert!(common.estimated_candidates >= 20_000);

    let exhaustive = index.plan_content_query(b"return", None).unwrap();
    assert_eq!(exhaustive.mode, ContentPlanMode::ScanDriven);
    assert!(exhaustive.estimated_density_ppm >= 900_000);
    assert_eq!(exhaustive.workers, 4);
    assert_eq!(
        index.search_content(b"return").unwrap(),
        index.search_content_with_workers(b"return", 4).unwrap()
    );
}

#[test]
fn q2_compact_sidecar_is_exact_optional_and_fail_closed() {
    let docs = (0..2_000)
        .map(|id| {
            let body = if id % 3 == 0 {
                format!("fn q2_{id}() {{ return config_error_{id}; }}")
            } else {
                format!("fn q2_{id}() {{ namespace item_{id}; }}")
            };
            document(id, body)
        })
        .collect::<Vec<_>>();
    let fixture = build_fixture(&docs, BuildMode::Direct, 500);
    let without = LazyPersistentIndex::open(fixture.path()).unwrap();
    assert_eq!(without.q2_sidecars_available(), 0);
    let expected = without.search_content(b"re").unwrap();

    let report = build_q2_sidecars(fixture.path(), false).unwrap();
    assert_eq!(report.segments, 4);
    assert!(report.bytes > 0);
    let with = LazyPersistentIndex::open(fixture.path()).unwrap();
    assert_eq!(with.q2_sidecars_available(), 4);
    assert_eq!(with.search_content(b"re").unwrap(), expected);
    assert_eq!(
        with.search_content(b"::").unwrap(),
        naive(&docs, b"::", false)
    );
    // Windows does not allow overwriting a file while an mmap-backed index still holds it open.
    drop(with);

    let sidecar = fixture.path().join("seg-00000.q2c");
    let mut bytes = std::fs::read(&sidecar).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x40;
    std::fs::write(&sidecar, bytes).unwrap();
    let corrupt = LazyPersistentIndex::open(fixture.path()).unwrap();
    assert!(corrupt.search_content(b"re").is_err());
}

#[test]
fn q2_parallel_build_is_deterministic_across_many_segments() {
    let docs = (0..2_400)
        .map(|id| {
            let body = match id % 4 {
                0 => format!("fn q2_parallel_{id}() {{ return config_error_{id}; }}"),
                1 => format!("fn q2_parallel_{id}() {{ metadata worker_{id}; }}"),
                2 => format!("fn q2_parallel_{id}() {{ namespace include_{id}; }}"),
                _ => format!("fn q2_parallel_{id}() {{ personalrag timeout_{id}; }}"),
            };
            document(id, body)
        })
        .collect::<Vec<_>>();
    let left = build_fixture(&docs, BuildMode::Direct, 200);
    let right = build_fixture(&docs, BuildMode::Direct, 200);

    let left_report = build_q2_sidecars(left.path(), false).unwrap();
    let right_report = build_q2_sidecars(right.path(), true).unwrap();
    assert_eq!(left_report, right_report);
    assert_eq!(left_report.segments, 12);

    for segment in 0..left_report.segments {
        let name = format!("seg-{segment:05}.q2c");
        assert_eq!(
            fs::read(left.path().join(&name)).unwrap(),
            fs::read(right.path().join(&name)).unwrap(),
            "Q2 sidecar bytes differ for {name}"
        );
    }

    let index = LazyPersistentIndex::open(left.path()).unwrap();
    for query in [b"re".as_slice(), b"er", b"ta", b"::"] {
        assert_eq!(
            index.search_content(query).unwrap(),
            naive(&docs, query, false)
        );
    }
}

#[test]
fn pooled_query_workers_match_lazy_index() {
    let docs = (0..1_200)
        .map(|id| {
            let term = if id % 3 == 0 {
                "return timeout"
            } else {
                "metadata worker"
            };
            document(id, format!("fn pooled_{id}() {{ {term}; }}"))
        })
        .collect::<Vec<_>>();
    let temp = build_fixture(&docs, BuildMode::Adaptive, 137);
    let lazy = LazyPersistentIndex::open(temp.path()).unwrap();
    let pooled = PooledLazyPersistentIndex::open(temp.path(), 4).unwrap();
    for query in [
        b"a".as_slice(),
        b"re",
        b"ret",
        b"return",
        b"timeout",
        b"missing",
    ] {
        assert_eq!(
            pooled.search_content(query).unwrap(),
            lazy.search_content(query).unwrap(),
            "pooled mismatch for {:?}",
            String::from_utf8_lossy(query)
        );
    }
}

#[test]
fn compaction_autotuner_uses_delta_count_bytes_and_tombstones() {
    let docs = (0..2_000)
        .map(|id| {
            LogicalDocument::new(
                id as u64 + 1,
                document(id, format!("compact_{id} return error")),
            )
        })
        .collect::<Vec<_>>();
    let store = TempDir::new("compaction-auto");
    let options = BuildOptions {
        mode: BuildMode::Direct,
        segment_docs: 500,
        workers: 2,
    };
    initialize_generation(store.path(), &docs, &options).unwrap();
    let base = MergedIndex::open(store.path(), false).unwrap();
    assert!(!base.auto_compaction_decision().unwrap().recommended);
    let tuned = base.tuned_compaction_policy();
    assert_eq!(tuned.max_delta_count, 24);
    assert!((tuned.max_delta_bytes_ratio - 0.20).abs() < f64::EPSILON);
    assert!((tuned.max_tombstone_ratio - 0.20).abs() < f64::EPSILON);

    let strict = CompactionAutoPolicy {
        max_delta_count: 1,
        max_delta_bytes_ratio: 1.0,
        max_tombstone_ratio: 1.0,
    };
    let mut snapshot = CatalogSnapshot {
        generation: 0,
        next_logical_id: 2_001,
        ..CatalogSnapshot::default()
    };
    for item in &docs {
        snapshot.live.insert(
            item.document.key.clone(),
            CatalogEntry {
                logical_id: item.logical_id,
                key: item.document.key.clone(),
                last_generation: 0,
            },
        );
    }
    let document = document(0, "compact_0 return changed".into());
    let batch = ChangeBatch {
        expected_base_generation: 0,
        changes: vec![DocumentChange {
            kind: ChangeKind::Upsert,
            key: document.key.clone(),
            document: Some(document),
        }],
    };
    let plan = plan_incremental_update(&snapshot, &batch, IncrementalPolicy::default()).unwrap();
    publish_incremental_update(store.path(), &plan, &options).unwrap();
    let merged = MergedIndex::open(store.path(), false).unwrap();
    let decision = merged.compaction_decision(strict).unwrap();
    assert!(decision.recommended);
    assert!(decision.reasons.delta_count);
    assert_eq!(decision.metrics.delta_count, 1);
    assert!(
        merged
            .compaction_decision(CompactionAutoPolicy {
                max_delta_count: 0,
                ..strict
            })
            .is_err()
    );
}

#[test]
fn merged_global_scheduler_matches_single_worker() {
    let docs = (0..3_000)
        .map(|id| {
            LogicalDocument::new(
                id as u64 + 1,
                document(id, format!("global_{id} return namespace error")),
            )
        })
        .collect::<Vec<_>>();
    let store = TempDir::new("merged-global");
    let options = BuildOptions {
        mode: BuildMode::Direct,
        segment_docs: 500,
        workers: 2,
    };
    initialize_generation(store.path(), &docs, &options).unwrap();
    let mut snapshot = CatalogSnapshot {
        generation: 0,
        next_logical_id: 3_001,
        ..CatalogSnapshot::default()
    };
    for item in &docs {
        snapshot.live.insert(
            item.document.key.clone(),
            CatalogEntry {
                logical_id: item.logical_id,
                key: item.document.key.clone(),
                last_generation: 0,
            },
        );
    }
    for round in 0..5u64 {
        let changes = (0..80usize)
            .map(|offset| {
                let id = round as usize * 80 + offset;
                {
                    let document =
                        document(id, format!("global_{id} return changed_{round} error"));
                    DocumentChange {
                        kind: ChangeKind::Upsert,
                        key: document.key.clone(),
                        document: Some(document),
                    }
                }
            })
            .collect::<Vec<_>>();
        let batch = ChangeBatch {
            expected_base_generation: snapshot.generation,
            changes,
        };
        let plan =
            plan_incremental_update(&snapshot, &batch, IncrementalPolicy::default()).unwrap();
        publish_incremental_update(store.path(), &plan, &options).unwrap();
        snapshot = apply_update_plan(&snapshot, &plan).unwrap();
    }
    let merged = MergedIndex::open(store.path(), false).unwrap();
    for query in [b"a".as_slice(), b"re", b"ret", b"return", b"changed_4"] {
        assert_eq!(
            merged.search_content_with_workers(query, 1).unwrap(),
            merged.search_content_with_workers(query, 4).unwrap()
        );
    }
}

#[test]
fn search_session_reuses_workers_and_q2_sidecars() {
    let docs = (0..4_000)
        .map(|id| document(id, format!("session_{id} return namespace config error re")))
        .collect::<Vec<_>>();
    let fixture = build_fixture(&docs, BuildMode::Direct, 500);
    build_q2_sidecars(fixture.path(), false).unwrap();
    let lazy = LazyPersistentIndex::open(fixture.path()).unwrap();
    let session = SearchSession::open(fixture.path(), 4).unwrap();
    for query in [b"a".as_slice(), b"re", b"ret", b"return", b"__rare__"] {
        assert_eq!(
            session.search_content(query).unwrap(),
            lazy.search_content(query).unwrap()
        );
        assert_eq!(
            session.first_n(query, false, 100).unwrap(),
            lazy.first_n(query, false, 100).unwrap()
        );
    }
}

#[test]
fn merged_search_session_reuses_global_workers() {
    let docs = (0..2_000)
        .map(|id| {
            LogicalDocument::new(
                id as u64 + 1,
                document(id, format!("merged_session_{id} return namespace error")),
            )
        })
        .collect::<Vec<_>>();
    let store = TempDir::new("merged-session");
    let options = BuildOptions {
        mode: BuildMode::Direct,
        segment_docs: 400,
        workers: 2,
    };
    initialize_generation(store.path(), &docs, &options).unwrap();
    let merged = MergedIndex::open(store.path(), false).unwrap();
    let session = MergedSearchSession::open(store.path(), false, 4).unwrap();
    for query in [b"a".as_slice(), b"re", b"ret", b"return", b"__rare__"] {
        assert_eq!(
            session.search_content(query).unwrap(),
            merged.search_content(query).unwrap()
        );
        assert_eq!(
            session.first_n(query, false, 100).unwrap(),
            merged.first_n(query, false, 100).unwrap()
        );
    }
}

#[test]
fn build_tuning_follows_measured_memory_tiers() {
    let mib = 1024 * 1024u64;
    let low = recommend_build_tuning(190 * mib, 4);
    assert_eq!(
        (low.segment_docs, low.build_workers, low.scan_workers),
        (2_500, 1, 2)
    );
    let medium = recommend_build_tuning(230 * mib, 4);
    assert_eq!((medium.segment_docs, medium.build_workers), (2_500, 2));
    let constrained_fast = recommend_build_tuning(270 * mib, 4);
    assert_eq!(
        (
            constrained_fast.segment_docs,
            constrained_fast.build_workers
        ),
        (2_500, 4)
    );
    let fast = recommend_build_tuning(384 * mib, 8);
    assert_eq!(
        (fast.segment_docs, fast.build_workers, fast.scan_workers),
        (5_000, 4, 2)
    );
    let dual = recommend_build_tuning(300 * mib, 2);
    assert_eq!((dual.segment_docs, dual.build_workers), (5_000, 2));
}

#[test]
fn positional_sidecar_codecs_are_exact_and_prseg_unchanged() {
    let docs = (0..2_000)
        .map(|id| {
            let body = if id % 5 == 0 {
                format!("fn pos_{id}() {{ return timeout config error unique_{id}; }}")
            } else {
                format!("fn pos_{id}() {{ return timeout config error common_{id}; }}")
            };
            document(id, body)
        })
        .collect::<Vec<_>>();
    let fixture = build_fixture(&docs, BuildMode::Direct, 500);
    let segment = fixture.path().join("seg-00000.prseg");
    let before = fs::read(&segment).unwrap();
    let expected = PersistentIndex::open(fixture.path(), true)
        .unwrap()
        .search_content(b"return")
        .unwrap();

    for codec in [
        PosCodec::DeltaVarint,
        PosCodec::StreamVByte,
        PosCodec::EliasFano,
        PosCodec::Block256Bitmap,
    ] {
        let report = build_positional_sidecars(fixture.path(), codec, 900_000, false).unwrap();
        assert_eq!(report.segments, 4);
        assert!(report.records > 0);
        let positional = PositionalIndex::open(fixture.path(), codec).unwrap();
        assert_eq!(positional.search_content(b"return", 4).unwrap(), expected);
        assert_eq!(
            positional.search_content(b"timeout", 4).unwrap(),
            naive(&docs, b"timeout", false)
        );
    }
    assert_eq!(fs::read(segment).unwrap(), before);
}

#[test]
fn positional_planner_is_optional_exact_and_fail_closed() {
    let docs = (0..25_000)
        .map(|id| {
            document(
                id,
                format!("planner_pos_{id} return timeout config error common"),
            )
        })
        .collect::<Vec<_>>();
    let fixture = build_fixture(&docs, BuildMode::Direct, 500);
    let without = LazyPersistentIndex::open(fixture.path()).unwrap();
    assert_eq!(without.positional_sidecars_available(), 0);
    assert_eq!(
        without.plan_content_query(b"return", None).unwrap().mode,
        ContentPlanMode::ScanDriven
    );
    let expected = without.search_content(b"return").unwrap();

    let report =
        build_positional_sidecars(fixture.path(), PosCodec::production(), 900_000, false).unwrap();
    assert_eq!(report.segments, 50);
    let with = LazyPersistentIndex::open(fixture.path()).unwrap();
    assert_eq!(with.positional_sidecars_available(), 50);
    assert_eq!(
        with.plan_content_query(b"return", None).unwrap().mode,
        ContentPlanMode::PositionalDriven
    );
    assert_eq!(with.search_content(b"return").unwrap(), expected);
    let session = SearchSession::open(fixture.path(), 4).unwrap();
    assert_eq!(session.search_content(b"return").unwrap(), expected);
    assert_eq!(
        session.search_content(b"timeout").unwrap(),
        naive(&docs, b"timeout", false)
    );
    assert_eq!(
        with.search_content(b"__missing_positional__").unwrap(),
        Vec::<u32>::new()
    );
    // Release mmap-backed readers before intentionally corrupting the sidecar on Windows.
    drop(session);
    drop(with);

    let sidecar = fixture.path().join(format!(
        "seg-00000.{}",
        PosCodec::production().sidecar_extension()
    ));
    make_writable(&sidecar).unwrap();
    let mut bytes = fs::read(&sidecar).unwrap();
    // PRPOS001: locate the `ret` posting used by `return` and corrupt its payload.
    // Fast query open skips the whole-file checksum, but the used posting must fail closed.
    const HEADER: usize = 48;
    const PREFIX_BYTES: usize = 257 * 4;
    const RECORD_BYTES: usize = 24;
    let key = u32::from(b'r') | (u32::from(b'e') << 8) | (u32::from(b't') << 16);
    let high = (key >> 16) as usize;
    let read_u32 = |slice: &[u8], offset: usize| {
        u32::from_le_bytes(slice[offset..offset + 4].try_into().unwrap())
    };
    let records = read_u32(&bytes, 16) as usize;
    let begin = read_u32(&bytes, HEADER + high * 4) as usize;
    let end = read_u32(&bytes, HEADER + (high + 1) * 4) as usize;
    let payload_base = HEADER + PREFIX_BYTES + records * RECORD_BYTES;
    let mut corrupted = false;
    for index in begin..end {
        let record = HEADER + PREFIX_BYTES + index * RECORD_BYTES;
        let suffix = u16::from_le_bytes(bytes[record..record + 2].try_into().unwrap()) as u32;
        if suffix == (key & 0xffff) {
            let offset = read_u32(&bytes, record + 8) as usize;
            let len = read_u32(&bytes, record + 12) as usize;
            assert!(len > 0);
            bytes[payload_base + offset + len / 2] ^= 0x20;
            corrupted = true;
            break;
        }
    }
    assert!(corrupted);
    fs::write(&sidecar, bytes).unwrap();
    let corrupt = LazyPersistentIndex::open(fixture.path()).unwrap();
    assert!(corrupt.search_content(b"return").is_err());
    assert!(verify_positional_sidecars(fixture.path(), PosCodec::production()).is_err());
}

#[test]
fn positional2_variable_gram_planner_is_exact_optional_and_fail_closed() {
    let docs = (0..10_000)
        .map(|id| {
            document(
                id,
                format!("planner_pos2_{id} return timeout config error common"),
            )
        })
        .collect::<Vec<_>>();
    let fixture = build_fixture(&docs, BuildMode::Direct, 500);
    let segment = fixture.path().join("seg-00000.prseg");
    let before = fs::read(&segment).unwrap();
    let expected = LazyPersistentIndex::open(fixture.path())
        .unwrap()
        .search_content(b"return")
        .unwrap();

    build_positional_sidecars(fixture.path(), PosCodec::production(), 500_000, false).unwrap();
    let without_pos2 = LazyPersistentIndex::open(fixture.path()).unwrap();
    assert_eq!(without_pos2.positional2_sidecars_available(), 0);
    assert_eq!(
        without_pos2
            .plan_content_query(b"return", None)
            .unwrap()
            .mode,
        ContentPlanMode::PositionalDriven
    );

    let report = build_positional2_sidecars(fixture.path(), 500_000, 500_000, false).unwrap();
    assert_eq!(report.segments, 20);
    assert!(report.records > 0);
    assert_eq!(fs::read(&segment).unwrap(), before);

    let with_pos2 = LazyPersistentIndex::open(fixture.path()).unwrap();
    assert_eq!(with_pos2.positional2_sidecars_available(), 20);
    assert_eq!(
        with_pos2.plan_content_query(b"return", None).unwrap().mode,
        ContentPlanMode::VariableGramDriven
    );
    assert_eq!(with_pos2.search_content(b"return").unwrap(), expected);
    assert_eq!(
        with_pos2.search_content(b"timeout").unwrap(),
        naive(&docs, b"timeout", false)
    );
    let session = SearchSession::open(fixture.path(), 4).unwrap();
    assert_eq!(
        session.search_content(b"config").unwrap(),
        naive(&docs, b"config", false)
    );
    // Release mmap-backed readers before intentionally corrupting the sidecar on Windows.
    drop(session);
    drop(with_pos2);

    let sidecar = fixture.path().join("seg-00000.pos2");
    make_writable(&sidecar).unwrap();
    let mut bytes = fs::read(&sidecar).unwrap();
    assert!(bytes.len() > 40);
    // PRPOS002 binds its header to the source PRSEG checksum at byte 32.
    // A mismatch must fail before any stale accelerator result can be used.
    bytes[32] ^= 0x01;
    fs::write(&sidecar, bytes).unwrap();
    let corrupt = LazyPersistentIndex::open(fixture.path()).unwrap();
    assert!(corrupt.search_content(b"return").is_err());
    assert!(verify_positional2_sidecars(fixture.path()).is_err());
}

#[test]
fn merged_session_mixes_variable_and_positional_base_with_plain_delta() {
    let docs = (0..4_000)
        .map(|id| {
            LogicalDocument::new(
                id as u64 + 1,
                document(
                    id,
                    format!("merged_pos_{id} return timeout config error namespace common"),
                ),
            )
        })
        .collect::<Vec<_>>();
    let store = TempDir::new("merged-positional-base");
    let options = BuildOptions {
        mode: BuildMode::Direct,
        segment_docs: 500,
        workers: 2,
    };
    initialize_generation(store.path(), &docs, &options).unwrap();
    let base_index = store
        .path()
        .join("components")
        .join("base-g0000000000000000");
    let report =
        build_positional_sidecars(&base_index, PosCodec::production(), 500_000, false).unwrap();
    assert_eq!(report.segments, 8);
    let pos2_report = build_positional2_sidecars(&base_index, 500_000, 500_000, false).unwrap();
    assert_eq!(pos2_report.segments, 8);
    let pos3_report = build_positional3_sidecars(
        &base_index,
        500_000,
        500_000,
        16,
        Pos3Policy::Adaptive,
        false,
    )
    .unwrap();
    assert_eq!(pos3_report.segments, 8);

    let mut snapshot = CatalogSnapshot {
        generation: 0,
        next_logical_id: 4_001,
        ..CatalogSnapshot::default()
    };
    for item in &docs {
        snapshot.live.insert(
            item.document.key.clone(),
            CatalogEntry {
                logical_id: item.logical_id,
                key: item.document.key.clone(),
                last_generation: 0,
            },
        );
    }
    let replacement = document(0, "merged_pos_0 changed_payload".into());
    let batch = ChangeBatch {
        expected_base_generation: 0,
        changes: vec![DocumentChange {
            kind: ChangeKind::Upsert,
            key: replacement.key.clone(),
            document: Some(replacement),
        }],
    };
    let plan = plan_incremental_update(&snapshot, &batch, IncrementalPolicy::default()).unwrap();
    publish_incremental_update(store.path(), &plan, &options).unwrap();

    let merged = MergedIndex::open(store.path(), false).unwrap();
    let session = MergedSearchSession::open(store.path(), false, 4).unwrap();
    let expected = merged.search_content_with_workers(b"return", 1).unwrap();
    assert_eq!(expected.len(), 3_999);
    assert!(!expected.contains(&1));
    assert_eq!(session.search_content(b"return").unwrap(), expected);
    let namespace_expected = merged.search_content_with_workers(b"namespace", 1).unwrap();
    assert_eq!(namespace_expected.len(), 3_999);
    assert!(!namespace_expected.contains(&1));
    assert_eq!(
        session.search_content(b"namespace").unwrap(),
        namespace_expected
    );
    assert_eq!(
        session.search_content(b"changed_payload").unwrap(),
        vec![1u64]
    );
}

#[test]
fn dense_segment_merges_preserve_manifest_order_without_global_resort() {
    let docs = (0..4_000)
        .map(|id| {
            document(
                id,
                format!(
                    "ordered_dense_{id} return timeout namespace configuration implementation common"
                ),
            )
        })
        .collect::<Vec<_>>();
    let fixture = build_fixture(&docs, BuildMode::Direct, 250);
    build_positional_sidecars(fixture.path(), PosCodec::production(), 500_000, false).unwrap();
    build_positional2_sidecars(fixture.path(), 500_000, 500_000, false).unwrap();
    build_positional3_sidecars(
        fixture.path(),
        500_000,
        500_000,
        16,
        Pos3Policy::Adaptive,
        false,
    )
    .unwrap();

    let assert_ordered = |hits: &[u32]| {
        assert!(hits.windows(2).all(|pair| pair[0] < pair[1]));
    };
    let expected = naive(&docs, b"namespace", false);

    let positional = PositionalIndex::open(fixture.path(), PosCodec::production()).unwrap();
    let pos_hits = positional.search_content(b"namespace", 4).unwrap();
    assert_eq!(pos_hits, expected);
    assert_ordered(&pos_hits);

    let positional2 = Positional2Index::open(fixture.path()).unwrap();
    let pos2_hits = positional2.search_content(b"namespace", 4).unwrap();
    assert_eq!(pos2_hits, expected);
    assert_ordered(&pos2_hits);

    let positional3 = Positional3Index::open(fixture.path()).unwrap();
    let pos3_hits = positional3.search_content(b"namespace", 4).unwrap();
    assert_eq!(pos3_hits, expected);
    assert_ordered(&pos3_hits);

    let lazy = LazyPersistentIndex::open(fixture.path()).unwrap();
    let lazy_hits = lazy.search_content(b"namespace").unwrap();
    assert_eq!(lazy_hits, expected);
    assert_ordered(&lazy_hits);
}

#[test]
fn positional3_adaptive_dense_gram_is_exact_optional_and_prseg_unchanged() {
    let docs = (0..10_000)
        .map(|id| {
            document(
                id,
                format!("pos3_{id} return timeout namespace configuration implementation common"),
            )
        })
        .collect::<Vec<_>>();
    let fixture = build_fixture(&docs, BuildMode::Direct, 500);
    let segment = fixture.path().join("seg-00000.prseg");
    let before = fs::read(&segment).unwrap();
    let report = build_positional3_sidecars(
        fixture.path(),
        500_000,
        500_000,
        16,
        Pos3Policy::Adaptive,
        false,
    )
    .unwrap();
    assert_eq!(report.segments, 20);
    assert!(report.records > 0);
    assert!(report.all_records > 0);
    assert_eq!(fs::read(&segment).unwrap(), before);
    verify_positional3_sidecars(fixture.path()).unwrap();

    let lazy = LazyPersistentIndex::open(fixture.path()).unwrap();
    assert_eq!(lazy.positional3_sidecars_available(), 20);
    assert_eq!(
        lazy.plan_content_query(b"namespace", None).unwrap().mode,
        ContentPlanMode::AdaptiveGramDriven
    );
    assert_eq!(
        lazy.search_content(b"namespace").unwrap(),
        naive(&docs, b"namespace", false)
    );
    let mut expected_first = naive(&docs, b"namespace", false);
    expected_first.truncate(100);
    assert_eq!(
        lazy.first_n(b"namespace", false, 100).unwrap(),
        expected_first
    );

    let pos3 = Positional3Index::open(fixture.path()).unwrap();
    for query in [
        b"return".as_slice(),
        b"timeout".as_slice(),
        b"namespace".as_slice(),
        b"configuration".as_slice(),
        b"implementation".as_slice(),
    ] {
        assert_eq!(
            pos3.search_content(query, 4).unwrap(),
            naive(&docs, query, false)
        );
        let mut expected = naive(&docs, query, false);
        expected.truncate(100);
        assert_eq!(pos3.first_n(query, 100).unwrap(), expected);
    }
    // Release mmap-backed readers before intentionally corrupting the sidecar on Windows.
    drop(pos3);
    drop(lazy);

    let sidecar = fixture.path().join("seg-00000.pos3");
    make_writable(&sidecar).unwrap();
    let mut bytes = fs::read(&sidecar).unwrap();
    assert!(bytes.len() > 48);
    bytes[32] ^= 0x01;
    fs::write(&sidecar, bytes).unwrap();
    assert!(verify_positional3_sidecars(fixture.path()).is_err());
    assert!(Positional3Index::open(fixture.path()).is_err());
}

#[test]
fn shared_pos23_frontier_is_byte_identical_to_separate_tiers() {
    let docs = (0..4_000usize)
        .map(|id| {
            let alias_text = if id % 5 < 3 { "metadata " } else { "metadata(" };
            document(
                id,
                format!(
                    "shared_pos23_{id} return timeout namespace configuration implementation {alias_text}common"
                ),
            )
        })
        .collect::<Vec<_>>();
    let separate = build_fixture(&docs, BuildMode::Direct, 250);
    let combined = build_fixture(&docs, BuildMode::Direct, 250);

    let pos2 = build_positional2_sidecars(separate.path(), 500_000, 400_000, false).unwrap();
    let pos3 = build_positional3_sidecars(
        separate.path(),
        500_000,
        600_000,
        16,
        Pos3Policy::Adaptive,
        false,
    )
    .unwrap();
    let shared = build_positional23_sidecars(
        combined.path(),
        500_000,
        400_000,
        600_000,
        16,
        Pos3Policy::Adaptive,
        false,
    )
    .unwrap();

    assert_eq!(shared.pos2.segments, pos2.segments);
    assert_eq!(shared.pos2.records, pos2.records);
    assert_eq!(shared.pos2.units, pos2.units);
    assert_eq!(shared.pos2.occurrences, pos2.occurrences);
    assert_eq!(shared.pos2.bytes, pos2.bytes);
    assert_eq!(shared.pos3.segments, pos3.segments);
    assert_eq!(shared.pos3.records, pos3.records);
    assert_eq!(shared.pos3.units, pos3.units);
    assert_eq!(shared.pos3.bytes, pos3.bytes);
    assert_eq!(shared.pos3.delta_records, pos3.delta_records);
    assert_eq!(shared.pos3.bitmap_records, pos3.bitmap_records);
    assert_eq!(shared.pos3.complement_records, pos3.complement_records);
    assert_eq!(shared.pos3.all_records, pos3.all_records);
    assert_eq!(shared.pos3.run_records, pos3.run_records);
    assert_eq!(shared.pos3.bp128_records, pos3.bp128_records);

    for segment in 0..16 {
        let name = format!("seg-{segment:05}");
        assert_eq!(
            fs::read(separate.path().join(format!("{name}.pos2"))).unwrap(),
            fs::read(combined.path().join(format!("{name}.pos2"))).unwrap(),
        );
        assert_eq!(
            fs::read(separate.path().join(format!("{name}.pos3"))).unwrap(),
            fs::read(combined.path().join(format!("{name}.pos3"))).unwrap(),
        );
    }
    verify_positional2_sidecars(combined.path()).unwrap();
    verify_positional3_sidecars(combined.path()).unwrap();

    let lazy = LazyPersistentIndex::open(combined.path()).unwrap();
    for query in [
        b"metadata ".as_slice(),
        b"metadata(".as_slice(),
        b"namespace".as_slice(),
        b"implementation".as_slice(),
    ] {
        assert_eq!(
            lazy.search_content(query).unwrap(),
            naive(&docs, query, false)
        );
    }
}

#[test]
fn explicit_disk_path_pipeline_reports_progress_inside_large_hydration_batch() {
    let corpus = TempDir::new("explicit-progress-corpus");
    let index_dir = TempDir::new("explicit-progress-index");
    let mut selected = Vec::new();
    for index in 0..300usize {
        let path = corpus.path().join(format!("file-{index:04}.txt"));
        fs::write(&path, format!("progress marker {index}")).unwrap();
        selected.push(DiskPathInput {
            display_path: format!("file-{index:04}.txt"),
            size_bytes: fs::metadata(&path).unwrap().len(),
            content_path: None,
            index_content: true,
            path,
        });
    }
    let cancel = AtomicBool::new(false);
    let build_options = BuildOptions {
        mode: BuildMode::Adaptive,
        segment_docs: 5_000,
        workers: 2,
    };
    let mut snapshots = Vec::new();
    let report = build_disk_path_inputs_index_pipelined(
        corpus.path(),
        selected,
        index_dir.path(),
        DiskPathBuildConfig {
            max_docs: None,
            max_file_bytes: 1024 * 1024,
            build: &build_options,
            scan_workers: 4,
            hydration_batch_bytes: 8 * 1024 * 1024,
            cancel: Some(&cancel),
        },
        |progress| snapshots.push(progress.clone()),
    )
    .unwrap();

    assert_eq!(report.processed_files, 300);
    assert_eq!(report.build.docs, 300);
    assert!(snapshots.iter().any(|progress| {
        progress.processed_files > 0 && progress.processed_files < report.source_files
    }));
    assert_eq!(
        snapshots.last().unwrap().processed_files,
        report.source_files
    );
}

#[test]
fn explicit_disk_path_pipeline_preserves_selected_order_and_progress() {
    let corpus = TempDir::new("explicit-path-corpus");
    let index_dir = TempDir::new("explicit-path-index");
    let fast_index_dir = TempDir::new("explicit-path-fast-index");
    fs::create_dir_all(corpus.path().join("nested")).unwrap();
    fs::write(corpus.path().join("zeta.txt"), "zeta unique").unwrap();
    fs::write(
        corpus.path().join("nested").join("alpha.txt"),
        "alpha timeout",
    )
    .unwrap();
    fs::write(
        corpus.path().join("oversize.txt"),
        vec![b'x'; 1024 * 1024 + 1],
    )
    .unwrap();
    fs::write(corpus.path().join("ignored.txt"), "ignored marker").unwrap();

    let selected = vec![
        corpus.path().join("nested").join("alpha.txt"),
        corpus.path().join("oversize.txt"),
        corpus.path().join("zeta.txt"),
    ];
    let cancel = AtomicBool::new(false);
    let mut snapshots = Vec::new();
    let build_options = BuildOptions {
        mode: BuildMode::Adaptive,
        segment_docs: 1,
        workers: 2,
    };
    let report = build_disk_paths_index_pipelined(
        corpus.path(),
        selected,
        index_dir.path(),
        DiskPathBuildConfig {
            max_docs: None,
            max_file_bytes: 1024 * 1024,
            build: &build_options,
            scan_workers: 2,
            hydration_batch_bytes: 8 * 1024 * 1024,
            cancel: Some(&cancel),
        },
        |progress| snapshots.push(progress.clone()),
    )
    .unwrap();

    let fast_selected = vec![
        DiskPathInput {
            path: corpus.path().join("nested").join("alpha.txt"),
            display_path: "nested/alpha.txt".to_owned(),
            size_bytes: fs::metadata(corpus.path().join("nested").join("alpha.txt"))
                .unwrap()
                .len(),
            content_path: None,
            index_content: true,
        },
        DiskPathInput {
            path: corpus.path().join("oversize.txt"),
            display_path: "oversize.txt".to_owned(),
            size_bytes: fs::metadata(corpus.path().join("oversize.txt"))
                .unwrap()
                .len(),
            content_path: None,
            index_content: true,
        },
        DiskPathInput {
            path: corpus.path().join("zeta.txt"),
            display_path: "zeta.txt".to_owned(),
            size_bytes: fs::metadata(corpus.path().join("zeta.txt")).unwrap().len(),
            content_path: None,
            index_content: true,
        },
    ];
    let fast_report = build_disk_path_inputs_index_pipelined(
        corpus.path(),
        fast_selected,
        fast_index_dir.path(),
        DiskPathBuildConfig {
            max_docs: None,
            max_file_bytes: 1024 * 1024,
            build: &build_options,
            scan_workers: 2,
            hydration_batch_bytes: 8 * 1024 * 1024,
            cancel: Some(&cancel),
        },
        |_| {},
    )
    .unwrap();

    assert_eq!(report.build.docs, 2);
    assert_eq!(report.display_paths, vec!["nested/alpha.txt", "zeta.txt"]);
    assert_eq!(report.source_indices, vec![0, 2]);
    assert_eq!(fast_report.display_paths, report.display_paths);
    assert_eq!(fast_report.source_indices, vec![0, 2]);
    assert_eq!(report.source_files, 3);
    assert_eq!(report.processed_files, 3);
    assert_eq!(report.skipped_files, 1);
    assert!(report.bytes_read >= "alpha timeout".len() as u64 + "zeta unique".len() as u64);
    assert!(
        snapshots
            .iter()
            .any(|progress| progress.processed_files == 3)
    );

    let index = LazyPersistentIndex::open(index_dir.path()).unwrap();
    assert_eq!(index.search_content(b"timeout").unwrap(), vec![0]);
    assert_eq!(index.search_name(b"zeta").unwrap(), vec![1]);
    assert!(index.search_content(b"ignored marker").unwrap().is_empty());

    let mut regular_files = fs::read_dir(index_dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    let mut fast_files = fs::read_dir(fast_index_dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    regular_files.sort();
    fast_files.sort();
    assert_eq!(regular_files, fast_files);
    for file in regular_files {
        assert_eq!(
            fs::read(index_dir.path().join(&file)).unwrap(),
            fs::read(fast_index_dir.path().join(&file)).unwrap(),
            "fast selected-path mismatch in {}",
            file.to_string_lossy()
        );
    }
}

#[test]
fn explicit_disk_path_pipeline_bounds_hydration_bytes_and_keeps_filename_only_docs() {
    let corpus = TempDir::new("explicit-byte-budget-corpus");
    let index_dir = TempDir::new("explicit-byte-budget-index");
    let one_mib = 1024 * 1024usize;
    let mut selected = Vec::new();
    for index in 0..6usize {
        let path = corpus.path().join(format!("text-{index}.txt"));
        let mut bytes = vec![b'a'; one_mib];
        bytes[..16].copy_from_slice(b"TEXT_MARKER_0000");
        fs::write(&path, bytes).unwrap();
        selected.push(DiskPathInput {
            path,
            display_path: format!("text-{index}.txt"),
            size_bytes: one_mib as u64,
            content_path: None,
            index_content: true,
        });
    }
    let image = corpus.path().join("photo.png");
    fs::write(&image, vec![b'Z'; 4 * one_mib]).unwrap();
    selected.insert(
        1,
        DiskPathInput {
            path: image,
            display_path: "photo.png".to_owned(),
            size_bytes: (4 * one_mib) as u64,
            content_path: None,
            index_content: false,
        },
    );

    let build_options = BuildOptions {
        mode: BuildMode::Adaptive,
        segment_docs: 64,
        workers: 2,
    };
    let cancel = AtomicBool::new(false);
    let mut snapshots = Vec::new();
    let report = build_disk_path_inputs_index_pipelined(
        corpus.path(),
        selected,
        index_dir.path(),
        DiskPathBuildConfig {
            max_docs: None,
            max_file_bytes: 8 * 1024 * 1024,
            build: &build_options,
            scan_workers: 4,
            hydration_batch_bytes: 2 * 1024 * 1024,
            cancel: Some(&cancel),
        },
        |progress| snapshots.push(progress.clone()),
    )
    .unwrap();

    assert_eq!(report.build.docs, 7);
    assert!(
        snapshots
            .iter()
            .all(|progress| { progress.prepared_bytes <= 2 * 1024 * 1024 })
    );
    assert!(snapshots.iter().any(|progress| progress.prepared_bytes > 0));
    let index = LazyPersistentIndex::open(index_dir.path()).unwrap();
    assert_eq!(index.search_name(b"photo").unwrap().len(), 1);
    assert!(index.search_content(b"zzzzzzzz").unwrap().is_empty());
    assert!(
        !index
            .search_content(b"text_marker_0000")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn adopted_prebuilt_index_preserves_physical_order_and_enables_incremental_generations() {
    let built = TempDir::new("adopt-built");
    let store = TempDir::new("adopt-store");
    // The generation root must not already contain CURRENT; TempDir itself may exist.
    let docs = vec![
        document(1, "alpha adoption marker".to_owned()),
        document(2, "beta stable marker".to_owned()),
        document(3, "gamma delta marker".to_owned()),
    ];
    build_index(
        &docs,
        built.path(),
        &BuildOptions {
            mode: BuildMode::Adaptive,
            segment_docs: 2,
            workers: 2,
        },
    )
    .unwrap();
    let identities = docs
        .iter()
        .enumerate()
        .map(|(index, doc)| {
            LogicalDocumentIdentity::new(
                index as u64 + 1,
                doc.key.clone(),
                doc.display_path.clone(),
            )
        })
        .collect::<Vec<_>>();
    let report =
        initialize_generation_from_built_index(store.path(), built.path(), &identities).unwrap();
    assert_eq!(report.generation, 0);
    assert_eq!(report.live_docs, 3);
    assert!(report.build.is_none());
    assert!(!built.path().exists());
    verify_generation(store.path()).unwrap();

    let merged = MergedIndex::open(store.path(), true).unwrap();
    assert_eq!(merged.search_content(b"adoption").unwrap(), vec![1]);
    assert_eq!(merged.search_name(b"module_0002").unwrap(), vec![2]);
}

#[test]
fn prepared_content_path_is_indexed_without_changing_original_display_path() {
    let corpus = TempDir::new("prepared-content-corpus");
    let index_dir = TempDir::new("prepared-content-index");
    let original = corpus.path().join("source.container");
    let prepared = corpus.path().join("source.prepared.txt");
    fs::write(&original, b"PK fake binary container").unwrap();
    fs::write(&prepared, "Generic Prepared Unique Marker").unwrap();
    let build_options = BuildOptions {
        mode: BuildMode::Adaptive,
        segment_docs: 10,
        workers: 1,
    };
    let report = build_disk_path_inputs_index_pipelined(
        corpus.path(),
        vec![DiskPathInput {
            path: original,
            display_path: "source.container".to_owned(),
            size_bytes: 24,
            content_path: Some(prepared),
            index_content: true,
        }],
        index_dir.path(),
        DiskPathBuildConfig {
            max_docs: None,
            max_file_bytes: 1024 * 1024,
            build: &build_options,
            scan_workers: 1,
            hydration_batch_bytes: 1024 * 1024,
            cancel: None,
        },
        |_| {},
    )
    .unwrap();
    assert_eq!(report.display_paths, vec!["source.container"]);
    let index = LazyPersistentIndex::open(index_dir.path()).unwrap();
    assert_eq!(index.search_name(b"source.container").unwrap(), vec![0]);
    assert_eq!(index.search_content(b"prepared unique").unwrap(), vec![0]);
    assert!(index.search_content(b"fake binary").unwrap().is_empty());
}

#[test]
fn unified_full_builder_is_byte_identical_to_legacy_sidecar_pipeline() {
    let docs = (0..2_000usize)
        .map(|id| {
            // Keep each segment above the unified frontier's sharding threshold so this byte
            // equivalence test covers the deterministic parallel merge, not only the fallback.
            let common =
                "common metadata configuration implementation return timeout namespace include "
                    .repeat(12);
            let body = match id % 4 {
                0 => format!("{common}alpha_{id}"),
                1 => format!("{common}beta_{id}"),
                2 => format!("{common}gamma_{id}"),
                _ => format!("{common}delta_{id}"),
            };
            document(id, body)
        })
        .collect::<Vec<_>>();
    let options = BuildOptions {
        mode: BuildMode::Direct,
        segment_docs: 500,
        workers: 2,
    };
    let legacy = TempDir::new("unified-legacy");
    let unified = TempDir::new("unified-new");

    build_index_benchmark(&docs, legacy.path(), &options).unwrap();
    build_q2_sidecars(legacy.path(), false).unwrap();
    build_positional_sidecars(legacy.path(), PosCodec::production(), 500_000, false).unwrap();
    build_positional23_sidecars(
        legacy.path(),
        500_000,
        500_000,
        500_000,
        16,
        Pos3Policy::Adaptive,
        false,
    )
    .unwrap();
    build_index_unified_benchmark(&docs, unified.path(), &options, AccelerationProfile::Full)
        .unwrap();

    let mut legacy_names = fs::read_dir(legacy.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let mut unified_names = fs::read_dir(unified.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    legacy_names.sort();
    unified_names.sort();
    assert_eq!(legacy_names, unified_names);
    for name in legacy_names {
        assert_eq!(
            fs::read(legacy.path().join(&name)).unwrap(),
            fs::read(unified.path().join(&name)).unwrap(),
            "unified full-build bytes differ for {name}",
        );
    }
}

#[test]
fn unified_adaptive_delta_and_balanced_compaction_match_full_rebuild_semantics() {
    let store = TempDir::new("unified-delta-store");
    let options = BuildOptions {
        mode: BuildMode::Direct,
        segment_docs: 128,
        workers: 2,
    };
    let mut base_docs = Vec::new();
    let mut catalog = CatalogSnapshot {
        generation: 0,
        next_logical_id: 301,
        ..CatalogSnapshot::default()
    };
    for id in 1..=300u64 {
        let key = format!("u-{id:04}");
        let mut doc = document(
            id as usize,
            format!("common metadata return timeout original_{id}"),
        );
        doc.key = key.clone();
        doc.display_path = key.clone();
        catalog.live.insert(
            key.clone(),
            CatalogEntry {
                logical_id: id,
                key,
                last_generation: 0,
            },
        );
        base_docs.push(LogicalDocument::new(id, doc));
    }
    initialize_generation(store.path(), &base_docs, &options).unwrap();

    let changes = (1..=100u64)
        .map(|id| {
            let key = format!("u-{id:04}");
            let body = format!("common metadata updated unified_delta_marker_{id} namespace");
            DocumentChange {
                kind: ChangeKind::Upsert,
                key: key.clone(),
                document: Some(DocumentInput::new(
                    key.clone(),
                    key,
                    fold_ascii(format!("renamed_{id}.rs").as_bytes()),
                    fold_ascii(body.as_bytes()),
                )),
            }
        })
        .collect::<Vec<_>>();
    let batch = ChangeBatch {
        expected_base_generation: 0,
        changes,
    };
    let plan = plan_incremental_update(&catalog, &batch, IncrementalPolicy::default()).unwrap();
    let report = publish_incremental_update_unified(store.path(), &plan, &options).unwrap();
    assert_eq!(report.generation, 1);
    verify_generation(store.path()).unwrap();
    let merged = MergedIndex::open(store.path(), true).unwrap();
    assert_eq!(
        merged.search_content(b"unified_delta_marker_42").unwrap(),
        vec![42]
    );

    let compacted = compact_generation_unified(store.path(), &options).unwrap();
    assert!(compacted.compacted);
    assert_eq!(compacted.delta_count, 0);
    verify_generation(store.path()).unwrap();
    let after = MergedIndex::open(store.path(), true).unwrap();
    assert_eq!(
        after.search_content(b"unified_delta_marker_42").unwrap(),
        vec![42]
    );

    let current = fs::read_to_string(store.path().join("CURRENT")).unwrap();
    let manifest_rel = current
        .lines()
        .find_map(|line| line.strip_prefix("manifest "))
        .unwrap();
    let manifest = fs::read_to_string(store.path().join(manifest_rel)).unwrap();
    let base_rel = manifest
        .lines()
        .find_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            (fields.first() == Some(&"source"))
                .then(|| fields.get(3).copied())
                .flatten()
        })
        .unwrap();
    let compacted_dir = store.path().join(base_rel);
    assert!(
        fs::read_dir(&compacted_dir).unwrap().any(|e| e
            .unwrap()
            .path()
            .extension()
            .is_some_and(|x| x == "q2c"))
    );
    assert!(!fs::read_dir(&compacted_dir).unwrap().any(|e| {
        e.unwrap()
            .path()
            .extension()
            .is_some_and(|x| matches!(x.to_str(), Some("pos1" | "pos2" | "pos3")))
    }));
}

#[test]
fn balanced_acceleration_is_exact_keeps_q2_and_skips_positional_sidecars() {
    let full = TempDir::new("balanced-profile-full");
    let balanced = TempDir::new("balanced-profile-balanced");
    let options = BuildOptions {
        mode: BuildMode::Adaptive,
        segment_docs: 128,
        workers: 4,
    };
    let docs = (0..256usize)
        .map(|id| {
            document(
                id,
                format!(
                    "common timeout metadata balanced marker_{id} {} 日本語検索",
                    if id % 17 == 0 {
                        "rare_path"
                    } else {
                        "ordinary"
                    }
                ),
            )
        })
        .collect::<Vec<_>>();
    build_index_unified_benchmark(&docs, full.path(), &options, AccelerationProfile::Full).unwrap();
    build_index_unified_benchmark(
        &docs,
        balanced.path(),
        &options,
        AccelerationProfile::Balanced,
    )
    .unwrap();

    let full_index = PersistentIndex::open(full.path(), true).unwrap();
    let balanced_index = PersistentIndex::open(balanced.path(), true).unwrap();
    for query in [
        b"e".as_slice(),
        b"ti".as_slice(),
        b"timeout".as_slice(),
        b"rare_path".as_slice(),
        b"marker_170".as_slice(),
        "日本語検索".as_bytes(),
        b"zzzz-no-hit".as_slice(),
    ] {
        assert_eq!(
            balanced_index.search_content(query).unwrap(),
            full_index.search_content(query).unwrap(),
            "Balanced profile changed exact results for {:?}",
            String::from_utf8_lossy(query),
        );
    }

    let entries = fs::read_dir(balanced.path())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect::<Vec<_>>();
    assert!(
        entries
            .iter()
            .any(|path| path.extension().is_some_and(|x| x == "q2c"))
    );
    assert!(!entries.iter().any(|path| {
        path.extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| matches!(x, "pos1" | "pos2" | "pos3"))
    }));
}

#[test]
fn adaptive_single_document_delta_stays_base_only_even_for_large_content() {
    let store = TempDir::new("adaptive-single-delta-store");
    let options = BuildOptions {
        mode: BuildMode::Direct,
        segment_docs: 128,
        workers: 2,
    };
    let mut base = document(0, "original small body".to_owned());
    base.key = "only-doc".to_owned();
    base.display_path = "only-doc".to_owned();
    initialize_generation(store.path(), &[LogicalDocument::new(1, base)], &options).unwrap();

    let marker = "adaptive_single_delta_marker";
    let mut large_body = marker.repeat(180_000);
    large_body.push_str(" final");
    let replacement = DocumentInput::new(
        "only-doc".to_owned(),
        "only-doc".to_owned(),
        fold_ascii(b"only-doc"),
        fold_ascii(large_body.as_bytes()),
    );
    let mut catalog = CatalogSnapshot {
        generation: 0,
        next_logical_id: 2,
        ..CatalogSnapshot::default()
    };
    catalog.live.insert(
        "only-doc".to_owned(),
        CatalogEntry {
            logical_id: 1,
            key: "only-doc".to_owned(),
            last_generation: 0,
        },
    );
    let batch = ChangeBatch {
        expected_base_generation: 0,
        changes: vec![DocumentChange {
            kind: ChangeKind::Upsert,
            key: "only-doc".to_owned(),
            document: Some(replacement),
        }],
    };
    let plan = plan_incremental_update(&catalog, &batch, IncrementalPolicy::default()).unwrap();
    publish_incremental_update_unified(store.path(), &plan, &options).unwrap();
    verify_generation(store.path()).unwrap();

    let current = fs::read_to_string(store.path().join("CURRENT")).unwrap();
    let manifest_rel = current
        .lines()
        .find_map(|line| line.strip_prefix("manifest "))
        .unwrap();
    let manifest = fs::read_to_string(store.path().join(manifest_rel)).unwrap();
    let delta_rel = manifest
        .lines()
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            (fields.get(1) == Some(&"delta"))
                .then(|| fields.get(3).copied())
                .flatten()
        })
        .next_back()
        .unwrap();
    let delta_dir = store.path().join(delta_rel);
    let extensions = fs::read_dir(&delta_dir)
        .unwrap()
        .map(|entry| {
            entry
                .unwrap()
                .path()
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("")
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert!(!extensions.iter().any(|ext| ext == "q2c"));
    assert!(!extensions.iter().any(|ext| ext == "pos2" || ext == "pos3"));
    assert!(!extensions.iter().any(|ext| ext.starts_with("pos-")));

    let merged = MergedIndex::open(store.path(), true).unwrap();
    assert_eq!(merged.search_content(marker.as_bytes()).unwrap(), vec![1]);
}
