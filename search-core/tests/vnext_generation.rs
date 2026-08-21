use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use personalrag_portable_search::{
    VNextDocumentInput, VNextGenerationIndex, VNextGenerationLayerKind, VNextGenerationLayerSpec,
    VNextSegmentReader, fold_ascii, write_vnext_segment_with_block_size,
};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn temp_root(label: &str) -> PathBuf {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "personalrag-vnext-generation-{label}-{}-{id}",
        std::process::id()
    ))
}

fn doc(id: u64, path: &str, content: &str) -> VNextDocumentInput {
    VNextDocumentInput::new(id, path, fold_ascii(content.as_bytes()))
}

fn write_segment(path: &Path, docs: &[VNextDocumentInput]) {
    write_vnext_segment_with_block_size(path, docs, 8).unwrap();
}

fn logical_hits(reader: &VNextSegmentReader, local: Vec<u16>) -> Vec<u64> {
    let mut out = local
        .into_iter()
        .map(|doc_id| reader.logical_id(doc_id).unwrap())
        .collect::<Vec<_>>();
    out.sort_unstable();
    out.dedup();
    out
}

fn naive_content(live: &BTreeMap<u64, VNextDocumentInput>, query: &[u8]) -> Vec<u64> {
    let query = fold_ascii(query);
    if query.is_empty() {
        return Vec::new();
    }
    live.iter()
        .filter_map(|(&id, item)| {
            item.normalized_content
                .windows(query.len())
                .any(|window| window == query)
                .then_some(id)
        })
        .collect()
}

fn naive_path(live: &BTreeMap<u64, VNextDocumentInput>, query: &[u8]) -> Vec<u64> {
    let query = fold_ascii(query);
    if query.is_empty() {
        return Vec::new();
    }
    live.iter()
        .filter_map(|(&id, item)| {
            fold_ascii(item.display_path.as_bytes())
                .windows(query.len())
                .any(|window| window == query)
                .then_some(id)
        })
        .collect()
}

