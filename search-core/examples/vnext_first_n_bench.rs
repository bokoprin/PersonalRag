use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use personalrag_portable_search::{
    VNextDocumentInput, initialize_vnext_generation_store, open_vnext_published_generation,
};

fn percentile(samples: &mut [Duration], p: f64) -> f64 {
    samples.sort_unstable();
    let i = ((samples.len() - 1) as f64 * p).round() as usize;
    samples[i].as_secs_f64() * 1000.0
}

fn bench<T, F: FnMut() -> T>(rounds: usize, mut f: F) -> (T, f64, f64) {
    let expected = f();
    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let started = Instant::now();
        black_box(f());
        samples.push(started.elapsed());
    }
    let mut p50 = samples.clone();
    (
        expected,
        percentile(&mut p50, 0.5),
        percentile(&mut samples, 0.95),
    )
}

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    let docs = args
        .get(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000usize);
    let rounds = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(31usize);
    let limit = args
        .get(3)
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000usize);
    let root = PathBuf::from(
        args.get(4)
            .cloned()
            .unwrap_or_else(|| "/tmp/pr-vnext-first-n".into()),
    );
    let _ = fs::remove_dir_all(&root);

    let inputs = (0..docs)
        .map(|i| {
            let id = i as u64 + 1;
            let content = format!(
                "document {i} timeout common marker payload repeated search text {:08} tail",
                i % 10_000
            );
            VNextDocumentInput::new(
                id,
                format!("root/module_{i:06}_timeout.txt"),
                content.into_bytes(),
            )
        })
        .collect::<Vec<_>>();

    let started = Instant::now();
    initialize_vnext_generation_store(&root, &inputs, 5_000).unwrap();
    let build_ms = started.elapsed().as_secs_f64() * 1000.0;
    let index = open_vnext_published_generation(&root).unwrap();
    let order = index.live_logical_ids().to_vec();

    let (full, full_p50, full_p95) = bench(rounds, || index.search_content(b"timeout").unwrap());
    assert_eq!(full.len(), docs);

    let (adaptive, adaptive_p50, adaptive_p95) = bench(rounds, || {
        index
            .first_n_in_order(b"timeout", false, &order, limit)
            .unwrap()
    });
    assert_eq!(adaptive, order[..limit.min(order.len())]);

    let (path_adaptive, path_p50, path_p95) = bench(rounds, || {
        index
            .first_n_in_order(b"timeout.txt", true, &order, limit)
            .unwrap()
    });
    assert_eq!(path_adaptive.len(), limit.min(order.len()));

    let (legacy_both, legacy_both_p50, legacy_both_p95) = bench(rounds, || {
        let paths = index.search_path(b"timeout.txt").unwrap();
        let content = index.search_content(b"timeout").unwrap();
        let path_set = paths.into_iter().collect::<std::collections::HashSet<_>>();
        content
            .into_iter()
            .filter(|id| path_set.contains(id))
            .take(limit)
            .collect::<Vec<_>>()
    });
    assert_eq!(legacy_both.len(), limit.min(order.len()));
    let (adaptive_both, adaptive_both_p50, adaptive_both_p95) = bench(rounds, || {
        index
            .first_n_conjunctive_in_order(b"timeout.txt", b"timeout", &order, limit)
            .unwrap()
    });
    assert_eq!(adaptive_both, order[..limit.min(order.len())]);

    println!(
        "FIRST_N_BENCH docs={docs} limit={limit} rounds={rounds} build_ms={build_ms:.3} full_hits={} full_p50_ms={full_p50:.6} full_p95_ms={full_p95:.6} adaptive_hits={} adaptive_p50_ms={adaptive_p50:.6} adaptive_p95_ms={adaptive_p95:.6} path_adaptive_p50_ms={path_p50:.6} path_adaptive_p95_ms={path_p95:.6} legacy_both_p50_ms={legacy_both_p50:.6} legacy_both_p95_ms={legacy_both_p95:.6} adaptive_both_p50_ms={adaptive_both_p50:.6} adaptive_both_p95_ms={adaptive_both_p95:.6}",
        full.len(),
        adaptive.len(),
    );
}
