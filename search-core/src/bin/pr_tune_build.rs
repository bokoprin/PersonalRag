use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use personalrag_portable_search::{
    AccelerationProfile, BuildMode, BuildOptions, DiskPathBuildConfig, DiskPathInput,
    PersistentIndex, build_disk_path_inputs_index_unified,
};

const FNV_OFFSET: u64 = 1_469_598_103_934_665_603;
const FNV_PRIME: u64 = 1_099_511_628_211;

fn temp_root() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("personalrag-build-tune-{}-{nonce}", std::process::id()))
}

fn update_hash(hash: &mut u64, bytes: &[u8]) {
    for &byte in bytes {
        *hash = (*hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME);
    }
}

fn collect_tree_files(root: &Path, current: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(current).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            collect_tree_files(root, &path, out);
        } else {
            out.push(path.strip_prefix(root).unwrap().to_path_buf());
        }
    }
}

fn tree_hash(root: &Path) -> u64 {
    let mut files = Vec::new();
    collect_tree_files(root, root, &mut files);
    files.sort_by_key(|path| path.to_string_lossy().replace('\\', "/"));
    let mut hash = FNV_OFFSET;
    for relative in files {
        let portable = relative.to_string_lossy().replace('\\', "/");
        update_hash(&mut hash, portable.as_bytes());
        update_hash(&mut hash, &[0]);
        update_hash(&mut hash, &fs::read(root.join(&relative)).unwrap());
    }
    hash
}

fn make_content(file_index: usize, target_bytes: usize) -> Vec<u8> {
    let mut text = String::with_capacity(target_bytes + 256);
    let mut line = 0usize;
    while text.len() < target_bytes {
        let module = file_index % 997;
        let symbol = (file_index.wrapping_mul(31).wrapping_add(line * 17)) % 8191;
        let value = file_index.wrapping_mul(1_103_515_245).wrapping_add(line * 12_345);
        let _ = writeln!(
            text,
            "pub fn module_{module:04}_symbol_{symbol:04}(input: usize) -> usize {{ let value = input.wrapping_mul(31).wrapping_add({value}); value ^ 0x5a5a5a5a }} // file={file_index} line={line}"
        );
        line += 1;
    }
    text.into_bytes()
}

fn build_inputs(corpus: &Path, files: usize, bytes_per_file: usize) -> Vec<DiskPathInput> {
    let started = Instant::now();
    fs::create_dir_all(corpus).unwrap();
    let mut inputs = Vec::with_capacity(files);
    for index in 0..files {
        let relative = format!("src/group_{:03}/module_{index:05}.rs", index % 128);
        let path = corpus.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let content = make_content(index, bytes_per_file);
        fs::write(&path, &content).unwrap();
        inputs.push(DiskPathInput {
            path,
            display_path: relative,
            size_bytes: content.len() as u64,
            content_path: None,
            index_content: true,
        });
    }
    println!(
        "CORPUS files={} bytes_per_file={} total_mib={:.1} create_ms={:.3}",
        files,
        bytes_per_file,
        (files as f64 * bytes_per_file as f64) / (1024.0 * 1024.0),
        started.elapsed().as_secs_f64() * 1000.0
    );
    inputs
}

fn run_build(
    root: &Path,
    inputs: &[DiskPathInput],
    work: &Path,
    segment_docs: usize,
    workers: usize,
    scan_workers: usize,
    iteration: usize,
) -> PathBuf {
    let output = work.join(format!("out-{segment_docs}-{workers}-{iteration}"));
    let _ = fs::remove_dir_all(&output);
    let options = BuildOptions {
        mode: BuildMode::Adaptive,
        segment_docs,
        workers,
    };
    let config = DiskPathBuildConfig {
        max_docs: None,
        max_file_bytes: 0,
        build: &options,
        scan_workers,
        hydration_batch_bytes: 128 * 1024 * 1024,
        cancel: None,
    };
    let started = Instant::now();
    let report = build_disk_path_inputs_index_unified(
        root,
        inputs.to_vec(),
        &output,
        config,
        AccelerationProfile::Balanced,
        |_| {},
    )
    .unwrap();
    let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    let byte_hash = tree_hash(&output);
    println!(
        "BUILD segment_docs={} workers={} iteration={} wall_ms={:.3} segments={} index_mib={:.3} byte_hash={:016x} hydration_ms={:.3} core_work_ms={:.3} content_grams_work_ms={:.3} content_post_work_ms={:.3} write_work_ms={:.3} accel_work_ms={:.3}",
        segment_docs,
        workers,
        iteration,
        wall_ms,
        report.build.segments,
        report.build.index_bytes as f64 / (1024.0 * 1024.0),
        byte_hash,
        report.timings.hydration_wall.as_secs_f64() * 1000.0,
        report.timings.segment_core_work.as_secs_f64() * 1000.0,
        report.timings.content_grams_work.as_secs_f64() * 1000.0,
        report.timings.content_post_work.as_secs_f64() * 1000.0,
        report.timings.segment_write_work.as_secs_f64() * 1000.0,
        report.timings.acceleration_work.as_secs_f64() * 1000.0,
    );
    output
}

fn verify_matrix(output: &Path) {
    for workers in [1usize, 2, 4, 8] {
        for iteration in 0..2 {
            let started = Instant::now();
            let index = PersistentIndex::open_with_workers(output, true, workers).unwrap();
            let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
            println!(
                "VERIFY workers={} iteration={} elapsed_ms={:.3} docs={}",
                workers,
                iteration,
                elapsed_ms,
                index.docs()
            );
        }
    }
}

fn main() {
    let cpus = std::thread::available_parallelism().map_or(1, usize::from);
    let files = std::env::var("PR_TUNE_FILES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(12_000usize);
    let bytes_per_file = std::env::var("PR_TUNE_BYTES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(16 * 1024usize);
    let root = temp_root();
    let corpus = root.join("corpus");
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    println!("SYSTEM cpus={cpus}");
    let inputs = build_inputs(&corpus, files, bytes_per_file);
    let scan_workers = cpus.min(8).max(1);

    // Warm filesystem/source cache with the current production geometry.
    let warm = run_build(&corpus, &inputs, &work, 5_000, 4.min(cpus), scan_workers, 0);
    verify_matrix(&warm);

    let current_workers = 4.min(cpus).max(1);
    for (segment_docs, workers) in [
        (2_500usize, current_workers),
        (5_000usize, current_workers),
        (10_000usize, current_workers),
        (15_000usize, current_workers),
    ] {
        for iteration in 1..=2 {
            let _ = run_build(
                &corpus,
                &inputs,
                &work,
                segment_docs,
                workers,
                scan_workers,
                iteration,
            );
        }
    }

    let _ = fs::remove_dir_all(&root);
}
