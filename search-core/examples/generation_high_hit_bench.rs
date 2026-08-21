use std::env;
use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use personalrag_portable_search::{
    VNextDocumentInput, VNextGenerationIndex, VNextGenerationLayerSpec, fold_ascii,
    write_vnext_segment,
};

fn doc(id: u64, updated: bool) -> VNextDocumentInput {
    let hot = if id.is_multiple_of(50) {
        "cold-marker"
    } else {
        "hotphrase"
    };
    let generation = if updated { "updated" } else { "base" };
    VNextDocumentInput::new(
        id,
        format!("{generation}/group_{:03}/module_{id:06}.txt", id % 97),
        fold_ascii(
            format!(
                "timeout common {hot} payload generation={generation} logical_id={id:06} search marker"
            )
            .as_bytes(),
        ),
    )
}

fn percentile(mut samples: Vec<Duration>, numerator: usize, denominator: usize) -> f64 {
    samples.sort_unstable();
    let index = samples.len().saturating_sub(1).saturating_mul(numerator) / denominator;
    samples[index].as_secs_f64() * 1000.0
}

fn bench(index: &VNextGenerationIndex, query: &[u8], rounds: usize) -> (usize, f64, f64) {
    for _ in 0..20 {
        black_box(index.search_content(black_box(query)).unwrap());
    }
    let mut samples = Vec::with_capacity(rounds);
    let mut hits = 0usize;
    for _ in 0..rounds {
        let started = Instant::now();
        let result = index.search_content(black_box(query)).unwrap();
        samples.push(started.elapsed());
        hits = result.len();
        black_box(result);
    }
    let p50 = percentile(samples.clone(), 50, 100);
    let p95 = percentile(samples, 95, 100);
    (hits, p50, p95)
}

fn main() {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let docs = args
        .first()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(60_000);
    let changed = args
        .get(1)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(2_000)
        .min(docs / 4);
    let rounds = args
        .get(2)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1_001);
    let root = args.get(3).map_or_else(
        || env::temp_dir().join(format!("pr-generation-high-hit-{}", std::process::id())),
        PathBuf::from,
    );
    assert!(docs >= 20_000 && changed > 0 && rounds > 0);
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();

    let base = (1..=docs as u64)
        .map(|id| doc(id, false))
        .collect::<Vec<_>>();
    let split = base.len() / 2;
    let base_a = root.join("base-a.prseg2");
    let base_b = root.join("base-b.prseg2");
    write_vnext_segment(&base_a, &base[..split]).unwrap();
    write_vnext_segment(&base_b, &base[split..]).unwrap();

    let delete_count = changed / 2;
    let mut tombstones = (1..=changed as u64).collect::<Vec<_>>();
    tombstones.extend((changed as u64 + 1)..=(changed + delete_count) as u64);
    tombstones.sort_unstable();
    tombstones.dedup();
    let mut delta = (1..=changed as u64)
        .map(|id| doc(id, true))
        .collect::<Vec<_>>();
    delta.extend((1..=delete_count as u64).map(|offset| doc(docs as u64 + offset, true)));
    let delta_path = root.join("delta.prseg2");
    write_vnext_segment(&delta_path, &delta).unwrap();

    let index = VNextGenerationIndex::open(
        1,
        &[
            VNextGenerationLayerSpec::base(0, [&base_a, &base_b]),
            VNextGenerationLayerSpec::delta(1, [&delta_path], tombstones),
        ],
    )
    .unwrap();
    assert_eq!(index.live_docs(), docs);

    for (label, query) in [
        ("all", b"timeout common".as_slice()),
        ("hot98", b"hotphrase".as_slice()),
    ] {
        let (hits, p50, p95) = bench(&index, query, rounds);
        println!(
            "GEN_HIGH_HIT label={label} docs={docs} changed={changed} rounds={rounds} hits={hits} p50_ms={p50:.6} p95_ms={p95:.6}"
        );
    }
}