#[test]
fn vnext_generation_newest_upsert_hides_old_physical_version_and_rename() {
    let root = temp_root("newest");
    fs::create_dir_all(&root).unwrap();
    let base = root.join("base.prseg2");
    let delta = root.join("delta.prseg2");
    write_segment(
        &base,
        &[
            doc(1, "old/alpha.txt", "old alpha payload"),
            doc(2, "stable/beta.txt", "stable beta payload"),
        ],
    );
    write_segment(
        &delta,
        &[
            doc(1, "new/renamed-alpha.txt", "new gamma payload"),
            doc(3, "new/charlie.txt", "charlie payload"),
        ],
    );

    let generation = VNextGenerationIndex::open(
        1,
        &[
            VNextGenerationLayerSpec::base(0, [&base]),
            VNextGenerationLayerSpec::delta(1, [&delta], vec![1]),
        ],
    )
    .unwrap();

    assert_eq!(generation.live_docs(), 3);
    assert_eq!(generation.live_logical_ids(), &[1, 2, 3]);
    assert!(generation.search_content(b"old alpha").unwrap().is_empty());
    assert_eq!(generation.search_content(b"new gamma").unwrap(), vec![1]);
    assert!(generation.search_path(b"old/alpha").unwrap().is_empty());
    assert_eq!(generation.search_path(b"RENAMED-ALPHA").unwrap(), vec![1]);
    assert_eq!(generation.search_content(b"stable beta").unwrap(), vec![2]);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_generation_tombstone_only_delta_deletes_live_document() {
    let root = temp_root("delete");
    fs::create_dir_all(&root).unwrap();
    let base = root.join("base.prseg2");
    write_segment(
        &base,
        &[
            doc(1, "a.txt", "keep alpha"),
            doc(2, "b.txt", "delete beta marker"),
            doc(3, "c.txt", "keep gamma"),
        ],
    );

    let generation = VNextGenerationIndex::open(
        1,
        &[
            VNextGenerationLayerSpec::base(0, [&base]),
            VNextGenerationLayerSpec::delta(1, std::iter::empty::<&PathBuf>(), vec![2, 999]),
        ],
    )
    .unwrap();

    assert_eq!(generation.live_logical_ids(), &[1, 3]);
    assert_eq!(generation.tombstone_events(), 2);
    assert!(!generation.contains_logical_id(2));
    assert!(
        generation
            .search_content(b"delete beta")
            .unwrap()
            .is_empty()
    );
    assert!(generation.search_path(b"b.txt").unwrap().is_empty());

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_generation_one_layer_can_span_multiple_local_id_segments() {
    let root = temp_root("multi-segment");
    fs::create_dir_all(&root).unwrap();
    let a = root.join("base-a.prseg2");
    let b = root.join("base-b.prseg2");
    write_segment(
        &a,
        &[
            doc(10, "a/ten.txt", "shared marker ten"),
            doc(20, "a/twenty.txt", "shared marker twenty"),
        ],
    );
    write_segment(
        &b,
        &[
            doc(30, "b/thirty.txt", "shared marker thirty"),
            doc(40, "b/forty.txt", "shared marker forty"),
        ],
    );

    let generation =
        VNextGenerationIndex::open(7, &[VNextGenerationLayerSpec::base(7, [&a, &b])]).unwrap();
    assert_eq!(generation.layer_count(), 1);
    assert_eq!(generation.segment_count(), 2);
    assert_eq!(
        generation.search_content(b"shared marker").unwrap(),
        vec![10, 20, 30, 40]
    );
    assert_eq!(generation.search_path(b"b/").unwrap(), vec![30, 40]);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_generation_matches_full_rebuild_and_naive_oracle_after_updates_and_deletes() {
    let root = temp_root("oracle");
    fs::create_dir_all(&root).unwrap();
    let base_a = root.join("base-a.prseg2");
    let base_b = root.join("base-b.prseg2");
    let delta1 = root.join("delta1.prseg2");
    let delta2 = root.join("delta2.prseg2");
    let full = root.join("full.prseg2");

    let base_docs = (1..=20u64)
        .map(|id| {
            doc(
                id,
                &format!("base/group_{}/doc_{id:03}.txt", id % 4),
                &format!("common base payload id{id:03}"),
            )
        })
        .collect::<Vec<_>>();
    write_segment(&base_a, &base_docs[..10]);
    write_segment(&base_b, &base_docs[10..]);

    let delta1_docs = vec![
        doc(3, "updated/doc_003.txt", "common updated-three 日本語"),
        doc(5, "updated/doc_005.txt", "common updated-five marker"),
        doc(21, "new/doc_021.txt", "common newly-added twenty-one"),
    ];
    write_segment(&delta1, &delta1_docs);

    let delta2_docs = vec![
        doc(5, "final/doc_005.txt", "common final-five needle-xyz"),
        doc(
            22,
            "new/doc_022.txt",
            "common newly-added twenty-two needle-xyz",
        ),
    ];
    write_segment(&delta2, &delta2_docs);

    let layers = [
        VNextGenerationLayerSpec::base(0, [&base_a, &base_b]),
        VNextGenerationLayerSpec::delta(1, [&delta1], vec![3, 5, 8]),
        VNextGenerationLayerSpec::delta(2, [&delta2], vec![2, 5, 21]),
    ];
    let generation = VNextGenerationIndex::open(2, &layers).unwrap();

    let mut live = base_docs
        .into_iter()
        .map(|item| (item.logical_id, item))
        .collect::<BTreeMap<_, _>>();
    for id in [3, 5, 8] {
        live.remove(&id);
    }
    for item in delta1_docs {
        live.insert(item.logical_id, item);
    }
    for id in [2, 5, 21] {
        live.remove(&id);
    }
    for item in delta2_docs {
        live.insert(item.logical_id, item);
    }

    let materialized = generation.materialize_live_documents().unwrap();
    assert_eq!(
        materialized
            .iter()
            .map(|item| item.logical_id)
            .collect::<Vec<_>>(),
        live.keys().copied().collect::<Vec<_>>()
    );
    write_segment(&full, &materialized);
    let full_reader = VNextSegmentReader::open(&full).unwrap();

    for query in [
        b"c".as_slice(),
        b"co",
        b"common",
        b"base payload",
        b"updated-three",
        "日本語".as_bytes(),
        b"final-five",
        b"needle-xyz",
        b"newly-added",
        b"definitely-missing",
    ] {
        let expected = naive_content(&live, query);
        assert_eq!(
            generation.search_content(query).unwrap(),
            expected,
            "generation content {query:?}"
        );
        assert_eq!(
            logical_hits(&full_reader, full_reader.search_content(query).unwrap()),
            expected,
            "full content {query:?}"
        );
    }

    for query in [
        b"doc_005".as_slice(),
        b"base/group_",
        b"UPDATED",
        b"final/doc_005",
        b"new/doc_022",
        b"doc_021",
        b"missing-path",
    ] {
        let expected = naive_path(&live, query);
        assert_eq!(
            generation.search_path(query).unwrap(),
            expected,
            "generation path {query:?}"
        );
        assert_eq!(
            logical_hits(&full_reader, full_reader.search_path(query).unwrap()),
            expected,
            "full path {query:?}"
        );
    }

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_generation_rejects_ambiguous_or_malformed_layer_semantics() {
    let root = temp_root("invalid");
    fs::create_dir_all(&root).unwrap();
    let dup_a = root.join("dup-a.prseg2");
    let dup_b = root.join("dup-b.prseg2");
    let zero = root.join("zero.prseg2");
    write_segment(&dup_a, &[doc(7, "a.txt", "a")]);
    write_segment(&dup_b, &[doc(7, "b.txt", "b")]);
    write_segment(&zero, &[doc(0, "zero.txt", "zero")]);

    assert!(
        VNextGenerationIndex::open(0, &[VNextGenerationLayerSpec::base(0, [&dup_a, &dup_b])])
            .is_err()
    );
    assert!(VNextGenerationIndex::open(0, &[VNextGenerationLayerSpec::base(0, [&zero])]).is_err());
    assert!(
        VNextGenerationIndex::open(
            1,
            &[
                VNextGenerationLayerSpec::base(1, [&dup_a]),
                VNextGenerationLayerSpec::delta(1, std::iter::empty::<&PathBuf>(), vec![]),
            ]
        )
        .is_err()
    );
    assert!(
        VNextGenerationIndex::open(
            2,
            &[
                VNextGenerationLayerSpec::base(0, [&dup_a]),
                VNextGenerationLayerSpec {
                    kind: VNextGenerationLayerKind::Delta,
                    generation: 2,
                    segment_paths: Vec::new(),
                    tombstones: vec![9, 8],
                },
            ]
        )
        .is_err()
    );
    assert!(
        VNextGenerationIndex::open(
            3,
            &[
                VNextGenerationLayerSpec::base(0, [&dup_a]),
                VNextGenerationLayerSpec::delta(2, std::iter::empty::<&PathBuf>(), vec![]),
            ]
        )
        .is_err()
    );

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_generation_matches_perf12_generation_newest_wins_and_tombstones() {
    use personalrag_portable_search::{
        BuildOptions, DocumentInput, LogicalDocument, MergedIndex, PlannedUpsert, UpdatePlan,
        initialize_generation, publish_incremental_update,
    };

    fn perf_doc(path: &str, content: &str) -> DocumentInput {
        DocumentInput::new(
            path,
            path,
            fold_ascii(path.as_bytes()),
            fold_ascii(content.as_bytes()),
        )
    }

    let root = temp_root("perf12-generation");
    let perf_root = root.join("perf12-generation");
    fs::create_dir_all(&root).unwrap();
    let vbase = root.join("vbase.prseg2");
    let vdelta = root.join("vdelta.prseg2");

    let base = [
        (1, "old/alpha.txt", "old alpha payload common"),
        (2, "old/beta.txt", "beta delete-me common"),
        (3, "stable/gamma.txt", "gamma stable common"),
    ];
    let base_vnext = base
        .iter()
        .map(|&(id, path, content)| doc(id, path, content))
        .collect::<Vec<_>>();
    write_segment(&vbase, &base_vnext);
    let base_perf = base
        .iter()
        .map(|&(id, path, content)| LogicalDocument::new(id, perf_doc(path, content)))
        .collect::<Vec<_>>();
    initialize_generation(&perf_root, &base_perf, &BuildOptions::default()).unwrap();

    let delta = [
        (
            1,
            "new/renamed-alpha.txt",
            "new alpha replacement needle common",
        ),
        (4, "new/delta.txt", "delta insert needle common"),
    ];
    let delta_vnext = delta
        .iter()
        .map(|&(id, path, content)| doc(id, path, content))
        .collect::<Vec<_>>();
    write_segment(&vdelta, &delta_vnext);
    let plan = UpdatePlan {
        base_generation: 0,
        next_generation: 1,
        upserts: delta
            .iter()
            .map(|&(id, path, content)| PlannedUpsert {
                logical_id: id,
                is_insert: id == 4,
                document: perf_doc(path, content),
            })
            .collect(),
        tombstones: vec![1, 2],
        live_docs_after: 3,
        compaction_recommended: false,
    };
    publish_incremental_update(&perf_root, &plan, &BuildOptions::default()).unwrap();
    let perf = MergedIndex::open(&perf_root, true).unwrap();

    let vnext = VNextGenerationIndex::open(
        1,
        &[
            VNextGenerationLayerSpec::base(0, [&vbase]),
            VNextGenerationLayerSpec::delta(1, [&vdelta], vec![1, 2]),
        ],
    )
    .unwrap();

    for query in [
        b"old alpha".as_slice(),
        b"new alpha",
        b"delete-me",
        b"gamma stable",
        b"needle",
        b"common",
        b"missing",
    ] {
        assert_eq!(
            vnext.search_content(query).unwrap(),
            perf.search_content(query).unwrap(),
            "content query={:?}",
            String::from_utf8_lossy(query)
        );
    }
    for query in [
        b"old/alpha".as_slice(),
        b"renamed-alpha",
        b"old/beta",
        b"stable/gamma",
        b"new/delta",
        b"missing",
    ] {
        assert_eq!(
            vnext.search_path(query).unwrap(),
            perf.search_name(query).unwrap(),
            "path query={:?}",
            String::from_utf8_lossy(query)
        );
    }

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_generation_newest_wins_even_without_redundant_update_tombstone() {
    let root = temp_root("newest-no-tombstone");
    fs::create_dir_all(&root).unwrap();
    let base = root.join("base.prseg2");
    let delta1 = root.join("delta1.prseg2");
    let delta2 = root.join("delta2.prseg2");
    write_segment(&base, &[doc(77, "v0/item.txt", "version-zero-only")]);
    write_segment(&delta1, &[doc(77, "v1/item.txt", "version-one-only")]);
    write_segment(&delta2, &[doc(77, "v2/item.txt", "version-two-only")]);

    let generation = VNextGenerationIndex::open(
        2,
        &[
            VNextGenerationLayerSpec::base(0, [&base]),
            VNextGenerationLayerSpec::delta(1, [&delta1], vec![]),
            VNextGenerationLayerSpec::delta(2, [&delta2], vec![]),
        ],
    )
    .unwrap();

    assert_eq!(generation.live_logical_ids(), &[77]);
    assert!(
        generation
            .search_content(b"version-zero-only")
            .unwrap()
            .is_empty()
    );
    assert!(
        generation
            .search_content(b"version-one-only")
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        generation.search_content(b"version-two-only").unwrap(),
        vec![77]
    );
    assert!(generation.search_path(b"v0/item").unwrap().is_empty());
    assert!(generation.search_path(b"v1/item").unwrap().is_empty());
    assert_eq!(generation.search_path(b"v2/item").unwrap(), vec![77]);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_generation_high_hit_split_jobs_filter_hidden_versions_exactly() {
    let root = temp_root("high-hit-split-hidden");
    fs::create_dir_all(&root).unwrap();
    let base_a = root.join("base-a.prseg2");
    let base_b = root.join("base-b.prseg2");
    let delta = root.join("delta.prseg2");

    let base_docs = (1u64..=10_000)
        .map(|id| {
            VNextDocumentInput::new(
                id,
                format!("base/doc_{id:05}.txt"),
                fold_ascii(format!("timeout common base payload {id:05}").as_bytes()),
            )
        })
        .collect::<Vec<_>>();
    write_vnext_segment_with_block_size(&base_a, &base_docs[..9_000], 8192).unwrap();
    write_vnext_segment_with_block_size(&base_b, &base_docs[9_000..], 8192).unwrap();

    let replacements = (1u64..=100)
        .map(|id| {
            VNextDocumentInput::new(
                id,
                format!("updated/doc_{id:05}.txt"),
                fold_ascii(format!("replacement payload without old marker {id:05}").as_bytes()),
            )
        })
        .collect::<Vec<_>>();
    write_vnext_segment_with_block_size(&delta, &replacements, 8192).unwrap();

    let index = VNextGenerationIndex::open(
        1,
        &[
            VNextGenerationLayerSpec::base(0, [&base_a, &base_b]),
            VNextGenerationLayerSpec::delta(1, [&delta], (1u64..=100).collect()),
        ],
    )
    .unwrap();
    let expected = (101u64..=10_000).collect::<Vec<_>>();
    assert_eq!(index.search_content(b"timeout common").unwrap(), expected);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_generation_global_high_hit_threshold_spans_small_segments() {
    let root = temp_root("global-high-hit");
    fs::create_dir_all(&root).unwrap();
    let mut paths = Vec::new();
    let mut expected = Vec::new();
    let mut next_id = 1u64;
    for segment_no in 0..4usize {
        let path = root.join(format!("base-{segment_no}.prseg2"));
        let mut docs = Vec::new();
        for local in 0..2050usize {
            let id = next_id;
            next_id += 1;
            docs.push(VNextDocumentInput::new(
                id,
                format!("src/s{segment_no}/doc_{local:04}.txt"),
                fold_ascii(format!("prefix timeout suffix {id}").as_bytes()),
            ));
            expected.push(id);
        }
        write_vnext_segment_with_block_size(&path, &docs, 8192).unwrap();
        paths.push(path);
    }
    // Every individual segment has only 2050 candidates (<8192), but the generation has 8200.
    let index = VNextGenerationIndex::open(0, &[VNextGenerationLayerSpec::base(0, paths)]).unwrap();
    assert_eq!(index.segment_count(), 4);
    assert_eq!(index.search_content(b"timeout").unwrap(), expected);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_generation_adaptive_first_n_stops_in_requested_row_order() {
    let root = temp_root("adaptive-first-n");
    fs::create_dir_all(&root).unwrap();
    let mut paths = Vec::new();
    let mut all_ids = Vec::new();
    let mut next_id = 1u64;
    for segment_no in 0..2usize {
        let path = root.join(format!("base-{segment_no}.prseg2"));
        let mut docs = Vec::new();
        for local in 0..4200usize {
            let id = next_id;
            next_id += 1;
            docs.push(VNextDocumentInput::new(
                id,
                format!("src/s{segment_no}/timeout_{local:04}.txt"),
                fold_ascii(format!("prefix timeout suffix {id}").as_bytes()),
            ));
            all_ids.push(id);
        }
        write_vnext_segment_with_block_size(&path, &docs, 8192).unwrap();
        paths.push(path);
    }
    let index = VNextGenerationIndex::open(0, &[VNextGenerationLayerSpec::base(0, paths)]).unwrap();
    let mut row_order = all_ids.clone();
    row_order.reverse();
    let first = index
        .first_n_in_order(b"timeout", false, &row_order, 37)
        .unwrap();
    assert_eq!(first, row_order[..37]);
    let names = index
        .first_n_in_order(b"timeout_", true, &row_order, 23)
        .unwrap();
    assert_eq!(names, row_order[..23]);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_generation_dense_blob_scan_keeps_exact_document_boundaries() {
    let root = temp_root("dense-blob-scan");
    fs::create_dir_all(&root).unwrap();
    let mut paths = Vec::new();
    let mut expected = Vec::new();
    let mut next_id = 1u64;
    for segment_no in 0..2usize {
        let path = root.join(format!("base-{segment_no}.prseg2"));
        let mut docs = Vec::new();
        for local in 0..4200usize {
            let id = next_id;
            next_id += 1;
            // Both q3 anchors `abc` and `bcd` are present in every document, forcing the
            // generation-wide dense scan. Only one quarter contains the exact `abcd` literal.
            let content = if id.is_multiple_of(4) {
                expected.push(id);
                format!("prefix abcd suffix {id}")
            } else {
                format!("prefix abcXbcd suffix {id}")
            };
            docs.push(VNextDocumentInput::new(
                id,
                format!("src/s{segment_no}/doc_{local:04}.txt"),
                fold_ascii(content.as_bytes()),
            ));
        }
        write_vnext_segment_with_block_size(&path, &docs, 8192).unwrap();
        paths.push(path);
    }
    let index = VNextGenerationIndex::open(0, &[VNextGenerationLayerSpec::base(0, paths)]).unwrap();
    assert_eq!(index.search_content(b"abcd").unwrap(), expected);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_generation_conjunctive_first_n_matches_exact_row_order() {
    let root = temp_root("conjunctive-first-n");
    fs::create_dir_all(&root).unwrap();
    let base = root.join("base.prseg2");
    let docs = (1..=240u64)
        .map(|id| {
            let group = if id % 3 == 0 { "keep" } else { "other" };
            let content = if id % 2 == 0 {
                format!("dense timeout content logical {id}")
            } else {
                format!("plain content logical {id}")
            };
            doc(id, &format!("root/{group}/doc_{id:03}.txt"), &content)
        })
        .collect::<Vec<_>>();
    write_segment(&base, &docs);
    let generation =
        VNextGenerationIndex::open(0, &[VNextGenerationLayerSpec::base(0, [&base])]).unwrap();

    let mut order = generation.live_logical_ids().to_vec();
    order.reverse();
    let expected = order
        .iter()
        .copied()
        .filter(|id| id % 6 == 0)
        .take(17)
        .collect::<Vec<_>>();
    let actual = generation
        .first_n_conjunctive_in_order(b"/keep/", b"timeout", &order, 17)
        .unwrap();
    assert_eq!(actual, expected);

    fs::remove_dir_all(root).unwrap();
}
