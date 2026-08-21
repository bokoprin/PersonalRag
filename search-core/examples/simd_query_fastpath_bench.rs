use personalrag_portable_search::{
    VNextDocumentInput, VNextGenerationIndex, VNextGenerationLayerSpec, fold_ascii,
    write_vnext_segment,
};
use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn percentile(samples: &mut [Duration], p: f64) -> f64 {
    samples.sort_unstable();
    let index = ((samples.len() - 1) as f64 * p).round() as usize;
    samples[index].as_secs_f64() * 1000.0
}

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    let docs = args
        .get(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000usize);
    let rounds = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(101usize);
    let root = PathBuf::from(
        args.get(3)
            .cloned()
            .unwrap_or_else(|| "/tmp/personalrag-simd-query-fastpath".into()),
    );
    assert!((8_192..=60_000).contains(&docs) && docs % 4 == 0);
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();

    let content = format!("{}aaab", "a".repeat(124));
    let mut paths = Vec::new();
    for shard in 0..4 {
        let begin = shard * (docs / 4);
        let end = begin + docs / 4;
        let segment = root.join(format!("base-{shard}.prseg2"));
        let inputs = (begin..end)
            .map(|id| {
                VNextDocumentInput::new(
                    id as u64 + 1,
                    format!("doc_{id:06}.txt"),
                    fold_ascii(content.as_bytes()),
                )
            })
            .collect::<Vec<_>>();
        write_vnext_segment(&segment, &inputs).unwrap();
        paths.push(segment);
    }
    let generation =
        VNextGenerationIndex::open(0, &[VNextGenerationLayerSpec::base(0, paths.iter())]).unwrap();
    let query = b"aaab";
    let warm = generation.search_content(query).unwrap();
    assert_eq!(warm.len(), docs);

    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let started = Instant::now();
        let hits = generation.search_content(query).unwrap();
        black_box(&hits);
        assert_eq!(hits.len(), docs);
        samples.push(started.elapsed());
    }
    let mut p50_samples = samples.clone();
    println!(
        "SIMD_FIND_FROM docs={} query=aaab hits={} p50_ms={:.6} p95_ms={:.6}",
        docs,
        warm.len(),
        percentile(&mut p50_samples, 0.50),
        percentile(&mut samples, 0.95),
    );
}
