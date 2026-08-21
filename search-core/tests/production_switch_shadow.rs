use std::{
    collections::HashMap,
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use personalrag_portable_search::{
    BuildMode, BuildOptions, CatalogEntry, CatalogSnapshot, ChangeBatch, ChangeKind,
    DocumentChange, DocumentInput, IncrementalPolicy, LogicalDocument, MergedIndex,
    VNextDocumentInput, apply_update_plan, fold_ascii, initialize_generation,
    initialize_vnext_generation_store, open_vnext_published_generation, plan_incremental_update,
    publish_incremental_update, publish_vnext_incremental_generation,
};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn temp_root(label: &str) -> PathBuf {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "personalrag-production-switch-{label}-{}-{id}",
        std::process::id()
    ))
}

fn input(path: &str, content: &str) -> DocumentInput {
    DocumentInput::new(
        path,
        path,
        fold_ascii(path.as_bytes()),
        fold_ascii(content.as_bytes()),
    )
}

fn vdoc(id: u64, path: &str, content: &str) -> VNextDocumentInput {
    VNextDocumentInput::new(id, path, fold_ascii(content.as_bytes()))
}

fn assert_same_queries(
    perf: &MergedIndex,
    vnext: &personalrag_portable_search::VNextGenerationIndex,
) {
    for query in [
        b"alpha".as_slice(),
        b"shared marker",
        b"updated",
        b"no-such-value",
    ] {
        assert_eq!(
            perf.search_content(query).unwrap(),
            vnext.search_content(query).unwrap()
        );
    }
    for query in [b"docs/".as_slice(), b"gamma.txt"] {
        assert_eq!(
            perf.search_name(query).unwrap(),
            vnext.search_name(query).unwrap()
        );
    }
}

#[test]
fn perf12_and_vnext_shadow_stay_equivalent_across_incremental_publish() {
    let root = temp_root("incremental");
    let perf = root.join("perf12");
    let vnext = root.join("vnext");

    let docs = vec![
        LogicalDocument::new(1, input("docs/alpha.txt", "alpha shared marker")),
        LogicalDocument::new(2, input("docs/beta.txt", "beta shared marker")),
        LogicalDocument::new(3, input("docs/gamma.txt", "gamma payload")),
    ];
    initialize_generation(
        &perf,
        &docs,
        &BuildOptions {
            mode: BuildMode::Adaptive,
            segment_docs: 5_000,
            workers: 2,
        },
    )
    .unwrap();
    initialize_vnext_generation_store(
        &vnext,
        &[
            vdoc(1, "docs/alpha.txt", "alpha shared marker"),
            vdoc(2, "docs/beta.txt", "beta shared marker"),
            vdoc(3, "docs/gamma.txt", "gamma payload"),
        ],
        5_000,
    )
    .unwrap();

    let perf_open = MergedIndex::open(&perf, true).unwrap();
    let vnext_open = open_vnext_published_generation(&vnext).unwrap();
    assert_same_queries(&perf_open, &vnext_open);
    drop(perf_open);
    drop(vnext_open);

    let live = HashMap::from([
        (
            "docs/alpha.txt".to_owned(),
            CatalogEntry {
                logical_id: 1,
                key: "docs/alpha.txt".into(),
                last_generation: 0,
            },
        ),
        (
            "docs/beta.txt".to_owned(),
            CatalogEntry {
                logical_id: 2,
                key: "docs/beta.txt".into(),
                last_generation: 0,
            },
        ),
        (
            "docs/gamma.txt".to_owned(),
            CatalogEntry {
                logical_id: 3,
                key: "docs/gamma.txt".into(),
                last_generation: 0,
            },
        ),
    ]);
    let snapshot = CatalogSnapshot {
        generation: 0,
        next_logical_id: 4,
        live,
    };
    let batch = ChangeBatch {
        expected_base_generation: 0,
        changes: vec![
            DocumentChange {
                kind: ChangeKind::Delete,
                key: "docs/beta.txt".into(),
                document: None,
            },
            DocumentChange {
                kind: ChangeKind::Upsert,
                key: "docs/alpha.txt".into(),
                document: Some(input("docs/alpha.txt", "alpha updated shared marker")),
            },
            DocumentChange {
                kind: ChangeKind::Upsert,
                key: "docs/delta.txt".into(),
                document: Some(input("docs/delta.txt", "delta payload")),
            },
        ],
    };
    let plan = plan_incremental_update(&snapshot, &batch, IncrementalPolicy::default()).unwrap();
    let _next = apply_update_plan(&snapshot, &plan).unwrap();
    publish_incremental_update(
        &perf,
        &plan,
        &BuildOptions {
            mode: BuildMode::Adaptive,
            segment_docs: 5_000,
            workers: 2,
        },
    )
    .unwrap();
    publish_vnext_incremental_generation(&vnext, &plan, 5_000).unwrap();

    let perf_open = MergedIndex::open(&perf, true).unwrap();
    let vnext_open = open_vnext_published_generation(&vnext).unwrap();
    assert_eq!(perf_open.generation(), vnext_open.generation());
    assert_eq!(perf_open.live_docs(), vnext_open.live_docs());
    assert_same_queries(&perf_open, &vnext_open);
    assert!(perf_open.search_content(b"beta shared").unwrap().is_empty());
    assert!(
        vnext_open
            .search_content(b"beta shared")
            .unwrap()
            .is_empty()
    );

    fs::remove_dir_all(root).unwrap();
}
