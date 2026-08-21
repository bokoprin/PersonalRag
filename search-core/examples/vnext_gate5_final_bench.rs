use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use personalrag_portable_search::{
    AccelerationProfile, BuildMode, BuildOptions, DocumentInput, LogicalDocumentIdentity,
    MergedIndex, PlannedUpsert, UpdatePlan, VNextDocumentInput, build_index_unified_benchmark,
    compact_generation_unified, compact_vnext_generation_store, fold_ascii,
    gc_vnext_generation_store, initialize_generation_from_built_index,
    initialize_vnext_generation_store, open_vnext_published_generation,
    publish_incremental_update_unified, publish_vnext_incremental_generation,
};

#[derive(Clone, Copy)]
struct FastRng(u64);
impl FastRng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let mut value = self.0;
        value ^= value >> 21;
        value ^= value << 35;
        value ^= value >> 4;
        value
    }
    fn index(&mut self, len: usize) -> usize {
        (self.next() as usize) % len
    }
}
fn random_ident(rng: &mut FastRng) -> String {
    let len = 4 + rng.index(11);
    let mut out = String::with_capacity(len);
    for _ in 0..len {
        out.push((b'a' + rng.index(26) as u8) as char);
    }
    out
}

fn corpus(
    n: usize,
) -> (
    Vec<DocumentInput>,
    Vec<VNextDocumentInput>,
    Vec<LogicalDocumentIdentity>,
) {
    const KEYWORDS: &[&str] = &[
        "return",
        "error",
        "timeout",
        "indexmanager",
        "personalrag",
        "redmine",
        "config",
        "request",
        "response",
        "vector",
        "string",
        "worker",
        "cache",
        "search",
        "document",
        "metadata",
        "content",
        "result",
        "parser",
        "builder",
    ];
    const PUNCT: &[&str] = &[
        " ", "\n", "::", "->", "(", ")", " { ", ";\n", " = ", ", ", "_", ".",
    ];
    let mut rng = FastRng::new(0x5052_5345_4152_4348);
    let mut perf = Vec::with_capacity(n);
    let mut vnext = Vec::with_capacity(n);
    let mut identities = Vec::with_capacity(n);
    for index in 0..n {
        let ext = match index % 3 {
            0 => "rs",
            1 => "cpp",
            _ => "py",
        };
        let name = format!("module_{}_{}.{}", index % 300, random_ident(&mut rng), ext);
        let mut normalized_content = if index == 0 {
            let mut bytes = vec![b'x'; 8190];
            bytes.extend_from_slice(b"boundary_cross_marker_20260817");
            bytes
        } else {
            let tokens = 80 + rng.index(100);
            let mut text = String::with_capacity(tokens * 10 + 96);
            for _ in 0..tokens {
                if rng.index(10) < 7 {
                    text.push_str(KEYWORDS[rng.index(KEYWORDS.len())]);
                } else {
                    text.push_str(&random_ident(&mut rng));
                }
                text.push_str(PUNCT[rng.index(PUNCT.len())]);
            }
            if index % 97 == 0 {
                text.push_str(&format!(" unique_marker_{index}::deep_timeout_path "));
            }
            if index % 211 == 0 {
                text.push_str(" 日本語検索マーカー ");
            }
            fold_ascii(text.as_bytes())
        };
        if index == 1 {
            normalized_content.extend_from_slice(b" timeout deep_timeout_path ");
        }
        let logical_id = index as u64 + 1;
        let normalized_name = fold_ascii(name.as_bytes());
        perf.push(DocumentInput::new(
            name.clone(),
            name.clone(),
            normalized_name,
            normalized_content.clone(),
        ));
        vnext.push(VNextDocumentInput::new(
            logical_id,
            name.clone(),
            normalized_content,
        ));
        identities.push(LogicalDocumentIdentity::new(logical_id, name.clone(), name));
    }
    (perf, vnext, identities)
}

fn updated_doc(id: u64, generation: u64) -> DocumentInput {
    let path = format!("updated/g{generation}/module_{id:05}.txt");
    let content = format!(
        "updated generation {generation} logical {id} delta_generation_{generation}_marker timeout 日本語検索"
    );
    DocumentInput::new(
        format!("key-{id}"),
        path.clone(),
        fold_ascii(path.as_bytes()),
        fold_ascii(content.as_bytes()),
    )
}

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
fn dir_bytes(path: &Path) -> u64 {
    if !path.exists() {
        return 0;
    }
    fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() && !meta.file_type().is_symlink() {
                dir_bytes(&path)
            } else {
                meta.len()
            }
        })
        .sum()
}

