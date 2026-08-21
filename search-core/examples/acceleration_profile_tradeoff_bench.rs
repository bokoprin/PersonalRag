use personalrag_portable_search::{
    AccelerationProfile, BuildMode, BuildOptions, DocumentInput, PersistentIndex,
    VNextDocumentInput, build_index_unified_benchmark, fold_ascii,
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
    let docs = std::env::var("PR_PROFILE_DOCS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000usize);
    let rounds = std::env::var("PR_QUERY_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(301usize);
    let root = PathBuf::from(
        std::env::var("PR_PROFILE_ROOT")
            .unwrap_or_else(|_| "/tmp/personalrag-accel-profile-tradeoff".into()),
    );
    let reuse_built = std::env::var_os("PR_REUSE_BUILT").is_some();
    if !reuse_built {
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
    }
    let (mut perf_docs, _) = synthetic_source(docs);
    let target_bytes = std::env::var("PR_PROFILE_PAYLOAD_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    if target_bytes > 0 {
        const FILL: &[u8] =
            b" return error timeout cache search document metadata content worker parser builder ";
        for document in &mut perf_docs {
            while document.normalized_content.len() < target_bytes {
                let remaining = target_bytes - document.normalized_content.len();
                document
                    .normalized_content
                    .extend_from_slice(&FILL[..remaining.min(FILL.len())]);
            }
        }
    }
    let options = BuildOptions {
        mode: BuildMode::Adaptive,
        segment_docs: 5_000,
        workers: 4,
    };
    let balanced_first = std::env::var_os("PR_BALANCED_FIRST").is_some();
    let profiles = if balanced_first {
        vec![
            ("balanced", AccelerationProfile::Balanced),
            ("full", AccelerationProfile::Full),
            ("adaptive", AccelerationProfile::AdaptiveDelta),
            ("none", AccelerationProfile::None),
        ]
    } else {
        vec![
            ("full", AccelerationProfile::Full),
            ("balanced", AccelerationProfile::Balanced),
            ("adaptive", AccelerationProfile::AdaptiveDelta),
            ("none", AccelerationProfile::None),
        ]
    };
    let mut opened = Vec::new();
    for (label, profile) in profiles {
        let dir = root.join(label);
        if !reuse_built {
            let started = Instant::now();
            build_index_unified_benchmark(&perf_docs, &dir, &options, profile).unwrap();
            let build_ms = started.elapsed().as_secs_f64() * 1000.0;
            println!("ACCEL_BUILD profile={label} docs={docs} build_ms={build_ms:.3}");
        }
        let index = PersistentIndex::open(&dir, true).unwrap();
        opened.push((label, index));
    }
    let queries: &[(&str, &[u8])] = &[
        ("q1", b"e"),
        ("q2", b"ti"),
        ("common", b"timeout"),
        ("medium", b"deep_timeout_path"),
        ("rare", b"unique_marker_970"),
        ("zero", b"zzzz_no_such_marker_20260817"),
        ("japanese", "日本語検索".as_bytes()),
        ("long", b"unique_marker_970::deep_timeout_path"),
    ];
    for (qlabel, query) in queries {
        let mut oracle: Option<Vec<u32>> = None;
        for (label, index) in &opened {
            let (hits, p50, p95) = bench(rounds, || index.search_content(query).unwrap());
            if let Some(expected) = &oracle {
                assert_eq!(&hits, expected, "query mismatch {qlabel} profile={label}");
            } else {
                oracle = Some(hits.clone());
            }
            println!(
                "ACCEL_QUERY query={qlabel} profile={label} hits={} p50_ms={p50:.6} p95_ms={p95:.6}",
                hits.len()
            );
        }
    }
}
