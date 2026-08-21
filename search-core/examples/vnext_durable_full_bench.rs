use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use personalrag_portable_search::{
    VNextDocumentInput, fold_ascii, initialize_vnext_generation_store,
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

fn synthetic_source(n: usize) -> Vec<VNextDocumentInput> {
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
        docs.push(VNextDocumentInput::new(
            index as u64 + 1,
            name,
            fold_ascii(text.as_bytes()),
        ));
    }
    docs
}

fn dir_bytes(path: &Path) -> u64 {
    if !path.exists() {
        return 0;
    }
    let mut total = 0u64;
    for entry in fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        let metadata = fs::symlink_metadata(entry.path()).unwrap();
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            total = total.saturating_add(dir_bytes(&entry.path()));
        } else {
            total = total.saturating_add(metadata.len());
        }
    }
    total
}

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    let n = args
        .get(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000usize);
    let root = PathBuf::from(
        args.get(2)
            .cloned()
            .unwrap_or_else(|| "/tmp/pr-vnext-durable-full".into()),
    );
    let segment_docs = args
        .get(3)
        .and_then(|v| v.parse().ok())
        .unwrap_or(5_000usize);
    let _ = fs::remove_dir_all(&root);
    let docs = synthetic_source(n);
    let started = Instant::now();
    let report = initialize_vnext_generation_store(&root, &docs, segment_docs).unwrap();
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
    let bytes = dir_bytes(&root);
    println!(
        "VNEXT_DURABLE_FULL docs={n} segments={} elapsed_ms={elapsed_ms:.3} bytes={bytes}",
        report.segment_count
    );
}
