use personalrag_v2::extraction::ExtractorConfig;
use personalrag_v2::gui::{GuiContentMode, GuiFileScope, GuiSearchRequest, GuiSearchSession};
use personalrag_v2::incremental::{
    BundleManifest, DeltaSnapshot, IncrementalState, write_bundle, write_delta_generation,
    write_metadata_generation, write_state_generation,
};
use personalrag_v2::usn::UsnCheckpoint;
use personalrag_v2::{MetadataIndex, MetadataRecord, publish_generation_with_extraction};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_dir(tag: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "personalrag-gui-{tag}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn write_large_text(path: &Path, marker: &str) {
    let mut bytes = Vec::with_capacity(2 * 1024 * 1024);
    bytes.extend_from_slice(format!("{marker}\n").as_bytes());
    let filler = b"personalrag gui controlled filler alpha beta gamma 0123456789\n";
    while bytes.len() < 2 * 1024 * 1024 {
        bytes.extend_from_slice(filler);
    }
    fs::write(path, bytes).unwrap();
}

fn metadata_record(file_id: u64, relative: &str, root: &Path) -> MetadataRecord {
    let metadata = fs::metadata(root.join(relative)).unwrap();
    let mut record = MetadataRecord::file(file_id, relative, metadata.len(), 0);
    record.content_searchable = true;
    record
}

fn setup_bundle() -> (PathBuf, PathBuf, GuiSearchSession) {
    let root = temp_dir("root");
    let store = temp_dir("store");
    fs::create_dir_all(root.join("notes")).unwrap();
    write_large_text(
        &root.join("notes/alpha.txt"),
        "ALPHA_ONLY_SENTINEL ERROR_1234 SharedContentToken",
    );
    write_large_text(
        &root.join("notes/bravo.txt"),
        "BRAVO_ONLY_SENTINEL ERROR_5678 SharedContentToken",
    );

    let extractor = ExtractorConfig::default();
    let published = publish_generation_with_extraction(&root, &store, 1, 0, &extractor).unwrap();
    let metadata = MetadataIndex::build(vec![
        metadata_record(100, "notes/alpha.txt", &root),
        metadata_record(200, "notes/bravo.txt", &root),
    ])
    .unwrap();
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
                journal_id: 7,
                next_usn: 100,
            },
            pending_renames: Vec::new(),
        },
    )
    .unwrap();
    write_bundle(
        &store,
        BundleManifest {
            generation: 1,
            parent_generation: 0,
            content_generation: published.generation,
            metadata_generation: 1,
            delta_generation: 1,
            state_generation: 1,
        },
    )
    .unwrap();

    let session = GuiSearchSession::load(&root, &store, extractor).unwrap();
    (root, store, session)
}

#[test]
fn gui_session_searches_metadata_content_and_intersection() {
    let (root, store, session) = setup_bundle();
    let status = session.status();
    assert_eq!(status.bundle_generation, 1);
    assert_eq!(status.metadata_records, 2);
    assert_eq!(status.delta_changes, 0);

    let metadata = session
        .search(&GuiSearchRequest {
            file_query: "alpha".into(),
            ..GuiSearchRequest::default()
        })
        .unwrap();
    assert_eq!(metadata.rows.len(), 1);
    assert_eq!(metadata.rows[0].name, "alpha.txt");
    assert!(metadata.rows[0].matches.is_empty());

    let full_path = session
        .search(&GuiSearchRequest {
            file_query: "notes\\bravo".into(),
            file_scope: GuiFileScope::FullPath,
            ..GuiSearchRequest::default()
        })
        .unwrap();
    assert_eq!(full_path.rows.len(), 1);
    assert_eq!(full_path.rows[0].name, "bravo.txt");

    let limited_content = session
        .search(&GuiSearchRequest {
            content_query: "SharedContentToken".into(),
            max_files: 1,
            ..GuiSearchRequest::default()
        })
        .unwrap();
    assert_eq!(limited_content.rows.len(), 1);

    let content = session
        .search(&GuiSearchRequest {
            content_query: "SharedContentToken".into(),
            max_files: 2,
            ..GuiSearchRequest::default()
        })
        .unwrap();
    assert_eq!(content.rows.len(), 2);
    assert!(content.rows.iter().all(|row| !row.matches.is_empty()));
    assert!(
        content
            .rows
            .iter()
            .all(|row| row.primary_preview().contains("SharedContentToken"))
    );

    let intersection = session
        .search(&GuiSearchRequest {
            file_query: "alpha".into(),
            content_query: "SharedContentToken".into(),
            ..GuiSearchRequest::default()
        })
        .unwrap();
    assert_eq!(intersection.rows.len(), 1);
    assert_eq!(intersection.rows[0].file_id, 100);
    assert_eq!(intersection.rows[0].primary_location(), "Line 1 · byte 31");
    assert_eq!(
        intersection.rows[0].absolute_path,
        root.join("notes/alpha.txt")
    );

    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(store).unwrap();
}

#[test]
fn gui_session_supports_literal_regex_wildcard_and_case_mode() {
    let (root, store, session) = setup_bundle();

    let regex = session
        .search(&GuiSearchRequest {
            content_query: "ERROR_[0-9]{4}".into(),
            content_mode: GuiContentMode::Regex,
            ..GuiSearchRequest::default()
        })
        .unwrap();
    assert_eq!(regex.rows.len(), 2);

    let wildcard = session
        .search(&GuiSearchRequest {
            content_query: "*ONLY_SENTINEL*".into(),
            content_mode: GuiContentMode::Wildcard,
            ..GuiSearchRequest::default()
        })
        .unwrap();
    assert_eq!(wildcard.rows.len(), 2);

    let insensitive = session
        .search(&GuiSearchRequest {
            file_query: "ALPHA.TXT".into(),
            ..GuiSearchRequest::default()
        })
        .unwrap();
    assert_eq!(insensitive.rows.len(), 1);

    let sensitive = session
        .search(&GuiSearchRequest {
            file_query: "ALPHA.TXT".into(),
            case_sensitive: true,
            ..GuiSearchRequest::default()
        })
        .unwrap();
    assert!(sensitive.rows.is_empty());

    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(store).unwrap();
}
