use personalrag_portable_search::{
    AccelerationProfile, BuildMode, BuildOptions, DocumentInput, PersistentIndex,
    VNextDocumentInput, VNextSegmentReader, build_index_unified_benchmark, fold_ascii,
    write_vnext_segment,
};
use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::{Duration, Instant};

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

fn synthetic_source(n: usize) -> (Vec<DocumentInput>, Vec<VNextDocumentInput>) {
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
    for index in 0..n {
        let ext = match index % 3 {
            0 => "rs",
            1 => "cpp",
            _ => "py",
        };
        let name = format!("module_{}_{}.{}", index % 300, random_ident(&mut rng), ext);
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
        let normalized_name = fold_ascii(name.as_bytes());
        let normalized_content = fold_ascii(text.as_bytes());
        perf.push(DocumentInput::new(
            name.clone(),
            name.clone(),
            normalized_name,
            normalized_content.clone(),
        ));
        vnext.push(VNextDocumentInput::new(
            index as u64,
            name,
            normalized_content,
        ));
    }
    (perf, vnext)
}

fn percentile(samples: &mut [Duration], percentile: f64) -> f64 {
    samples.sort_unstable();
    let index = ((samples.len() - 1) as f64 * percentile).round() as usize;
    samples[index].as_secs_f64() * 1000.0
}

fn bench<T, F>(rounds: usize, mut f: F) -> (T, f64, f64)
where
    F: FnMut() -> T,
{
    let first = f();
    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let started = Instant::now();
        black_box(f());
        samples.push(started.elapsed());
    }
    let mut p50_samples = samples.clone();
    let p50 = percentile(&mut p50_samples, 0.50);
    let p95 = percentile(&mut samples, 0.95);
    (first, p50, p95)
}

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    let docs = args
        .get(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(20_000usize);
    let rounds = args
        .get(2)
        .and_then(|value| value.parse().ok())
        .unwrap_or(15usize);
    let root = PathBuf::from(
        args.get(3)
            .cloned()
            .unwrap_or_else(|| "/tmp/personalrag-vnext-query-bench".into()),
    );
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let perf_dir = root.join("perf12");
    let vnext_path = root.join("segment.prseg2");
    let (perf_docs, vnext_docs) = synthetic_source(docs);
    let options = BuildOptions {
        mode: BuildMode::Adaptive,
        segment_docs: 5_000,
        workers: 4,
    };
    build_index_unified_benchmark(&perf_docs, &perf_dir, &options, AccelerationProfile::Full)
        .unwrap();
    write_vnext_segment(&vnext_path, &vnext_docs).unwrap();
    let perf = PersistentIndex::open(&perf_dir, true).unwrap();
    let vnext = VNextSegmentReader::open(&vnext_path).unwrap();

    let content_queries: &[(&str, &[u8])] = &[
        ("q1", b"e"),
        ("q2", b"ti"),
        ("common", b"timeout"),
        ("medium", b"deep_timeout_path"),
        ("rare", b"unique_marker_970"),
        ("zero", b"zzzz_no_such_marker_20260817"),
        ("japanese", "日本語検索".as_bytes()),
        ("long", b"unique_marker_970::deep_timeout_path"),
    ];
    for (label, query) in content_queries {
        let (perf_hits, perf_p50, perf_p95) = bench(rounds, || perf.search_content(query).unwrap());
        let (vnext_hits, vnext_p50, vnext_p95) =
            bench(rounds, || vnext.search_content(query).unwrap());
        let vnext_hits_u32 = vnext_hits.into_iter().map(u32::from).collect::<Vec<_>>();
        assert_eq!(perf_hits, vnext_hits_u32, "content query mismatch: {label}");
        let (_, diagnostics) = vnext.search_content_with_diagnostics(query).unwrap();
        println!(
            "QUERY label={label} hits={} perf_p50_ms={perf_p50:.6} perf_p95_ms={perf_p95:.6} vnext_p50_ms={vnext_p50:.6} vnext_p95_ms={vnext_p95:.6} mode={:?} anchor_blocks={} selected_anchors={} candidate_blocks={} verified_blocks={}",
            perf_hits.len(),
            diagnostics.mode,
            diagnostics.anchor_blocks,
            diagnostics.selected_anchor_count,
            diagnostics.candidate_blocks,
            diagnostics.verified_blocks,
        );
    }

    let name_query = b"module_42_";
    let (perf_hits, perf_p50, perf_p95) = bench(rounds, || perf.search_name(name_query).unwrap());
    let (vnext_hits, vnext_p50, vnext_p95) =
        bench(rounds, || vnext.search_path(name_query).unwrap());
    assert_eq!(
        perf_hits,
        vnext_hits.into_iter().map(u32::from).collect::<Vec<_>>()
    );
    println!(
        "QUERY label=filename hits={} perf_p50_ms={perf_p50:.6} perf_p95_ms={perf_p95:.6} vnext_p50_ms={vnext_p50:.6} vnext_p95_ms={vnext_p95:.6}",
        perf_hits.len()
    );
}
