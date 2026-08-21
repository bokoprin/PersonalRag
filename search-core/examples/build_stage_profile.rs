use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use personalrag_portable_search::{
    AccelerationProfile, BuildMode, BuildOptions, DiskPathBuildConfig, DiskPathInput,
    VNextDocumentInput, build_disk_path_inputs_index_unified, initialize_vnext_generation_store,
    verify_vnext_generation_store,
};

fn temp_root(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "personalrag-build-profile-{label}-{}-{nonce}",
        std::process::id()
    ))
}

fn remove(path: &Path) {
    let _ = fs::remove_dir_all(path);
}

fn make_corpus(root: &Path, docs: usize, payload_bytes: usize) -> Vec<DiskPathInput> {
    fs::create_dir_all(root).unwrap();
    let mut inputs = Vec::with_capacity(docs);
    for row in 0..docs {
        let group = root.join(format!("g{:03}", row % 127));
        fs::create_dir_all(&group).unwrap();
        let path = group.join(format!("doc_{row:07}.txt"));
        let prefix = format!(
            "timeout marker row={row:07} group={:03} alpha={} beta={} ",
            row % 127,
            row.wrapping_mul(2_654_435_761usize) % 1_000_003,
            row.wrapping_mul(1_146_067_499usize) % 1_000_033,
        );
        let mut bytes = prefix.into_bytes();
        while bytes.len() < payload_bytes {
            bytes.push(b'a' + ((row + bytes.len()) % 26) as u8);
        }
        bytes.truncate(payload_bytes.max(1));
        fs::write(&path, &bytes).unwrap();
        let display = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        inputs.push(DiskPathInput {
            path: path.clone(),
            display_path: display,
            size_bytes: bytes.len() as u64,
            content_path: Some(path),
            index_content: true,
        });
    }
    inputs
}

fn vnext_documents(inputs: &[DiskPathInput]) -> Vec<VNextDocumentInput> {
    inputs
        .iter()
        .enumerate()
        .map(|(row, input)| {
            let mut content = fs::read(input.content_path.as_deref().unwrap()).unwrap();
            content.make_ascii_lowercase();
            VNextDocumentInput::new(row as u64 + 1, input.display_path.clone(), content)
        })
        .collect()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let docs = std::env::var("PR_PROFILE_DOCS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(20_000);
    let payload_bytes = std::env::var("PR_PROFILE_PAYLOAD_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(4_096);
    let root = temp_root("corpus");
    let perf = temp_root("perf12");
    let vnext = temp_root("vnext");
    let inputs = make_corpus(&root, docs, payload_bytes);
    let options = BuildOptions {
        mode: BuildMode::Adaptive,
        segment_docs: 5_000,
        workers: 4,
    };
    let cancel = AtomicBool::new(false);
    let config = DiskPathBuildConfig {
        max_docs: None,
        max_file_bytes: 64 * 1024 * 1024,
        build: &options,
        scan_workers: 8,
        hydration_batch_bytes: 128 * 1024 * 1024,
        cancel: Some(&cancel),
    };

    // Perf12 is profiled without retaining a second corpus so PRPOS timing is not distorted by
    // shared-hydration memory pressure. This is the same production builder used by the GUI.
    let perf_started = Instant::now();
    let perf_report = build_disk_path_inputs_index_unified(
        &root,
        inputs.clone(),
        &perf,
        config,
        AccelerationProfile::Full,
        |_| {},
    )?;
    let perf_ms = perf_started.elapsed().as_secs_f64() * 1000.0;

    // Materialize vNext inputs outside the timed section so the reported vNext stages are the
    // segment/index writer itself. Production either moves retained normalized documents or
    // materializes the verified Perf12 snapshot before entering the same initializer.
    let vnext_docs = vnext_documents(&inputs);
    let vnext_started = Instant::now();
    let vnext_report = initialize_vnext_generation_store(&vnext, &vnext_docs, 5_000)?;
    verify_vnext_generation_store(&vnext)?;
    let vnext_ms = vnext_started.elapsed().as_secs_f64() * 1000.0;

    println!(
        "BUILD_STAGE_PROFILE docs={} payload_bytes={} perf12_ms={:.3} perf12_segments={} vnext_ms={:.3} vnext_segments={} kernel_total_ms={:.3}",
        docs,
        payload_bytes,
        perf_ms,
        perf_report.build.segments,
        vnext_ms,
        vnext_report.segment_count,
        perf_ms + vnext_ms,
    );

    remove(&root);
    remove(&perf);
    remove(&vnext);
    Ok(())
}
