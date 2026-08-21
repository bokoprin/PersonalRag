use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{SystemTime, UNIX_EPOCH};

use personalrag_portable_search::{
    AccelerationProfile, BuildMode, BuildOptions, DiskPathBuildConfig, DiskPathInput,
    build_disk_path_inputs_index_unified, build_disk_path_inputs_index_unified_retained,
};

fn temp_root(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "personalrag-retained-hydration-{label}-{}-{nonce}",
        std::process::id()
    ))
}

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

#[test]
fn retained_hydration_is_byte_identical_and_returns_normalized_docs_in_id_order() {
    let corpus = temp_root("corpus");
    let normal = temp_root("normal");
    let retained = temp_root("retained");
    fs::create_dir_all(&corpus).unwrap();

    let mut inputs = Vec::new();
    for row in 0..17usize {
        let path = corpus.join(format!("doc_{row:03}.txt"));
        let text = format!("TiMeOuT row={row:03} {}", "Payload ".repeat(31 + row));
        fs::write(&path, text.as_bytes()).unwrap();
        inputs.push(DiskPathInput {
            path: path.clone(),
            display_path: format!("docs/doc_{row:03}.txt"),
            size_bytes: text.len() as u64,
            content_path: Some(path),
            index_content: true,
        });
    }

    let options = BuildOptions {
        mode: BuildMode::Adaptive,
        segment_docs: 5,
        workers: 3,
    };
    let cancel = AtomicBool::new(false);
    let config = || DiskPathBuildConfig {
        max_docs: None,
        max_file_bytes: 1024 * 1024,
        build: &options,
        scan_workers: 4,
        hydration_batch_bytes: 4 * 1024 * 1024,
        cancel: Some(&cancel),
    };

    let normal_report = build_disk_path_inputs_index_unified(
        &corpus,
        inputs.clone(),
        &normal,
        config(),
        AccelerationProfile::Full,
        |_| {},
    )
    .unwrap();
    let (retained_report, documents) = build_disk_path_inputs_index_unified_retained(
        &corpus,
        inputs,
        &retained,
        config(),
        AccelerationProfile::Full,
        |_| {},
    )
    .unwrap();

    assert_eq!(normal_report.build.docs, retained_report.build.docs);
    assert_eq!(normal_report.build.segments, retained_report.build.segments);
    assert_eq!(
        normal_report.build.index_bytes,
        retained_report.build.index_bytes
    );
    assert_eq!(normal_report.display_paths, retained_report.display_paths);
    assert_eq!(normal_report.source_indices, retained_report.source_indices);
    assert_eq!(documents.len(), retained_report.build.docs);
    for (row, document) in documents.iter().enumerate() {
        assert_eq!(document.display_path, format!("docs/doc_{row:03}.txt"));
        assert!(document.normalized_content.starts_with(b"timeout row="));
    }

    let normal_files = files_under(&normal);
    let retained_files = files_under(&retained);
    assert_eq!(normal_files, retained_files);
    for relative in normal_files {
        assert_eq!(
            fs::read(normal.join(&relative)).unwrap(),
            fs::read(retained.join(&relative)).unwrap(),
            "retained builder changed {}",
            relative.display()
        );
    }

    fs::remove_dir_all(corpus).unwrap();
    fs::remove_dir_all(normal).unwrap();
    fs::remove_dir_all(retained).unwrap();
}
