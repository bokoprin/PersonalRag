use personalrag_portable_search::{
    VNextDocumentInput, VNextSegmentReader, fold_ascii, write_vnext_segment_with_block_size,
};
use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn push_docs(docs: &mut Vec<VNextDocumentInput>, next_id: &mut u64, content: &str, count: usize) {
    for _ in 0..count {
        let id = *next_id;
        docs.push(VNextDocumentInput::new(
            id,
            format!("doc_{id:05}.txt"),
            fold_ascii(content.as_bytes()),
        ));
        *next_id += 1;
    }
}

fn p50(samples: &mut [Duration]) -> f64 {
    samples.sort_unstable();
    samples[samples.len() / 2].as_secs_f64() * 1000.0
}

fn main() {
    let rounds = std::env::args()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(101usize);
    let root = PathBuf::from(
        std::env::args()
            .nth(2)
            .unwrap_or_else(|| "/tmp/personalrag-common-q3".into()),
    );
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let path = root.join("segment.prseg2");
    let mut docs = Vec::with_capacity(33_000);
    let mut id = 0u64;
    push_docs(&mut docs, &mut id, "xxxxxxxabcdezzz", 1_000);
    push_docs(&mut docs, &mut id, "xxxxxxxabc___bcd", 4_000);
    push_docs(&mut docs, &mut id, "xxxxxxxabc___cde", 4_000);
    push_docs(&mut docs, &mut id, "xxxxxxxabc___qqq", 2_000);
    push_docs(&mut docs, &mut id, "xxxxxxxbcd___qqq", 10_000);
    push_docs(&mut docs, &mut id, "xxxxxxxcde___qqq", 12_000);
    write_vnext_segment_with_block_size(&path, &docs, 8192).unwrap();
    let reader = VNextSegmentReader::open(&path).unwrap();
    let (hits, diag) = reader.search_content_with_diagnostics(b"abcde").unwrap();
    assert_eq!(hits.len(), 1_000);
    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let started = Instant::now();
        let result = reader.search_content(b"abcde").unwrap();
        black_box(&result);
        samples.push(started.elapsed());
    }
    println!(
        "COMMON_Q3 docs={} hits={} p50_ms={:.6} anchor_blocks={} selected_anchors={} candidate_blocks={} verified_blocks={}",
        docs.len(),
        hits.len(),
        p50(&mut samples),
        diag.anchor_blocks,
        diag.selected_anchor_count,
        diag.candidate_blocks,
        diag.verified_blocks
    );
}
