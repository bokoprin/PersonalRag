use std::env;
use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use personalrag_portable_search::{
    VNextDocumentInput, VNextGenerationIndex, VNextGenerationLayerSpec, fold_ascii,
    write_vnext_segment,
};

fn doc(id: u64, updated: bool, filler: &str) -> VNextDocumentInput {
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
                "timeout common {hot} payload {filler} generation={generation} logical_id={id:06} search marker"
            )
            .as_bytes(),
        ),
    )
}

fn percentile(mut samples: Vec<Duration>, n: usize) -> f64 {
    samples.sort_unstable();
    samples[samples.len().saturating_sub(1) * n / 100].as_secs_f64() * 1000.0
}

fn bench(index: &VNextGenerationIndex, query: &[u8], rounds: usize) -> (usize, f64, f64) {
    for _ in 0..10 {
        black_box(index.search_content(black_box(query)).unwrap());
    }
    let mut samples = Vec::with_capacity(rounds);
    let mut hits = 0;
    for _ in 0..rounds {
        let started = Instant::now();
        let result = index.search_content(black_box(query)).unwrap();
        samples.push(started.elapsed());
        hits = result.len();
        black_box(result);
    }
    (
        hits,
        percentile(samples.clone(), 50),
        percentile(samples, 95),
    )
}

fn main() {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let docs = args
        .first()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30_000usize);
    let changed = args
        .get(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(1_000usize);
    let rounds = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(201usize);
    let root = args.get(3).map_or_else(
        || env::temp_dir().join(format!("pr-ghh-long-{}", std::process::id())),
        PathBuf::from,
    );
    let filler = "abcdefghijklmnopqrstuvwxyz0123456789".repeat(12);
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();

    let base = (1..=docs as u64)
        .map(|id| doc(id, false, &filler))
        .collect::<Vec<_>>();
    let split = base.len() / 2;
    let a = root.join("a.prseg2");
    let b = root.join("b.prseg2");
    write_vnext_segment(&a, &base[..split]).unwrap();
    write_vnext_segment(&b, &base[split..]).unwrap();

    let delete_count = changed / 2;
    let mut tombstones = (1..=changed as u64).collect::<Vec<_>>();
    tombstones.extend((changed as u64 + 1)..=(changed + delete_count) as u64);
    tombstones.sort_unstable();
    tombstones.dedup();
    let mut delta = (1..=changed as u64)
        .map(|id| doc(id, true, &filler))
        .collect::<Vec<_>>();
    delta.extend((1..=delete_count as u64).map(|off| doc(docs as u64 + off, true, &filler)));
    let d = root.join("d.prseg2");
    write_vnext_segment(&d, &delta).unwrap();

    let index = VNextGenerationIndex::open(
        1,
        &[
            VNextGenerationLayerSpec::base(0, [&a, &b]),
            VNextGenerationLayerSpec::delta(1, [&d], tombstones),
        ],
    )
    .unwrap();
    for (label, query) in [
        ("all", b"timeout common".as_slice()),
        ("hot98", b"hotphrase".as_slice()),
    ] {
        let (hits, p50, p95) = bench(&index, query, rounds);
        println!(
            "GEN_HIGH_HIT_LONG label={label} docs={docs} changed={changed} rounds={rounds} hits={hits} p50_ms={p50:.6} p95_ms={p95:.6}"
        );
    }
}
