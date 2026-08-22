use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use personalrag_portable_search::{
    AccelerationProfile, BuildMode, BuildOptions, DocumentInput, build_index_unified_benchmark,
};

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

fn update_hash(hash: &mut u64, bytes: &[u8]) {
    for &byte in bytes {
        *hash = (*hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME);
    }
}

fn hash_output(path: &Path) -> u64 {
    let mut files = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    files.sort();
    let mut hash = FNV_OFFSET;
    for file in files {
        update_hash(&mut hash, file.file_name().unwrap().to_string_lossy().as_bytes());
        update_hash(&mut hash, &fs::read(file).unwrap());
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

fn make_documents(files: usize, bytes_per_file: usize) -> Vec<DocumentInput> {
    (0..files)
        .map(|index| {
            let path = format!("src/group_{:03}/module_{index:05}.rs", index % 128);
            let content = make_content(index, bytes_per_file);
            DocumentInput::new(path.clone(), path.clone(), path.to_ascii_lowercase(), content)
        })
        .collect()
}

fn temp_output(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!("personalrag-ab-core-{label}-{}-{nonce}", std::process::id()))
}

fn main() {
    let label = std::env::var("PR_AB_LABEL").unwrap_or_else(|_| "unknown".to_owned());
    let files = 12_000usize;
    let bytes_per_file = 16 * 1024usize;
    let cpus = std::thread::available_parallelism().map_or(1, usize::from);
    let workers = cpus.min(4).max(1);
    let documents = make_documents(files, bytes_per_file);
    let output = temp_output(&label);
    let options = BuildOptions { mode: BuildMode::Adaptive, segment_docs: 5_000, workers };
    let started = Instant::now();
    let report = build_index_unified_benchmark(&documents, &output, &options, AccelerationProfile::None).unwrap();
    let wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    let hash = hash_output(&output);
    println!("AB label={} wall_ms={:.3} files={} bytes_per_file={} workers={} segments={} index_mib={:.3} byte_hash={hash:016x}", label, wall_ms, files, bytes_per_file, workers, report.segments, report.index_bytes as f64 / (1024.0 * 1024.0));
    let _ = fs::remove_dir_all(output);
}
