use personalrag_portable_search::{
    VNextDocumentInput, initialize_vnext_generation_store,
    initialize_vnext_generation_store_streaming,
};
use std::fs;
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

fn temp_root(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "personalrag-vnext-build-{label}-{}-{nonce}",
        std::process::id()
    ))
}

fn docs(count: usize, payload_bytes: usize) -> Vec<VNextDocumentInput> {
    (0..count)
        .map(|row| {
            let path = format!("g{:03}/doc_{row:07}.txt", row % 127);
            let prefix = format!(
                "timeout marker row={row:07} group={:03} alpha={} beta={} ",
                row % 127,
                row.wrapping_mul(2_654_435_761usize) % 1_000_003,
                row.wrapping_mul(1_146_067_499usize) % 1_000_033
            );
            let mut bytes = prefix.into_bytes();
            while bytes.len() < payload_bytes {
                bytes.push(b'a' + ((row + bytes.len()) % 26) as u8);
            }
            bytes.truncate(payload_bytes.max(1));
            VNextDocumentInput::new(row as u64 + 1, path, bytes)
        })
        .collect()
}

fn main() {
    let count = std::env::var("PR_VNEXT_BENCH_DOCS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000usize);
    let payload = std::env::var("PR_VNEXT_BENCH_PAYLOAD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4_096usize);
    let seg = std::env::var("PR_VNEXT_BENCH_SEGMENT_DOCS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5_000usize);
    let root = std::env::var("PR_VNEXT_BENCH_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| temp_root("root"));
    let input = docs(count, payload);
    let started = Instant::now();
    let report = if let Some(workers) = std::env::var("PR_VNEXT_BENCH_STREAM_WORKERS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
    {
        initialize_vnext_generation_store_streaming(&root, input, seg, workers).unwrap()
    } else {
        initialize_vnext_generation_store(&root, &input, seg).unwrap()
    };
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    let bytes = dir_bytes(&root);
    println!(
        "VNEXT_BUILD_PROFILE docs={count} payload={payload} segments={} elapsed_ms={ms:.3} bytes={bytes}",
        report.segment_count
    );
    if std::env::var_os("PR_VNEXT_BENCH_KEEP").is_none() {
        let _ = fs::remove_dir_all(root);
    }
}
fn dir_bytes(path: &std::path::Path) -> u64 {
    let mut total = 0u64;
    let Ok(rd) = fs::read_dir(path) else { return 0 };
    for e in rd.flatten() {
        if let Ok(m) = e.metadata() {
            if m.is_dir() {
                total = total.saturating_add(dir_bytes(&e.path()));
            } else {
                total = total.saturating_add(m.len());
            }
        }
    }
    total
}
