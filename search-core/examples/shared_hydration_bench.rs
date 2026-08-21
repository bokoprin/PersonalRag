use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use personalrag_portable_search::{
    AccelerationProfile, BuildMode, BuildOptions, DiskPathBuildConfig, DiskPathInput,
    LogicalDocumentIdentity, MergedIndex, VNextDocumentInput, build_disk_path_inputs_index_unified,
    build_disk_path_inputs_index_unified_observed, build_disk_path_inputs_index_unified_retained,
    initialize_generation_from_built_index, initialize_vnext_generation_store,
    initialize_vnext_generation_store_streaming, open_vnext_published_generation,
    verify_generation, verify_vnext_generation_store,
};

fn temp_root(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "personalrag-shared-hydration-{label}-{}-{nonce}",
        std::process::id()
    ))
}

fn median(mut values: Vec<Duration>) -> Duration {
    values.sort_unstable();
    values[values.len() / 2]
}

fn remove(path: &Path) {
    let _ = fs::remove_dir_all(path);
}

fn make_inputs(root: &Path, docs: usize, payload_bytes: usize) -> Vec<DiskPathInput> {
    fs::create_dir_all(root).unwrap();
    let source = root.join("payload.txt");
    let mut text = String::from("shared hydration timeout marker ");
    text.push_str(&"x".repeat(payload_bytes));
    fs::write(&source, text.as_bytes()).unwrap();
    let size = fs::metadata(&source).unwrap().len();
    (0..docs)
        .map(|row| DiskPathInput {
            path: source.clone(),
            display_path: format!("virtual/group_{:03}/doc_{row:07}.txt", row % 251),
            size_bytes: size,
            content_path: Some(source.clone()),
            index_content: true,
        })
        .collect()
}

fn config<'a>(options: &'a BuildOptions, cancel: &'a AtomicBool) -> DiskPathBuildConfig<'a> {
    DiskPathBuildConfig {
        max_docs: None,
        max_file_bytes: 16 * 1024 * 1024,
        build: options,
        scan_workers: 4,
        hydration_batch_bytes: 64 * 1024 * 1024,
        cancel: Some(cancel),
    }
}

fn finalize_perf_generation(
    perf_root: &Path,
    base_index: &Path,
    paths: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    let identities = paths
        .iter()
        .enumerate()
        .map(|(row, path)| LogicalDocumentIdentity::new(row as u64 + 1, path.clone(), path.clone()))
        .collect::<Vec<_>>();
    initialize_generation_from_built_index(perf_root, base_index, &identities)?;
    verify_generation(perf_root)?;
    Ok(())
}

fn run_legacy(
    corpus: &Path,
    inputs: Vec<DiskPathInput>,
    options: &BuildOptions,
) -> Result<(Duration, Vec<u64>), Box<dyn std::error::Error>> {
    let out = temp_root("legacy");
    let base = out.join("base-index");
    let vnext = out.join("vnext-store");
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let report = build_disk_path_inputs_index_unified(
        corpus,
        inputs,
        &base,
        config(options, &cancel),
        AccelerationProfile::Full,
        |_| {},
    )?;
    finalize_perf_generation(&out, &base, &report.display_paths)?;
    let perf = MergedIndex::open(&out, true)?;
    let docs = perf
        .live_documents()?
        .into_iter()
        .map(|doc| {
            VNextDocumentInput::new(
                doc.logical_id,
                doc.document.display_path,
                doc.document.normalized_content,
            )
        })
        .collect::<Vec<_>>();
    initialize_vnext_generation_store(&vnext, &docs, 5_000)?;
    verify_vnext_generation_store(&vnext)?;
    let hits = open_vnext_published_generation(&vnext)?.search_content(b"timeout")?;
    let elapsed = started.elapsed();
    remove(&out);
    Ok((elapsed, hits))
}

fn run_shared(
    corpus: &Path,
    inputs: Vec<DiskPathInput>,
    options: &BuildOptions,
    vnext_workers: usize,
) -> Result<(Duration, Vec<u64>), Box<dyn std::error::Error>> {
    let out = temp_root("shared");
    let base = out.join("base-index");
    let vnext = out.join("vnext-store");
    let cancel = AtomicBool::new(false);
    let started = Instant::now();

    let report = thread::scope(|scope| -> Result<_, Box<dyn std::error::Error>> {
        let (tx, rx) = mpsc::sync_channel::<VNextDocumentInput>(16);
        let vnext_handle = scope.spawn(|| {
            initialize_vnext_generation_store_streaming(&vnext, rx, 5_000, vnext_workers)
        });
        let mut logical_id = 1u64;
        let report = build_disk_path_inputs_index_unified_observed(
            corpus,
            inputs,
            &base,
            config(options, &cancel),
            AccelerationProfile::Full,
            |_| {},
            |doc| {
                let item = VNextDocumentInput::new(
                    logical_id,
                    doc.display_path.clone(),
                    doc.normalized_content.clone(),
                );
                logical_id += 1;
                tx.send(item).map_err(|_| {
                    personalrag_portable_search::SearchError::Format(
                        "shared hydration receiver closed".into(),
                    )
                })
            },
        )?;
        drop(tx);
        vnext_handle
            .join()
            .map_err(|_| "vNext shared hydration thread panicked")??;
        Ok(report)
    })?;
    finalize_perf_generation(&out, &base, &report.display_paths)?;
    verify_vnext_generation_store(&vnext)?;
    let hits = open_vnext_published_generation(&vnext)?.search_content(b"timeout")?;
    let elapsed = started.elapsed();
    remove(&out);
    Ok((elapsed, hits))
}

