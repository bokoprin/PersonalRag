use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};

use personalrag_gui_bridge_core::{
    scan_files, search_catalog, search_catalog_with_metadata, snippets, ExclusionConfig,
    ExtractionBudget, IncrementalCatalogState, IncrementalChangeSyncRequest,
    IncrementalSyncRequest, IncrementalSyncResult, IndexEngine, OfficeExtractionConfig,
    OfficeExtractionService, PortableEngine, ScanExclusions, ScannerMode, SearchCatalogView,
    SearchEngine, SearchOptions, SearchRequest,
};
use personalrag_portable_search::{
    build_disk_path_inputs_index_pipelined, BuildMode, BuildOptions, DiskPathBuildConfig,
    DiskPathInput,
};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "personalrag-gui-bridge-{label}-{}-{id}",
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
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn logical_inverse(ids: &[u64]) -> Vec<u32> {
    let max = ids.iter().copied().max().unwrap_or(0) as usize;
    let mut inverse = vec![u32::MAX; max.saturating_add(1)];
    for (row, &logical_id) in ids.iter().enumerate() {
        inverse[logical_id as usize] = row as u32;
    }
    inverse
}

fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let offset = bytes.len() as u32;
        bytes.extend_from_slice(&0x04034b50u32.to_le_bytes());
        bytes.extend_from_slice(&20u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(name.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(data);

        central.extend_from_slice(&0x02014b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let central_offset = bytes.len() as u32;
    let central_size = central.len() as u32;
    bytes.extend_from_slice(&central);
    bytes.extend_from_slice(&0x06054b50u32.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    bytes.extend_from_slice(&central_size.to_le_bytes());
    bytes.extend_from_slice(&central_offset.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes
}

fn build_fixture() -> (TempDir, TempDir, Vec<String>) {
    let corpus = TempDir::new("corpus");
    let index = TempDir::new("index");
    fs::create_dir_all(corpus.path().join("src")).unwrap();
    fs::create_dir_all(corpus.path().join("node_modules/pkg")).unwrap();
    fs::create_dir_all(corpus.path().join("build")).unwrap();
    fs::create_dir_all(corpus.path().join(".venv2/Scripts")).unwrap();
    fs::create_dir_all(corpus.path().join("bin/Release")).unwrap();
    fs::create_dir_all(corpus.path().join("obj")).unwrap();
    fs::write(corpus.path().join("src/a.txt"), "Hello TOKEN alpha").unwrap();
    fs::write(corpus.path().join("src/b.md"), "日本 token beta").unwrap();
    fs::write(corpus.path().join("src/c.txt"), "tokenized only").unwrap();
    fs::write(corpus.path().join("node_modules/pkg/skip.js"), "TOKEN").unwrap();
    fs::write(corpus.path().join("build/skip.txt"), "TOKEN").unwrap();
    fs::write(corpus.path().join(".venv2/Scripts/skip.py"), "TOKEN").unwrap();
    fs::write(corpus.path().join("bin/Release/skip.dll"), "TOKEN").unwrap();
    fs::write(corpus.path().join("obj/skip.obj"), "TOKEN").unwrap();
    fs::write(corpus.path().join("photo.png"), b"BINARY_TOKEN_IN_IMAGE").unwrap();
    fs::write(corpus.path().join("ignored.tmp"), "TOKEN").unwrap();
    fs::write(corpus.path().join("ignored-by-git.txt"), "TOKEN").unwrap();
    fs::write(corpus.path().join(".gitignore"), "ignored-by-git.txt\n").unwrap();

    let cancel = Arc::new(AtomicBool::new(false));
    let report = scan_files(
        corpus.path(),
        1024 * 1024,
        ScannerMode::WalkDir,
        &ScanExclusions {
            node_modules: true,
            virtual_envs: true,
            build_artifacts: true,
            use_gitignore: true,
            custom_globs: vec!["*.tmp".to_owned()],
            ..ScanExclusions::default()
        },
        cancel,
        Arc::new(|_| {}),
    )
    .unwrap();
    let mut files = report.files;
    files.sort_by(|left, right| left.path.cmp(&right.path));
    let files = files
        .into_iter()
        .map(|file| DiskPathInput {
            path: file.path,
            display_path: file.display_path,
            size_bytes: file.size_bytes,
            content_path: None,
            index_content: file.index_content,
        })
        .collect();
    let build_options = BuildOptions {
        mode: BuildMode::Adaptive,
        segment_docs: 2,
        workers: 2,
    };
    let cancel = AtomicBool::new(false);
    let built = build_disk_path_inputs_index_pipelined(
        corpus.path(),
        files,
        index.path(),
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
    (corpus, index, built.display_paths)
}

fn options() -> SearchOptions {
    SearchOptions {
        file_query: None,
        include_path: false,
        content_query: None,
        extensions: Vec::new(),
        path_scope: None,
        match_case: false,
        whole_words: false,
        regex: false,
        sort_field: "path".to_owned(),
        sort_direction: "ascending".to_owned(),
        limit: 2_000,
        backend: "v2".to_owned(),
    }
}

#[test]
fn scanner_exclusions_and_all_search_controls_are_connected() {
    let (corpus, index, paths) = build_fixture();
    assert!(paths.iter().any(|path| path == "src/a.txt"));
    assert!(paths.iter().any(|path| path == "src/b.md"));
    assert!(paths.iter().any(|path| path == "src/c.txt"));
    assert!(!paths.iter().any(|path| path.contains("node_modules")));
    assert!(!paths.iter().any(|path| path.starts_with("build/")));
    assert!(!paths.iter().any(|path| path.starts_with(".venv2/")));
    assert!(!paths.iter().any(|path| path.starts_with("bin/")));
    assert!(!paths.iter().any(|path| path.starts_with("obj/")));
    assert!(paths.iter().any(|path| path == "photo.png"));
    assert!(!paths.iter().any(|path| path.ends_with("ignored.tmp")));
    assert!(!paths
        .iter()
        .any(|path| path.ends_with("ignored-by-git.txt")));

    let image_name_hits = search_catalog(
        index.path(),
        corpus.path(),
        &paths,
        &SearchOptions {
            file_query: Some("photo".to_owned()),
            ..options()
        },
        || false,
    )
    .unwrap();
    assert_eq!(image_name_hits.len(), 1);
    let image_content_hits = search_catalog(
        index.path(),
        corpus.path(),
        &paths,
        &SearchOptions {
            content_query: Some("BINARY_TOKEN_IN_IMAGE".to_owned()),
            ..options()
        },
        || false,
    )
    .unwrap();
    assert!(image_content_hits.is_empty());

    let mut request = options();
    request.content_query = Some("token".to_owned());
    let hits = search_catalog(index.path(), corpus.path(), &paths, &request, || false).unwrap();
    assert_eq!(hits.len(), 3);

    request.whole_words = true;
    let hits = search_catalog(index.path(), corpus.path(), &paths, &request, || false).unwrap();
    assert_eq!(hits.len(), 2);
    request.whole_words = false;

    request.match_case = true;
    request.content_query = Some("TOKEN".to_owned());
    let hits = search_catalog(index.path(), corpus.path(), &paths, &request, || false).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].name, "a.txt");

    request.match_case = false;
    request.content_query = Some("token".to_owned());
    request.extensions = vec!["md".to_owned()];
    let hits = search_catalog(index.path(), corpus.path(), &paths, &request, || false).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].name, "b.md");

    request.extensions.clear();
    request.path_scope = Some("src".to_owned());
    request.regex = true;
    request.content_query = Some("h.llo".to_owned());
    let hits = search_catalog(index.path(), corpus.path(), &paths, &request, || false).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].name, "a.txt");

    request.regex = false;
    request.content_query = None;
    request.path_scope = None;
    request.file_query = Some("src".to_owned());
    request.include_path = false;
    assert!(
        search_catalog(index.path(), corpus.path(), &paths, &request, || false)
            .unwrap()
            .is_empty()
    );
    request.include_path = true;
    assert_eq!(
        search_catalog(index.path(), corpus.path(), &paths, &request, || false)
            .unwrap()
            .len(),
        3
    );

    request.backend = "v1".to_owned();
    assert_eq!(
        search_catalog(index.path(), corpus.path(), &paths, &request, || false)
            .unwrap()
            .len(),
        3
    );

    request.file_query = None;
    request.content_query = Some("token".to_owned());
    request.include_path = false;
    request.backend = "v2".to_owned();
    request.sort_field = "name".to_owned();
    request.sort_direction = "descending".to_owned();
    let hits = search_catalog(index.path(), corpus.path(), &paths, &request, || false).unwrap();
    let names = hits.iter().map(|hit| hit.name.as_str()).collect::<Vec<_>>();
    assert_eq!(names, vec!["c.txt", "b.md", "a.txt"]);
}