fn compare_queries(
    perf: &MergedIndex,
    vnext: &personalrag_portable_search::VNextGenerationIndex,
    rounds: usize,
    phase: &str,
) {
    let content: &[(&str, &[u8])] = &[
        ("q1", b"e"),
        ("q2", b"ti"),
        ("common", b"timeout"),
        ("medium", b"deep_timeout_path"),
        ("rare", b"unique_marker_970"),
        ("zero", b"zzzz_no_such_marker_20260817"),
        ("japanese", "日本語検索".as_bytes()),
        ("long", b"unique_marker_970::deep_timeout_path"),
        ("boundary", b"boundary_cross_marker_20260817"),
    ];
    for (label, query) in content {
        let (ph, pp50, pp95) = bench(rounds, || perf.search_content(query).unwrap());
        let (vh, vp50, vp95) = bench(rounds, || vnext.search_content(query).unwrap());
        assert_eq!(ph, vh, "Gate5 content mismatch {phase}/{label}");
        println!(
            "GATE5_QUERY phase={phase} label={label} hits={} perf_p50_ms={pp50:.6} perf_p95_ms={pp95:.6} vnext_p50_ms={vp50:.6} vnext_p95_ms={vp95:.6}",
            ph.len()
        );
    }
    for (label, query) in [
        ("filename", b"module_42_".as_slice()),
        ("path_zero", b"missing_name_marker".as_slice()),
    ] {
        let (ph, pp50, pp95) = bench(rounds, || perf.search_name(query).unwrap());
        let (vh, vp50, vp95) = bench(rounds, || vnext.search_path(query).unwrap());
        assert_eq!(ph, vh, "Gate5 path mismatch {phase}/{label}");
        println!(
            "GATE5_QUERY phase={phase} label={label} hits={} perf_p50_ms={pp50:.6} perf_p95_ms={pp95:.6} vnext_p50_ms={vp50:.6} vnext_p95_ms={vp95:.6}",
            ph.len()
        );
    }
}

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    let docs = args
        .get(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000usize);
    let rounds = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(31usize);
    let root = PathBuf::from(
        args.get(3)
            .cloned()
            .unwrap_or_else(|| "/tmp/pr-gate5-final".into()),
    );
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let perf_built = root.join("perf-built");
    let perf_root = root.join("perf-generation");
    let vnext_root = root.join("vnext-generation");
    let (perf_docs, vnext_docs, identities) = corpus(docs);
    let options = BuildOptions {
        mode: BuildMode::Adaptive,
        segment_docs: 5_000,
        workers: 4,
    };

    let started = Instant::now();
    build_index_unified_benchmark(&perf_docs, &perf_built, &options, AccelerationProfile::Full)
        .unwrap();
    initialize_generation_from_built_index(&perf_root, &perf_built, &identities).unwrap();
    let perf_build_ms = started.elapsed().as_secs_f64() * 1000.0;
    let perf_bytes = dir_bytes(&perf_root);

    let started = Instant::now();
    let vreport = initialize_vnext_generation_store(&vnext_root, &vnext_docs, 5_000).unwrap();
    let vnext_build_ms = started.elapsed().as_secs_f64() * 1000.0;
    let vnext_bytes = dir_bytes(&vnext_root);
    println!(
        "GATE5_BUILD docs={docs} perf_ms={perf_build_ms:.3} perf_bytes={perf_bytes} vnext_ms={vnext_build_ms:.3} vnext_bytes={vnext_bytes} vnext_segments={}",
        vreport.segment_count
    );

    let mut perf_open_samples = Vec::new();
    let mut vnext_open_samples = Vec::new();
    for _ in 0..9 {
        let t = Instant::now();
        black_box(MergedIndex::open(&perf_root, true).unwrap());
        perf_open_samples.push(t.elapsed());
        let t = Instant::now();
        black_box(open_vnext_published_generation(&vnext_root).unwrap());
        vnext_open_samples.push(t.elapsed());
    }
    let perf_open_p50 = percentile(&mut perf_open_samples, 0.5);
    let vnext_open_p50 = percentile(&mut vnext_open_samples, 0.5);
    println!(
        "GATE5_OPEN docs={docs} perf_p50_ms={perf_open_p50:.3} vnext_p50_ms={vnext_open_p50:.3}"
    );

    let perf = MergedIndex::open(&perf_root, true).unwrap();
    let vnext = open_vnext_published_generation(&vnext_root).unwrap();
    compare_queries(&perf, &vnext, rounds, "base");
    drop(perf);
    drop(vnext);

    let change_counts = [1usize, 10, 100, 1_000];
    for (i, changes) in change_counts.into_iter().enumerate() {
        let generation = i as u64 + 1;
        let start_id = 2_000u64 * generation + 1;
        let ids = (0..changes)
            .map(|offset| ((start_id + offset as u64 - 1) % docs as u64) + 1)
            .collect::<Vec<_>>();
        let upserts = ids
            .iter()
            .copied()
            .map(|id| PlannedUpsert {
                logical_id: id,
                is_insert: false,
                document: updated_doc(id, generation),
            })
            .collect::<Vec<_>>();
        let mut tombstones = ids;
        tombstones.sort_unstable();
        tombstones.dedup();
        let plan = UpdatePlan {
            base_generation: generation - 1,
            next_generation: generation,
            upserts,
            tombstones,
            live_docs_after: docs,
            compaction_recommended: generation == 4,
        };
        let t = Instant::now();
        let perf_report = publish_incremental_update_unified(
            &perf_root,
            &plan,
            &BuildOptions {
                mode: BuildMode::Direct,
                segment_docs: 5_000,
                workers: 4,
            },
        )
        .unwrap();
        let perf_ms = t.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        let vnext_report = publish_vnext_incremental_generation(&vnext_root, &plan, 5_000).unwrap();
        let vnext_ms = t.elapsed().as_secs_f64() * 1000.0;
        let marker = format!("delta_generation_{generation}_marker");
        let ph = MergedIndex::open(&perf_root, true)
            .unwrap()
            .search_content(marker.as_bytes())
            .unwrap();
        let vh = open_vnext_published_generation(&vnext_root)
            .unwrap()
            .search_content(marker.as_bytes())
            .unwrap();
        assert_eq!(ph, vh);
        assert_eq!(ph.len(), changes);
        println!(
            "GATE5_DELTA generation={generation} changes={changes} perf_ms={perf_ms:.3} vnext_ms={vnext_ms:.3} perf_deltas={} vnext_layers={} segments={}",
            perf_report.delta_count, vnext_report.layer_count, vnext_report.segment_count
        );
    }

    let perf_before = MergedIndex::open(&perf_root, true).unwrap();
    let vnext_before = open_vnext_published_generation(&vnext_root).unwrap();
    for query in [
        b"timeout".as_slice(),
        b"delta_generation_4_marker",
        b"zzzz_no_such_marker_20260817",
    ] {
        assert_eq!(
            perf_before.search_content(query).unwrap(),
            vnext_before.search_content(query).unwrap()
        );
    }
    drop(perf_before);
    drop(vnext_before);

    let t = Instant::now();
    let perf_compact = compact_generation_unified(&perf_root, &options).unwrap();
    let perf_compact_ms = t.elapsed().as_secs_f64() * 1000.0;
    let t = Instant::now();
    let vnext_compact = compact_vnext_generation_store(&vnext_root, 5_000).unwrap();
    let vnext_compact_ms = t.elapsed().as_secs_f64() * 1000.0;
    let before_gc = dir_bytes(&vnext_root);
    let t = Instant::now();
    let gc = gc_vnext_generation_store(&vnext_root, Duration::ZERO).unwrap();
    let gc_ms = t.elapsed().as_secs_f64() * 1000.0;
    let after_gc = dir_bytes(&vnext_root);
    println!(
        "GATE5_COMPACTION perf_ms={perf_compact_ms:.3} vnext_ms={vnext_compact_ms:.3} perf_generation={} vnext_generation={} vnext_source_layers={} vnext_compacted_segments={} gc_ms={gc_ms:.3} gc_components={} gc_manifests={} gc_reclaimed_bytes={} store_before_gc={} store_after_gc={}",
        perf_compact.generation,
        vnext_compact.compacted_generation,
        vnext_compact.source_layer_count,
        vnext_compact.compacted_segment_count,
        gc.removed_component_dirs,
        gc.removed_manifest_files,
        gc.reclaimed_bytes,
        before_gc,
        after_gc
    );

    let perf = MergedIndex::open(&perf_root, true).unwrap();
    let vnext = open_vnext_published_generation(&vnext_root).unwrap();
    assert_eq!(perf.live_docs(), vnext.live_docs());
    compare_queries(&perf, &vnext, rounds, "compacted");
}
