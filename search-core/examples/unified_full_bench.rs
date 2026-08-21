use personalrag_portable_search::{
    AccelerationProfile, BuildMode, BuildOptions, DocumentInput, Pos3Policy, PosCodec,
    build_index_benchmark, build_index_unified_benchmark, build_positional_sidecars,
    build_positional23_sidecars, build_q2_sidecars, fold_ascii,
};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

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

fn synthetic_source(n: usize) -> Vec<DocumentInput> {
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
    let mut docs = Vec::with_capacity(n);
    for index in 0..n {
        let ext = match index % 3 {
            0 => "rs",
            1 => "cpp",
            _ => "py",
        };
        let name = format!("module_{}_{}.{}", index % 300, random_ident(&mut rng), ext);
        let tokens = 80 + rng.index(100);
        let mut text = String::with_capacity(tokens * 10 + 64);
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
        docs.push(DocumentInput::new(
            name.clone(),
            name.clone(),
            fold_ascii(name.as_bytes()),
            fold_ascii(text.as_bytes()),
        ));
    }
    docs
}

fn name_only_source(n: usize) -> Vec<DocumentInput> {
    (0..n)
        .map(|index| {
            let name = format!(
                "src/module_{index:06}/component_{:08x}.cpp",
                index.wrapping_mul(2_654_435_761usize)
            );
            DocumentInput::new(
                name.clone(),
                name.clone(),
                fold_ascii(name.as_bytes()),
                Vec::new(),
            )
        })
        .collect()
}

fn main() {
    let args = env::args().collect::<Vec<_>>();
    let mode = args.get(1).map(String::as_str).unwrap_or("unified");
    let n = args
        .get(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000usize);
    let out = PathBuf::from(
        args.get(3)
            .cloned()
            .unwrap_or_else(|| format!("/tmp/pr-unified-{mode}")),
    );
    let _ = fs::remove_dir_all(&out);
    let corpus = args.get(4).map(String::as_str).unwrap_or("text");
    let docs = match corpus {
        "text" => synthetic_source(n),
        "name" => name_only_source(n),
        _ => panic!("corpus"),
    };
    let opts = BuildOptions {
        mode: BuildMode::Adaptive,
        segment_docs: 5_000,
        workers: 4,
    };
    let started = Instant::now();
    match mode {
        "legacy" => {
            build_index_benchmark(&docs, &out, &opts).unwrap();
            build_q2_sidecars(&out, false).unwrap();
            build_positional_sidecars(&out, PosCodec::production(), 500_000, false).unwrap();
            build_positional23_sidecars(
                &out,
                500_000,
                500_000,
                500_000,
                16,
                Pos3Policy::Adaptive,
                false,
            )
            .unwrap();
        }
        "unified" => {
            build_index_unified_benchmark(&docs, &out, &opts, AccelerationProfile::Full).unwrap();
        }
        _ => panic!("mode"),
    }
    let bytes = fs::read_dir(&out)
        .unwrap()
        .map(|e| fs::metadata(e.unwrap().path()).unwrap().len())
        .sum::<u64>();
    println!(
        "mode={mode} docs={n} elapsed_ms={:.3} bytes={bytes}",
        started.elapsed().as_secs_f64() * 1000.0
    );
}