#[test]
fn scanner_honors_max_size_and_pre_cancel() {
    let corpus = TempDir::new("scanner-controls");
    fs::write(corpus.path().join("small.txt"), "ok").unwrap();
    fs::write(corpus.path().join("large.txt"), vec![b'x'; 4096]).unwrap();

    let report = scan_files(
        corpus.path(),
        1024,
        ScannerMode::WalkDir,
        &ScanExclusions::default(),
        Arc::new(AtomicBool::new(false)),
        Arc::new(|_| {}),
    )
    .unwrap();
    assert_eq!(report.files.len(), 1);
    assert!(report.files[0].path.ends_with("small.txt"));
    assert_eq!(report.files[0].display_path, "small.txt");
    assert_eq!(report.files[0].size_bytes, 2);
    assert!(report.progress.pruned_entries >= 1);

    let error = scan_files(
        corpus.path(),
        1024,
        ScannerMode::Auto,
        &ScanExclusions::default(),
        Arc::new(AtomicBool::new(true)),
        Arc::new(|_| {}),
    )
    .unwrap_err();
    assert_eq!(error, "cancelled");
}

#[test]
fn cached_catalog_metadata_drives_size_sort_without_filesystem_restat_semantics() {
    let (corpus, index, paths) = build_fixture();
    let mut sizes = vec![0_u64; paths.len()];
    let modified = vec![0_u64; paths.len()];
    for (index, path) in paths.iter().enumerate() {
        sizes[index] = match path.as_str() {
            "src/a.txt" => 10,
            "src/b.md" => 30,
            "src/c.txt" => 20,
            _ => 0,
        };
    }

    let mut request = options();
    request.content_query = Some("token".to_owned());
    request.sort_field = "size".to_owned();
    request.sort_direction = "descending".to_owned();
    let hits = search_catalog_with_metadata(
        index.path(),
        corpus.path(),
        &paths,
        &sizes,
        &modified,
        &request,
        || false,
    )
    .unwrap();
    let names = hits.iter().map(|hit| hit.name.as_str()).collect::<Vec<_>>();
    assert_eq!(names, vec!["b.md", "c.txt", "a.txt"]);
    assert_eq!(
        hits.iter().map(|hit| hit.size_bytes).collect::<Vec<_>>(),
        vec![30, 20, 10]
    );
}

