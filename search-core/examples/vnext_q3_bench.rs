use personalrag_portable_search::{
    VNextDocumentInput, VNextSegmentReader, fold_ascii, write_vnext_segment,
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
            index as u64,
            name,
            fold_ascii(text.as_bytes()),
        ));
    }
    docs
}

fn main() {
    let args = env::args().collect::<Vec<_>>();
    let n = args
        .get(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_000usize);
    let out = PathBuf::from(
        args.get(2)
            .cloned()
            .unwrap_or_else(|| "/tmp/pr-vnext-q3-20k.prseg2".into()),
    );
    let _ = fs::remove_file(&out);
    let docs = synthetic_source(n);
    let source_bytes = docs
        .iter()
        .map(|doc| doc.normalized_content.len() as u64)
        .sum::<u64>();

    let started = Instant::now();
    let report = write_vnext_segment(&out, &docs).unwrap();
    let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;

    let reader = VNextSegmentReader::open(&out).unwrap();
    let stats = reader.q3_stats().unwrap();
    let timeout_posting = reader.q3_posting(*b"tim").unwrap();
    let timeout_blocks = timeout_posting.len();
    let timeout_ids = timeout_posting.iter().collect::<Vec<_>>();
    assert_eq!(timeout_ids.len(), timeout_blocks);
    assert!(timeout_ids.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(stats.0, report.q3_keys);
    assert_eq!(stats.1, report.q3_posting_ids);
    assert_eq!(stats.2, report.q3_active_shards);

    println!(
        "VNEXT_Q3_BENCH docs={n} source_bytes={source_bytes} blocks={} elapsed_ms={elapsed_ms:.3} file_bytes={} q3_keys={} q3_posting_ids={} active_shards={} singleton_keys={} raw_u16_keys={} dense_bitmap_keys={} posting_bytes={} tim_blocks={timeout_blocks}",
        report.blocks,
        report.file_bytes,
        report.q3_keys,
        report.q3_posting_ids,
        report.q3_active_shards,
        report.q3_singleton_keys,
        report.q3_raw_u16_keys,
        report.q3_dense_bitmap_keys,
        report.q3_posting_bytes,
    );
}
