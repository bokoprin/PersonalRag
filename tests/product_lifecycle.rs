use personalrag_v2::extraction::ExtractorConfig;
use personalrag_v2::gui::{GuiSearchRequest, GuiSearchSession};
use personalrag_v2::product::{initialize_store, reconcile_store};
use personalrag_v2::usn::UsnCheckpoint;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_dir(tag: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "personalrag-product-e2e-{tag}-{}-{stamp}",
        std::process::id()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn search(session: &GuiSearchSession, file: &str, content: &str) -> Vec<String> {
    session
        .search(&GuiSearchRequest {
            file_query: file.to_string(),
            content_query: content.to_string(),
            max_files: 200,
            ..GuiSearchRequest::default()
        })
        .unwrap()
        .rows
        .into_iter()
        .map(|row| row.relative_path.to_string_lossy().replace('\\', "/"))
        .collect()
}

fn product_manifest(
    root: &std::path::Path,
    store: &std::path::Path,
    extractor: &ExtractorConfig,
) -> personalrag_v2::incremental::BundleManifest {
    personalrag_v2::product::load_product_bundle(root, store, extractor)
        .unwrap()
        .manifest
}

#[test]
fn product_init_and_reconcile_drive_real_gui_bundle_lifecycle() {
    let base = temp_dir("lifecycle");
    let root = base.join("root");
    let store = base.join("store");
    fs::create_dir_all(root.join("ops")).unwrap();
    fs::create_dir_all(&store).unwrap();

    let mut large = Vec::with_capacity(4 * 1024 * 1024);
    large.extend_from_slice(b"PR_PRODUCT_FILLER\n");
    while large.len() < 4 * 1024 * 1024 {
        large.extend_from_slice(b"searchable filler alpha beta gamma 0123456789\n");
    }
    fs::write(root.join("large.txt"), large).unwrap();
    fs::write(root.join("ops/item.txt"), b"PR_PRODUCT_OLD_TOKEN\n").unwrap();
    for index in 0..160 {
        fs::write(root.join(format!("meta-{index:03}.bin")), [index as u8]).unwrap();
    }

    let extractor = ExtractorConfig::default();
    let initialized = initialize_store(&root, &store, &extractor).unwrap();
    assert_eq!(initialized.manifest.generation, 1);
    assert!(initialized.metadata_records > 150);

    let mut gui = GuiSearchSession::load(&root, &store, extractor.clone()).unwrap();
    assert_eq!(search(&gui, "item", "PR_PRODUCT_OLD_TOKEN").len(), 1);

    fs::write(root.join("ops/item.txt"), b"PR_PRODUCT_NEW_TOKEN\n").unwrap();
    let modified = reconcile_store(&root, &store, &extractor, None).unwrap();
    assert!(modified.committed);
    assert!(!modified.compacted);
    gui.reload().unwrap();
    assert!(search(&gui, "item", "PR_PRODUCT_OLD_TOKEN").is_empty());
    assert_eq!(search(&gui, "item", "PR_PRODUCT_NEW_TOKEN").len(), 1);

    fs::rename(root.join("ops/item.txt"), root.join("ops/renamed.txt")).unwrap();
    let renamed = reconcile_store(&root, &store, &extractor, None).unwrap();
    assert!(renamed.committed);
    assert!(!renamed.compacted);
    gui.reload().unwrap();
    assert!(search(&gui, "item", "PR_PRODUCT_NEW_TOKEN").is_empty());
    assert_eq!(search(&gui, "renamed", "PR_PRODUCT_NEW_TOKEN").len(), 1);

    fs::create_dir_all(root.join("moved")).unwrap();
    fs::rename(root.join("ops/renamed.txt"), root.join("moved/renamed.txt")).unwrap();
    reconcile_store(&root, &store, &extractor, None).unwrap();
    gui.reload().unwrap();
    assert_eq!(search(&gui, "renamed", "PR_PRODUCT_NEW_TOKEN").len(), 1);

    fs::remove_file(root.join("moved/renamed.txt")).unwrap();
    reconcile_store(&root, &store, &extractor, None).unwrap();
    gui.reload().unwrap();
    assert!(search(&gui, "renamed", "PR_PRODUCT_NEW_TOKEN").is_empty());

    fs::write(root.join("created.txt"), b"PR_PRODUCT_CREATED_TOKEN\n").unwrap();
    reconcile_store(&root, &store, &extractor, None).unwrap();
    gui.reload().unwrap();
    assert_eq!(
        search(&gui, "created", "PR_PRODUCT_CREATED_TOKEN"),
        vec!["created.txt"]
    );

    let before_checkpoint = product_manifest(&root, &store, &extractor);
    let checkpoint_only = reconcile_store(
        &root,
        &store,
        &extractor,
        Some(UsnCheckpoint {
            journal_id: 42,
            next_usn: 99,
        }),
    )
    .unwrap();
    assert!(checkpoint_only.committed);
    assert!(!checkpoint_only.compacted);
    assert_eq!(
        checkpoint_only.manifest.delta_generation,
        before_checkpoint.delta_generation
    );
    assert_ne!(
        checkpoint_only.manifest.state_generation,
        before_checkpoint.state_generation
    );

    fs::remove_dir_all(base).unwrap();
}