#[test]
#[ignore = "large filesystem stress; run explicitly in Windows acceptance"]
fn scanner_large_tree_stress() {
    use std::sync::atomic::AtomicUsize;

    let count = std::env::var("PERSONALRAG_STRESS_FILES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(50_000);
    let corpus = TempDir::new("scanner-large");
    let groups = 256usize;
    for group in 0..groups {
        fs::create_dir_all(corpus.path().join(format!("g{group:03}"))).unwrap();
    }
    for index in 0..count {
        let group = index % groups;
        fs::write(
            corpus
                .path()
                .join(format!("g{group:03}"))
                .join(format!("f{index:07}.txt")),
            b"x",
        )
        .unwrap();
    }

    let callbacks = Arc::new(AtomicUsize::new(0));
    let callback_counter = Arc::clone(&callbacks);
    let report = scan_files(
        corpus.path(),
        1024,
        ScannerMode::Auto,
        &ScanExclusions::default(),
        Arc::new(AtomicBool::new(false)),
        Arc::new(move |_| {
            callback_counter.fetch_add(1, Ordering::Relaxed);
        }),
    )
    .unwrap();
    assert_eq!(report.files.len(), count);
    assert_eq!(report.progress.selected_files, count);
    assert!(callbacks.load(Ordering::Relaxed) > 1);
}

#[test]
fn regex_and_whole_word_snippets_follow_search_controls() {
    let corpus = TempDir::new("snippets");
    let path = corpus.path().join("sample.txt");
    fs::write(&path, "prefix tokenized\nexact TOKEN\n日本 token\n").unwrap();
    let hits = snippets(&path, "token", 0, 10, false, true, false).unwrap();
    assert_eq!(hits.len(), 2);
    let regex_hits = snippets(&path, "^exact T.KEN$", 0, 10, false, false, true).unwrap();
    assert_eq!(regex_hits.len(), 1);
}

#[test]
fn portable_facade_owns_search_and_index_engine_boundary() {
    use personalrag_gui_bridge_core::SnippetRequest;

    let corpus = TempDir::new("facade-corpus");
    let index = TempDir::new("facade-index");
    fs::create_dir_all(corpus.path().join("src")).unwrap();
    fs::write(corpus.path().join("src/a.txt"), "Hello facade TOKEN").unwrap();
    fs::write(corpus.path().join("src/b.txt"), "日本 facade").unwrap();

    let engine = PortableEngine::default();
    let cancel = Arc::new(AtomicBool::new(false));
    let scan = engine
        .scan(
            corpus.path(),
            1024 * 1024,
            "auto",
            &ExclusionConfig::default(),
            Arc::clone(&cancel),
            Arc::new(|_| {}),
        )
        .unwrap();
    let mut progress_events = 0usize;
    let built = engine
        .build(
            corpus.path(),
            scan.files,
            index.path(),
            1024 * 1024,
            cancel.as_ref(),
            &mut |_| progress_events += 1,
        )
        .unwrap();
    assert!(progress_events >= 5);
    assert_eq!(built.indexed_files, 2);

    let logical_to_row = logical_inverse(&built.logical_ids);
    let hits = engine
        .search(
            index.path(),
            SearchCatalogView {
                root: corpus.path(),
                paths: &built.paths,
                size_bytes: &built.size_bytes,
                modified_ns: &built.modified_ns,
                logical_ids: &built.logical_ids,
                logical_to_row: &logical_to_row,
                generation: built.generation,
                max_file_bytes: 1024 * 1024,
            },
            SearchRequest {
                file_query: None,
                include_path: false,
                content_query: Some("facade".to_owned()),
                extensions: Vec::new(),
                path_scope: None,
                match_case: false,
                whole_words: false,
                regex: false,
                sort_field: "path".to_owned(),
                sort_direction: "ascending".to_owned(),
                limit: 100,
            },
            "v2",
            &|| false,
        )
        .unwrap();
    assert_eq!(hits.len(), 2);

    let generation_mismatch = engine
        .search(
            index.path(),
            SearchCatalogView {
                root: corpus.path(),
                paths: &built.paths,
                size_bytes: &built.size_bytes,
                modified_ns: &built.modified_ns,
                logical_ids: &built.logical_ids,
                logical_to_row: &logical_to_row,
                generation: built.generation + 1,
                max_file_bytes: 1024 * 1024,
            },
            SearchRequest {
                file_query: None,
                include_path: false,
                content_query: Some("facade".to_owned()),
                extensions: Vec::new(),
                path_scope: None,
                match_case: false,
                whole_words: false,
                regex: false,
                sort_field: "path".to_owned(),
                sort_direction: "ascending".to_owned(),
                limit: 100,
            },
            "v2",
            &|| false,
        )
        .unwrap_err();
    assert!(generation_mismatch.contains("generation"));

    let requests = hits
        .iter()
        .map(|hit| SnippetRequest {
            path: PathBuf::from(&hit.path),
            query: "facade".to_owned(),
            context: 0,
            max_hits: 2,
            match_case: false,
            whole_words: false,
            regex: false,
        })
        .collect::<Vec<_>>();
    let batches = engine.snippets_batch(&requests).unwrap();
    assert_eq!(batches.len(), 2);
    assert!(batches.iter().all(|batch| !batch.hits.is_empty()));
}

#[test]
fn non_path_sort_uses_exact_top_k_semantics() {
    let corpus = TempDir::new("top-k-corpus");
    let index = TempDir::new("top-k-index");
    let mut inputs = Vec::new();
    for value in 0..128usize {
        let name = format!("f{value:03}.txt");
        let path = corpus.path().join(&name);
        fs::write(&path, "common-token").unwrap();
        inputs.push(DiskPathInput {
            path,
            display_path: name,
            size_bytes: 12,
            content_path: None,
            index_content: true,
        });
    }
    let build_options = BuildOptions {
        mode: BuildMode::Adaptive,
        segment_docs: 32,
        workers: 2,
    };
    let cancel = AtomicBool::new(false);
    let built = build_disk_path_inputs_index_pipelined(
        corpus.path(),
        inputs,
        index.path(),
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
    let sizes = built
        .display_paths
        .iter()
        .enumerate()
        .map(|(index, _)| ((index * 37) % 211) as u64)
        .collect::<Vec<_>>();
    let modified = vec![0_u64; sizes.len()];
    let mut request = options();
    request.content_query = Some("common-token".to_owned());
    request.sort_field = "size".to_owned();
    request.sort_direction = "descending".to_owned();
    request.limit = 10;
    let hits = search_catalog_with_metadata(
        index.path(),
        corpus.path(),
        &built.display_paths,
        &sizes,
        &modified,
        &request,
        || false,
    )
    .unwrap();

    let mut expected = built
        .display_paths
        .iter()
        .enumerate()
        .map(|(index, path)| (sizes[index], path.clone()))
        .collect::<Vec<_>>();
    expected.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    expected.truncate(10);
    assert_eq!(
        hits.iter()
            .map(|hit| (hit.size_bytes, hit.name.clone()))
            .collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn office_extraction_and_incremental_sync_work_through_engine_facade() {
    let corpus = TempDir::new("office-incremental-corpus");
    let index = TempDir::new("office-incremental-index");
    fs::write(
        corpus.path().join("keep.txt"),
        "stable-token rare-order-token",
    )
    .unwrap();
    fs::write(corpus.path().join("modify.txt"), "old-token").unwrap();
    fs::write(corpus.path().join("delete.txt"), "delete-token").unwrap();
    let office = stored_zip(&[(
        "word/document.xml",
        br#"<?xml version="1.0"?><w:document><w:body><w:p><w:r><w:t>OfficeUniqueMarker</w:t></w:r></w:p></w:body></w:document>"#,
    )]);
    fs::write(corpus.path().join("report.docx"), office).unwrap();

    let engine = PortableEngine::default();
    let cancel = Arc::new(AtomicBool::new(false));
    let first_scan = engine
        .scan(
            corpus.path(),
            4 * 1024 * 1024,
            "walk_dir",
            &ExclusionConfig::default(),
            Arc::clone(&cancel),
            Arc::new(|_| {}),
        )
        .unwrap();
    let first = engine
        .build(
            corpus.path(),
            first_scan.files,
            index.path(),
            4 * 1024 * 1024,
            cancel.as_ref(),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(first.generation, 0);
    let first_inverse = logical_inverse(&first.logical_ids);
    let office_hits = engine
        .search(
            index.path(),
            SearchCatalogView {
                root: corpus.path(),
                paths: &first.paths,
                size_bytes: &first.size_bytes,
                modified_ns: &first.modified_ns,
                logical_ids: &first.logical_ids,
                logical_to_row: &first_inverse,
                generation: first.generation,
                max_file_bytes: 4 * 1024 * 1024,
            },
            SearchRequest {
                file_query: None,
                include_path: false,
                content_query: Some("OfficeUniqueMarker".to_owned()),
                extensions: Vec::new(),
                path_scope: None,
                match_case: false,
                whole_words: false,
                regex: false,
                sort_field: "path".to_owned(),
                sort_direction: "ascending".to_owned(),
                limit: 100,
            },
            "v2",
            &|| false,
        )
        .unwrap();
    assert_eq!(office_hits.len(), 1);
    assert!(office_hits[0].path.ends_with("report.docx"));

    let old_modify_id = first
        .paths
        .iter()
        .position(|path| path == "modify.txt")
        .map(|row| first.logical_ids[row])
        .unwrap();
    let previous = IncrementalCatalogState {
        generation: first.generation,
        next_logical_id: first.next_logical_id,
        paths: first.paths.clone(),
        logical_ids: first.logical_ids.clone(),
        size_bytes: first.size_bytes.clone(),
        modified_ns: first.modified_ns.clone(),
    };

    fs::write(
        corpus.path().join("modify.txt"),
        "new-token-with-different-length",
    )
    .unwrap();
    fs::remove_file(corpus.path().join("delete.txt")).unwrap();
    fs::write(
        corpus.path().join("added.txt"),
        "added-token rare-order-token",
    )
    .unwrap();
    let second_scan = engine
        .scan(
            corpus.path(),
            4 * 1024 * 1024,
            "walk_dir",
            &ExclusionConfig::default(),
            Arc::clone(&cancel),
            Arc::new(|_| {}),
        )
        .unwrap();
    let second_files = second_scan.files;
    let sync = engine
        .sync_incremental(
            IncrementalSyncRequest {
                root: corpus.path(),
                files: &second_files,
                index_dir: index.path(),
                previous,
                max_file_bytes: 4 * 1024 * 1024,
            },
            cancel.as_ref(),
            &mut |_| {},
        )
        .unwrap();
    let IncrementalSyncResult::Applied(second) = sync else {
        panic!("expected incremental update");
    };
    assert_eq!(second.generation, 1);
    assert_eq!(second.processed_files, 3);
    let new_modify_id = second
        .paths
        .iter()
        .position(|path| path == "modify.txt")
        .map(|row| second.logical_ids[row])
        .unwrap();
    assert_eq!(new_modify_id, old_modify_id);
    assert!(!second.paths.iter().any(|path| path == "delete.txt"));
    assert!(second.paths.iter().any(|path| path == "added.txt"));

    let second_inverse = logical_inverse(&second.logical_ids);
    let search_token = |query: &str| {
        engine
            .search(
                index.path(),
                SearchCatalogView {
                    root: corpus.path(),
                    paths: &second.paths,
                    size_bytes: &second.size_bytes,
                    modified_ns: &second.modified_ns,
                    logical_ids: &second.logical_ids,
                    logical_to_row: &second_inverse,
                    generation: second.generation,
                    max_file_bytes: 4 * 1024 * 1024,
                },
                SearchRequest {
                    file_query: None,
                    include_path: false,
                    content_query: Some(query.to_owned()),
                    extensions: Vec::new(),
                    path_scope: None,
                    match_case: false,
                    whole_words: false,
                    regex: false,
                    sort_field: "path".to_owned(),
                    sort_direction: "ascending".to_owned(),
                    limit: 100,
                },
                "v2",
                &|| false,
            )
            .unwrap()
    };
    assert_eq!(search_token("new-token").len(), 1);
    assert_eq!(search_token("added-token").len(), 1);
    assert!(search_token("old-token").is_empty());
    assert!(search_token("delete-token").is_empty());
    assert_eq!(search_token("OfficeUniqueMarker").len(), 1);

    let rare_first = engine
        .search(
            index.path(),
            SearchCatalogView {
                root: corpus.path(),
                paths: &second.paths,
                size_bytes: &second.size_bytes,
                modified_ns: &second.modified_ns,
                logical_ids: &second.logical_ids,
                logical_to_row: &second_inverse,
                generation: second.generation,
                max_file_bytes: 4 * 1024 * 1024,
            },
            SearchRequest {
                file_query: None,
                include_path: false,
                content_query: Some("rare-order-token".to_owned()),
                extensions: Vec::new(),
                path_scope: None,
                match_case: false,
                whole_words: false,
                regex: false,
                sort_field: "path".to_owned(),
                sort_direction: "ascending".to_owned(),
                limit: 1,
            },
            "v2",
            &|| false,
        )
        .unwrap();
    assert_eq!(rare_first.len(), 1);
    assert!(rare_first[0].path.ends_with("added.txt"));

    let rare_filename_first = engine
        .search(
            index.path(),
            SearchCatalogView {
                root: corpus.path(),
                paths: &second.paths,
                size_bytes: &second.size_bytes,
                modified_ns: &second.modified_ns,
                logical_ids: &second.logical_ids,
                logical_to_row: &second_inverse,
                generation: second.generation,
                max_file_bytes: 4 * 1024 * 1024,
            },
            SearchRequest {
                file_query: Some("added.txt".to_owned()),
                include_path: false,
                content_query: None,
                extensions: Vec::new(),
                path_scope: None,
                match_case: false,
                whole_words: false,
                regex: false,
                sort_field: "path".to_owned(),
                sort_direction: "ascending".to_owned(),
                limit: 1,
            },
            "v2",
            &|| false,
        )
        .unwrap();
    assert_eq!(rare_filename_first.len(), 1);
    assert!(rare_filename_first[0].path.ends_with("added.txt"));

    let second_state = IncrementalCatalogState {
        generation: second.generation,
        next_logical_id: second.next_logical_id,
        paths: second.paths.clone(),
        logical_ids: second.logical_ids.clone(),
        size_bytes: second.size_bytes.clone(),
        modified_ns: second.modified_ns.clone(),
    };
    let unchanged = engine
        .sync_incremental(
            IncrementalSyncRequest {
                root: corpus.path(),
                files: &second_files,
                index_dir: index.path(),
                previous: second_state,
                max_file_bytes: 4 * 1024 * 1024,
            },
            cancel.as_ref(),
            &mut |_| {},
        )
        .unwrap();
    let IncrementalSyncResult::Unchanged(third) = unchanged else {
        panic!("expected unchanged incremental sync");
    };
    assert_eq!(third.generation, second.generation);
    assert_eq!(third.processed_files, 0);
}

#[test]
fn office_cache_reuses_media_only_change_and_refreshes_searchable_xml() {
    let corpus = TempDir::new("office-cache-corpus");
    let index = TempDir::new("office-cache-index");
    let document_v1 = br#"<?xml version="1.0"?><w:document><w:body><w:p><w:r><w:t>OfficeCacheMarkerV1</w:t></w:r></w:p></w:body></w:document>"#;
    fs::write(
        corpus.path().join("report.docx"),
        stored_zip(&[
            ("word/document.xml", document_v1),
            ("word/media/image1.bin", b"media-A"),
        ]),
    )
    .unwrap();

    let engine = PortableEngine::default();
    let cancel = Arc::new(AtomicBool::new(false));
    let first_scan = engine
        .scan(
            corpus.path(),
            4 * 1024 * 1024,
            "walk_dir",
            &ExclusionConfig::default(),
            Arc::clone(&cancel),
            Arc::new(|_| {}),
        )
        .unwrap();
    let first = engine
        .build(
            corpus.path(),
            first_scan.files,
            index.path(),
            4 * 1024 * 1024,
            cancel.as_ref(),
            &mut |_| {},
        )
        .unwrap();
    let cache_root = OfficeExtractionService::cache_root_for_index_path(index.path());
    let cache = OfficeExtractionService::new(
        cache_root.clone(),
        ExtractionBudget::from_max_file_bytes(4 * 1024 * 1024),
        OfficeExtractionConfig::default(),
    );
    let live1 = cache.load_live();
    let key1 = live1.get("report.docx").cloned().expect("Office LIVE key");
    let object1 = cache_root
        .join("objects")
        .join(&key1[..2])
        .join(format!("{key1}.txt"));
    assert!(object1.exists());
    let object1_modified = fs::metadata(&object1).unwrap().modified().unwrap();

    // Change only ignored media bytes. USN/metadata sees an upsert, but searchable XML fingerprint
    // stays the same, so the cached extraction must be reused without rewriting the object.
    fs::write(
        corpus.path().join("report.docx"),
        stored_zip(&[
            ("word/document.xml", document_v1),
            ("word/media/image1.bin", b"media-B-with-different-size"),
        ]),
    )
    .unwrap();
    let second_scan = engine
        .scan(
            corpus.path(),
            4 * 1024 * 1024,
            "walk_dir",
            &ExclusionConfig::default(),
            Arc::clone(&cancel),
            Arc::new(|_| {}),
        )
        .unwrap();
    let previous1 = IncrementalCatalogState {
        generation: first.generation,
        next_logical_id: first.next_logical_id,
        paths: first.paths.clone(),
        logical_ids: first.logical_ids.clone(),
        size_bytes: first.size_bytes.clone(),
        modified_ns: first.modified_ns.clone(),
    };
    let second = match engine
        .sync_incremental(
            IncrementalSyncRequest {
                root: corpus.path(),
                files: &second_scan.files,
                index_dir: index.path(),
                previous: previous1,
                max_file_bytes: 4 * 1024 * 1024,
            },
            cancel.as_ref(),
            &mut |_| {},
        )
        .unwrap()
    {
        IncrementalSyncResult::Applied(value) => value,
        other => panic!("expected Office media-only incremental apply, got {other:?}"),
    };
    let live2 = cache.load_live();
    assert_eq!(live2.get("report.docx"), Some(&key1));
    assert_eq!(
        fs::metadata(&object1).unwrap().modified().unwrap(),
        object1_modified
    );

    // Change searchable XML. The fingerprint must change, a new cache object must be published,
    // and search semantics must move to the new text.
    let document_v2 = br#"<?xml version="1.0"?><w:document><w:body><w:p><w:r><w:t>OfficeCacheMarkerV2</w:t></w:r></w:p></w:body></w:document>"#;
    fs::write(
        corpus.path().join("report.docx"),
        stored_zip(&[("word/document.xml", document_v2)]),
    )
    .unwrap();
    let third_scan = engine
        .scan(
            corpus.path(),
            4 * 1024 * 1024,
            "walk_dir",
            &ExclusionConfig::default(),
            Arc::clone(&cancel),
            Arc::new(|_| {}),
        )
        .unwrap();
    let previous2 = IncrementalCatalogState {
        generation: second.generation,
        next_logical_id: second.next_logical_id,
        paths: second.paths.clone(),
        logical_ids: second.logical_ids.clone(),
        size_bytes: second.size_bytes.clone(),
        modified_ns: second.modified_ns.clone(),
    };
    let third = match engine
        .sync_incremental(
            IncrementalSyncRequest {
                root: corpus.path(),
                files: &third_scan.files,
                index_dir: index.path(),
                previous: previous2,
                max_file_bytes: 4 * 1024 * 1024,
            },
            cancel.as_ref(),
            &mut |_| {},
        )
        .unwrap()
    {
        IncrementalSyncResult::Applied(value) => value,
        other => panic!("expected Office XML incremental apply, got {other:?}"),
    };
    let live3 = cache.load_live();
    let key3 = live3.get("report.docx").expect("updated Office LIVE key");
    assert_ne!(key3, &key1);

    let inverse = logical_inverse(&third.logical_ids);
    let run_search = |query: &str| {
        engine
            .search(
                index.path(),
                SearchCatalogView {
                    root: corpus.path(),
                    paths: &third.paths,
                    size_bytes: &third.size_bytes,
                    modified_ns: &third.modified_ns,
                    logical_ids: &third.logical_ids,
                    logical_to_row: &inverse,
                    generation: third.generation,
                    max_file_bytes: 4 * 1024 * 1024,
                },
                SearchRequest {
                    file_query: None,
                    include_path: false,
                    content_query: Some(query.to_owned()),
                    extensions: Vec::new(),
                    path_scope: None,
                    match_case: false,
                    whole_words: false,
                    regex: false,
                    sort_field: "path".to_owned(),
                    sort_direction: "ascending".to_owned(),
                    limit: 100,
                },
                "v2",
                &|| false,
            )
            .unwrap()
    };
    assert!(run_search("OfficeCacheMarkerV1").is_empty());
    assert_eq!(run_search("OfficeCacheMarkerV2").len(), 1);
    let _ = fs::remove_dir_all(cache_root);
}

#[test]
fn sparse_incremental_change_sync_preserves_unchanged_catalog_rows() {
    let corpus = TempDir::new("sparse-incremental-corpus");
    let index = TempDir::new("sparse-incremental-index");
    fs::write(corpus.path().join("a.txt"), "old-a-token").unwrap();
    fs::write(corpus.path().join("keep.txt"), "keep-token").unwrap();
    fs::write(corpus.path().join("gone.txt"), "gone-token").unwrap();

    let engine = PortableEngine::default();
    let cancel = Arc::new(AtomicBool::new(false));
    let first_scan = engine
        .scan(
            corpus.path(),
            1024 * 1024,
            "walk_dir",
            &ExclusionConfig::default(),
            Arc::clone(&cancel),
            Arc::new(|_| {}),
        )
        .unwrap();
    let first = engine
        .build(
            corpus.path(),
            first_scan.files,
            index.path(),
            1024 * 1024,
            cancel.as_ref(),
            &mut |_| {},
        )
        .unwrap();
    let previous = IncrementalCatalogState {
        generation: first.generation,
        next_logical_id: first.next_logical_id,
        paths: first.paths.clone(),
        logical_ids: first.logical_ids.clone(),
        size_bytes: first.size_bytes.clone(),
        modified_ns: first.modified_ns.clone(),
    };

    fs::write(corpus.path().join("a.txt"), "new-a-token").unwrap();
    fs::remove_file(corpus.path().join("gone.txt")).unwrap();
    fs::write(corpus.path().join("z.txt"), "new-z-token").unwrap();
    let metadata_scan = engine
        .scan(
            corpus.path(),
            1024 * 1024,
            "walk_dir",
            &ExclusionConfig::default(),
            Arc::clone(&cancel),
            Arc::new(|_| {}),
        )
        .unwrap();
    let mut upserts = metadata_scan
        .files
        .into_iter()
        .filter(|file| matches!(file.display_path.as_str(), "a.txt" | "z.txt"))
        .collect::<Vec<_>>();
    assert_eq!(upserts.len(), 2);
    let old_a_row = first.paths.iter().position(|path| path == "a.txt").unwrap();
    let a = upserts
        .iter_mut()
        .find(|file| file.display_path == "a.txt")
        .unwrap();
    assert_eq!(a.size_bytes, first.size_bytes[old_a_row]);
    a.modified_ns = first.modified_ns[old_a_row];
    // A delete+create/rename window may report both states for the same path; the final upsert wins.
    let deleted = vec!["gone.txt".to_owned(), "a.txt".to_owned()];
    let sync = engine
        .sync_incremental_changes(
            IncrementalChangeSyncRequest {
                root: corpus.path(),
                upserts: &upserts,
                deleted_paths: &deleted,
                index_dir: index.path(),
                previous,
                max_file_bytes: 1024 * 1024,
            },
            cancel.as_ref(),
            &mut |_| {},
        )
        .unwrap();
    let IncrementalSyncResult::Applied(second) = sync else {
        panic!("expected sparse incremental update");
    };
    assert_eq!(second.processed_files, 3);
    assert!(second.paths.iter().any(|path| path == "keep.txt"));
    assert!(!second.paths.iter().any(|path| path == "gone.txt"));
    assert!(second.paths.iter().any(|path| path == "z.txt"));

    let inverse = logical_inverse(&second.logical_ids);
    let hits = engine
        .search(
            index.path(),
            SearchCatalogView {
                root: corpus.path(),
                paths: &second.paths,
                size_bytes: &second.size_bytes,
                modified_ns: &second.modified_ns,
                logical_ids: &second.logical_ids,
                logical_to_row: &inverse,
                generation: second.generation,
                max_file_bytes: 1024 * 1024,
            },
            SearchRequest {
                file_query: None,
                include_path: false,
                content_query: Some("keep-token".to_owned()),
                extensions: Vec::new(),
                path_scope: None,
                match_case: false,
                whole_words: false,
                regex: false,
                sort_field: "path".to_owned(),
                sort_direction: "ascending".to_owned(),
                limit: 10,
            },
            "v2",
            &|| false,
        )
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert!(hits[0].path.ends_with("keep.txt"));

    let search_content = |query: &str| {
        engine
            .search(
                index.path(),
                SearchCatalogView {
                    root: corpus.path(),
                    paths: &second.paths,
                    size_bytes: &second.size_bytes,
                    modified_ns: &second.modified_ns,
                    logical_ids: &second.logical_ids,
                    logical_to_row: &inverse,
                    generation: second.generation,
                    max_file_bytes: 1024 * 1024,
                },
                SearchRequest {
                    file_query: None,
                    include_path: false,
                    content_query: Some(query.to_owned()),
                    extensions: Vec::new(),
                    path_scope: None,
                    match_case: false,
                    whole_words: false,
                    regex: false,
                    sort_field: "path".to_owned(),
                    sort_direction: "ascending".to_owned(),
                    limit: 10,
                },
                "v2",
                &|| false,
            )
            .unwrap()
    };
    assert_eq!(search_content("new-a-token").len(), 1);
    assert!(search_content("old-a-token").is_empty());
}