fn run_captured(
    corpus: &Path,
    inputs: Vec<DiskPathInput>,
    options: &BuildOptions,
) -> Result<(Duration, Vec<u64>), Box<dyn std::error::Error>> {
    let out = temp_root("captured");
    let base = out.join("base-index");
    let vnext = out.join("vnext-store");
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let mut captured = Vec::<VNextDocumentInput>::with_capacity(inputs.len());
    let mut logical_id = 1u64;
    let report = build_disk_path_inputs_index_unified_observed(
        corpus,
        inputs,
        &base,
        config(options, &cancel),
        AccelerationProfile::Full,
        |_| {},
        |doc| {
            captured.push(VNextDocumentInput::new(
                logical_id,
                doc.display_path.clone(),
                doc.normalized_content.clone(),
            ));
            logical_id += 1;
            Ok(())
        },
    )?;
    finalize_perf_generation(&out, &base, &report.display_paths)?;
    initialize_vnext_generation_store(&vnext, &captured, 5_000)?;
    verify_vnext_generation_store(&vnext)?;
    let hits = open_vnext_published_generation(&vnext)?.search_content(b"timeout")?;
    let elapsed = started.elapsed();
    remove(&out);
    Ok((elapsed, hits))
}

fn run_retained(
    corpus: &Path,
    inputs: Vec<DiskPathInput>,
    options: &BuildOptions,
) -> Result<(Duration, Vec<u64>), Box<dyn std::error::Error>> {
    let out = temp_root("retained");
    let base = out.join("base-index");
    let vnext = out.join("vnext-store");
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let (report, retained) = build_disk_path_inputs_index_unified_retained(
        corpus,
        inputs,
        &base,
        config(options, &cancel),
        AccelerationProfile::Full,
        |_| {},
    )?;
    finalize_perf_generation(&out, &base, &report.display_paths)?;
    let docs = retained
        .into_iter()
        .enumerate()
        .map(|(row, doc)| {
            VNextDocumentInput::new(row as u64 + 1, doc.display_path, doc.normalized_content)
        })
        .collect::<Vec<_>>();
    initialize_vnext_generation_store(&vnext, &docs, 5_000)?;
    verify_vnext_generation_store(&vnext)?;
    let hits = open_vnext_published_generation(&vnext)?.search_content(b"timeout")?;
    let elapsed = started.elapsed();
    remove(&out);
    Ok((elapsed, hits))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let docs = std::env::var("PR_BENCH_DOCS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(20_000);
    let payload_bytes = std::env::var("PR_BENCH_PAYLOAD_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(512);
    let vnext_workers = std::env::var("PR_VNEXT_WORKERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(2);
    let rounds = std::env::var("PR_BENCH_ROUNDS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(3)
        .max(1);
    let corpus = temp_root("corpus");
    let inputs = make_inputs(&corpus, docs, payload_bytes);
    let options = BuildOptions {
        mode: BuildMode::Adaptive,
        segment_docs: 5_000,
        workers: 4,
    };

    // Warm filesystem cache and code paths with a small throwaway run.
    let _ = run_shared(
        &corpus,
        inputs[..inputs.len().min(2_000)].to_vec(),
        &options,
        vnext_workers,
    )?;

    let mut legacy = Vec::new();
    let mut shared = Vec::new();
    let mut captured = Vec::new();
    let mut retained = Vec::new();
    let mut expected = None::<Vec<u64>>;
    for round in 0..rounds {
        let (legacy_elapsed, legacy_hits) = run_legacy(&corpus, inputs.clone(), &options)?;
        let (shared_elapsed, shared_hits) =
            run_shared(&corpus, inputs.clone(), &options, vnext_workers)?;
        let (captured_elapsed, captured_hits) = run_captured(&corpus, inputs.clone(), &options)?;
        let (retained_elapsed, retained_hits) = run_retained(&corpus, inputs.clone(), &options)?;
        assert_eq!(legacy_hits, shared_hits);
        assert_eq!(legacy_hits, captured_hits);
        assert_eq!(legacy_hits, retained_hits);
        if let Some(expected) = &expected {
            assert_eq!(&legacy_hits, expected);
        } else {
            expected = Some(legacy_hits);
        }
        legacy.push(legacy_elapsed);
        shared.push(shared_elapsed);
        captured.push(captured_elapsed);
        retained.push(retained_elapsed);
        eprintln!(
            "round={} legacy_ms={:.3} shared_ms={:.3} captured_ms={:.3} retained_ms={:.3}",
            round + 1,
            legacy_elapsed.as_secs_f64() * 1e3,
            shared_elapsed.as_secs_f64() * 1e3,
            captured_elapsed.as_secs_f64() * 1e3,
            retained_elapsed.as_secs_f64() * 1e3
        );
    }
    let legacy = median(legacy);
    let shared = median(shared);
    let captured = median(captured);
    let retained = median(retained);
    println!(
        "SHARED_HYDRATION docs={docs} payload_bytes={payload_bytes} vnext_workers={vnext_workers} legacy_ms={:.6} shared_ms={:.6} shared_speedup={:.3} captured_ms={:.6} captured_speedup={:.3} retained_ms={:.6} retained_speedup={:.3}",
        legacy.as_secs_f64() * 1e3,
        shared.as_secs_f64() * 1e3,
        legacy.as_secs_f64() / shared.as_secs_f64().max(f64::EPSILON),
        captured.as_secs_f64() * 1e3,
        legacy.as_secs_f64() / captured.as_secs_f64().max(f64::EPSILON),
        retained.as_secs_f64() * 1e3,
        legacy.as_secs_f64() / retained.as_secs_f64().max(f64::EPSILON)
    );
    remove(&corpus);
    Ok(())
}
