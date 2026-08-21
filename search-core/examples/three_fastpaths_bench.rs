use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use personalrag_portable_search::{
    VNextDocumentInput, fold_ascii, initialize_vnext_generation_store,
    initialize_vnext_generation_store_streaming, open_vnext_published_generation,
};

fn temp_root(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "personalrag-three-fastpaths-{label}-{}-{nonce}",
        std::process::id()
    ))
}

fn median(mut values: Vec<Duration>) -> Duration {
    values.sort_unstable();
    values[values.len() / 2]
}

fn regex_like(content: &[u8]) -> bool {
    let error = b"error";
    let timeout = b"timeout";
    let Some(error_at) = content.windows(error.len()).position(|w| w == error) else {
        return false;
    };
    content[error_at + error.len()..]
        .windows(timeout.len())
        .any(|w| w == timeout)
}

fn remove(path: &Path) {
    let _ = fs::remove_dir_all(path);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let docs_count = std::env::var("PR_BENCH_DOCS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(50_000);
    let rounds = std::env::var("PR_BENCH_ROUNDS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(9)
        .max(3);
    let limit = 2_000usize.min(docs_count.max(1));
    let payload_bytes = std::env::var("PR_BENCH_PAYLOAD_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(64);
    let filler = "x".repeat(payload_bytes);

    let docs = (0..docs_count)
        .map(|row| {
            let logical_id = row as u64 + 1;
            let marker = if row % 100 == 0 {
                format!("document {row} error transient {filler} timeout final marker")
            } else {
                format!("document {row} common timeout {filler} ordinary marker")
            };
            VNextDocumentInput::new(
                logical_id,
                format!("group/{:03}/report_{row:07}.txt", row % 251),
                fold_ascii(marker.as_bytes()),
            )
        })
        .collect::<Vec<_>>();

    let root = temp_root("search");
    initialize_vnext_generation_store(&root, &docs, 5_000)?;
    let index = open_vnext_published_generation(&root)?;

    // Simulate a GUI size-desc sort order. Ties use path/document order.
    let mut order = (1..=docs_count as u64).collect::<Vec<_>>();
    order.sort_unstable_by(|left, right| {
        let l = (*left - 1) as usize;
        let r = (*right - 1) as usize;
        let lsize = (l.wrapping_mul(17) % 4096) as u64;
        let rsize = (r.wrapping_mul(17) % 4096) as u64;
        rsize.cmp(&lsize).then_with(|| left.cmp(right))
    });
    let mut rank = vec![usize::MAX; docs_count + 1];
    for (pos, logical_id) in order.iter().copied().enumerate() {
        rank[logical_id as usize] = pos;
    }

    let expected = index.first_n_in_order(b"timeout", false, &order, limit)?;
    let mut legacy_times = Vec::new();
    let mut fast_times = Vec::new();
    for _ in 0..rounds {
        let started = Instant::now();
        let mut hits = index.search_content(b"timeout")?;
        hits.sort_unstable_by_key(|logical_id| rank[*logical_id as usize]);
        hits.truncate(limit);
        legacy_times.push(started.elapsed());
        assert_eq!(hits, expected);

        let started = Instant::now();
        let hits = index.first_n_in_order(b"timeout", false, &order, limit)?;
        fast_times.push(started.elapsed());
        assert_eq!(hits, expected);
    }
    let legacy = median(legacy_times);
    let fast = median(fast_times);

    let mut full_regex_times = Vec::new();
    let mut prefilter_regex_times = Vec::new();
    let expected_regex = docs
        .iter()
        .filter(|doc| regex_like(&doc.normalized_content))
        .map(|doc| doc.logical_id)
        .collect::<Vec<_>>();
    let prefilter_candidates = index.search_content(b"error")?;
    for _ in 0..rounds {
        let started = Instant::now();
        let hits = docs
            .iter()
            .filter(|doc| regex_like(&doc.normalized_content))
            .map(|doc| doc.logical_id)
            .collect::<Vec<_>>();
        full_regex_times.push(started.elapsed());
        assert_eq!(hits, expected_regex);

        let started = Instant::now();
        let candidates = index.search_content(b"error")?;
        let hits = candidates
            .into_iter()
            .filter(|logical_id| regex_like(&docs[*logical_id as usize - 1].normalized_content))
            .collect::<Vec<_>>();
        prefilter_regex_times.push(started.elapsed());
        assert_eq!(hits, expected_regex);
    }
    let regex_full = median(full_regex_times);
    let regex_prefilter = median(prefilter_regex_times);

    // Isolate durable vNext segment publication scaling using the same deterministic stream.
    let mut worker_results = Vec::new();
    for workers in [1usize, 2, 4] {
        let mut samples = Vec::new();
        for round in 0..3 {
            let build_root = temp_root(&format!("workers-{workers}-{round}"));
            let started = Instant::now();
            initialize_vnext_generation_store_streaming(&build_root, docs.clone(), 5_000, workers)?;
            samples.push(started.elapsed());
            remove(&build_root);
        }
        worker_results.push((workers, median(samples)));
    }

    println!(
        "THREE_FASTPATHS docs={docs_count} rounds={rounds} limit={limit} payload_bytes={payload_bytes}"
    );
    println!(
        "SORT_FIRST_N legacy_ms={:.6} fast_ms={:.6} speedup={:.3}",
        legacy.as_secs_f64() * 1e3,
        fast.as_secs_f64() * 1e3,
        legacy.as_secs_f64() / fast.as_secs_f64().max(f64::EPSILON)
    );
    println!(
        "REGEX_PREFILTER full_scan_ms={:.6} prefilter_ms={:.6} speedup={:.3} candidates={} matches={}",
        regex_full.as_secs_f64() * 1e3,
        regex_prefilter.as_secs_f64() * 1e3,
        regex_full.as_secs_f64() / regex_prefilter.as_secs_f64().max(f64::EPSILON),
        prefilter_candidates.len(),
        expected_regex.len()
    );
    for (workers, elapsed) in worker_results {
        println!(
            "VNEXT_STREAM_BUILD workers={workers} ms={:.6}",
            elapsed.as_secs_f64() * 1e3
        );
    }

    remove(&root);
    Ok(())
}
