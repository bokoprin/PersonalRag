#[allow(dead_code)]
mod extractor {
    include!("../../bridge-core/src/extractor.rs");
}

use extractor::{ExtractionBudget, ExtractorRegistry, PreparedContent};
use personalrag_portable_search::{VNextDocumentInput, write_vnext_segment};
use std::{
    env, fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
struct Timing {
    extract: Duration,
    index: Duration,
    e2e: Duration,
    pipeline: Duration,
}

fn median(mut values: Vec<Duration>) -> Duration {
    values.sort_unstable();
    values[values.len() / 2]
}

fn paths_in(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut paths = fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .map(|entry| entry.map(|value| value.path()).map_err(|e| e.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();
    Ok(paths)
}

fn normalized_content(
    path: &Path,
    registry: &ExtractorRegistry,
    budget: ExtractionBudget,
) -> Result<Vec<u8>, String> {
    let mut bytes = match registry.prepare(path, budget)? {
        PreparedContent::SourceFile => fs::read(path).map_err(|e| e.to_string())?,
        PreparedContent::NameOnly => Vec::new(),
        PreparedContent::Extracted(document) => document.text.into_bytes(),
    };
    bytes.make_ascii_lowercase();
    Ok(bytes)
}

fn extract_docs(paths: &[PathBuf]) -> Result<(Vec<VNextDocumentInput>, u64), String> {
    let registry = ExtractorRegistry::new();
    let budget = ExtractionBudget::from_max_file_bytes(64 * 1024 * 1024);
    let mut docs = Vec::with_capacity(paths.len());
    let mut extracted_bytes = 0u64;
    for (index, path) in paths.iter().enumerate() {
        let content = normalized_content(path, &registry, budget)?;
        extracted_bytes += content.len() as u64;
        let display = path
            .file_name()
            .and_then(|v| v.to_str())
            .ok_or_else(|| "non UTF-8 benchmark filename".to_owned())?;
        docs.push(VNextDocumentInput::new(index as u64 + 1, display, content));
    }
    Ok((docs, extracted_bytes))
}

fn production_like_docs(
    paths: &[PathBuf],
    spool_root: &Path,
) -> Result<Vec<VNextDocumentInput>, String> {
    if spool_root.exists() {
        fs::remove_dir_all(spool_root).map_err(|e| e.to_string())?;
    }
    fs::create_dir_all(spool_root).map_err(|e| e.to_string())?;
    let registry = ExtractorRegistry::new();
    let budget = ExtractionBudget::from_max_file_bytes(64 * 1024 * 1024);
    let mut content_paths = Vec::with_capacity(paths.len());
    for (index, path) in paths.iter().enumerate() {
        match registry.prepare(path, budget)? {
            PreparedContent::SourceFile => content_paths.push(path.clone()),
            PreparedContent::NameOnly => content_paths.push(PathBuf::new()),
            PreparedContent::Extracted(document) => {
                let prepared = spool_root.join(format!("{index:08}.txt"));
                fs::write(&prepared, document.text.as_bytes()).map_err(|e| e.to_string())?;
                content_paths.push(prepared);
            }
        }
    }
    let mut docs = Vec::with_capacity(paths.len());
    for (index, (source_path, content_path)) in paths.iter().zip(content_paths.iter()).enumerate() {
        let mut content = if content_path.as_os_str().is_empty() {
            Vec::new()
        } else {
            fs::read(content_path).map_err(|e| e.to_string())?
        };
        content.make_ascii_lowercase();
        let display = source_path
            .file_name()
            .and_then(|v| v.to_str())
            .ok_or_else(|| "non UTF-8 benchmark filename".to_owned())?;
        docs.push(VNextDocumentInput::new(index as u64 + 1, display, content));
    }
    Ok(docs)
}

fn build_segments(
    root: &Path,
    docs: &[VNextDocumentInput],
    segment_docs: usize,
) -> Result<u64, String> {
    if root.exists() {
        fs::remove_dir_all(root).map_err(|e| e.to_string())?;
    }
    fs::create_dir_all(root).map_err(|e| e.to_string())?;
    let mut bytes = 0u64;
    for (segment, chunk) in docs.chunks(segment_docs).enumerate() {
        let path = root.join(format!("segment-{segment:05}.prseg2"));
        let report = write_vnext_segment(&path, chunk).map_err(|e| e.to_string())?;
        bytes += report.file_bytes;
    }
    Ok(bytes)
}

fn bench_one(
    corpus: &Path,
    profile: &str,
    format: &str,
    output: &Path,
    segment_docs: usize,
    repeats: usize,
) -> Result<(), String> {
    let dir = corpus.join(profile).join(format);
    let paths = paths_in(&dir)?;
    if paths.is_empty() {
        return Err(format!("empty corpus: {}", dir.display()));
    }

    // Untimed warm-up and canonical extracted docs for index-only measurement.
    let source_bytes = paths.iter().try_fold(0u64, |sum, path| {
        fs::metadata(path)
            .map(|m| sum + m.len())
            .map_err(|e| e.to_string())
    })?;
    let (docs, extracted_bytes) = extract_docs(&paths)?;
    let index_root = output.join(format!("index-{profile}-{format}"));
    let index_bytes = build_segments(&index_root, &docs, segment_docs)?;

    let mut extract_times = Vec::with_capacity(repeats);
    let mut index_times = Vec::with_capacity(repeats);
    let mut e2e_times = Vec::with_capacity(repeats);
    let mut pipeline_times = Vec::with_capacity(repeats);

    for _ in 0..repeats {
        let start = Instant::now();
        let _ = extract_docs(&paths)?;
        extract_times.push(start.elapsed());

        let start = Instant::now();
        let _ = build_segments(&index_root, &docs, segment_docs)?;
        index_times.push(start.elapsed());

        let start = Instant::now();
        let (run_docs, _) = extract_docs(&paths)?;
        let _ = build_segments(&index_root, &run_docs, segment_docs)?;
        e2e_times.push(start.elapsed());

        let start = Instant::now();
        let pipeline_docs =
            production_like_docs(&paths, &output.join(format!("spool-{profile}-{format}")))?;
        let _ = build_segments(&index_root, &pipeline_docs, segment_docs)?;
        pipeline_times.push(start.elapsed());
    }

    let timing = Timing {
        extract: median(extract_times),
        index: median(index_times),
        e2e: median(e2e_times),
        pipeline: median(pipeline_times),
    };
    let files = paths.len() as f64;
    let extract_s = timing.extract.as_secs_f64();
    let e2e_s = timing.e2e.as_secs_f64();
    println!(
        "OFFICE_INDEX_BENCH profile={profile} format={format} files={} source_bytes={} extracted_bytes={} expansion={:.3} index_bytes={} extract_ms={:.3} extract_files_s={:.1} index_ms={:.3} direct_e2e_ms={:.3} direct_e2e_files_s={:.1} production_like_ms={:.3} production_like_files_s={:.1}",
        paths.len(),
        source_bytes,
        extracted_bytes,
        extracted_bytes as f64 / source_bytes.max(1) as f64,
        index_bytes,
        extract_s * 1000.0,
        files / extract_s,
        timing.index.as_secs_f64() * 1000.0,
        e2e_s * 1000.0,
        files / e2e_s,
        timing.pipeline.as_secs_f64() * 1000.0,
        files / timing.pipeline.as_secs_f64(),
    );
    Ok(())
}

fn main() -> Result<(), String> {
    let args = env::args().collect::<Vec<_>>();
    if args.len() < 2 {
        return Err(
            "usage: office_extraction_index_bench <corpus> [segment_docs] [repeats] [output]"
                .to_owned(),
        );
    }
    let corpus = PathBuf::from(&args[1]);
    let segment_docs = args
        .get(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or(5000usize);
    let repeats = args
        .get(3)
        .and_then(|v| v.parse().ok())
        .unwrap_or(5usize)
        .max(1);
    let output = args
        .get(4)
        .map(PathBuf::from)
        .unwrap_or_else(|| env::temp_dir().join("personalrag-office-index-bench"));
    fs::create_dir_all(&output).map_err(|e| e.to_string())?;

    for profile in ["single", "multipart"] {
        for format in ["txt", "docx", "xlsx", "pptx"] {
            bench_one(&corpus, profile, format, &output, segment_docs, repeats)?;
        }
    }
    println!("OFFICE_INDEX_BENCH_PASS");
    Ok(())
}
