#[allow(dead_code)]
#[rustfmt::skip]
#[path = "../../bridge-core/src/extractor.rs"]
mod extractor;
#[allow(dead_code)]
#[rustfmt::skip]
#[path = "../../bridge-core/src/office_cache.rs"]
mod office_cache;

use extractor::{ExtractionBudget, ExtractorRegistry, PreparedContent};
use office_cache::{
    OfficeExtractionConfig, OfficeExtractionRequest, OfficeExtractionService, OfficePreparedContent,
};
use personalrag_portable_search::{VNextDocumentInput, write_vnext_segment};
use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

fn median(mut values: Vec<Duration>) -> Duration {
    values.sort_unstable();
    values[values.len() / 2]
}

fn paths_in(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut paths = fs::read_dir(dir)
        .map_err(|error| error.to_string())?
        .map(|entry| {
            entry
                .map(|value| value.path())
                .map_err(|error| error.to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();
    Ok(paths)
}

fn build_segments(
    root: &Path,
    docs: &[VNextDocumentInput],
    segment_docs: usize,
) -> Result<u64, String> {
    if root.exists() {
        fs::remove_dir_all(root).map_err(|error| error.to_string())?;
    }
    fs::create_dir_all(root).map_err(|error| error.to_string())?;
    let mut bytes = 0u64;
    for (segment, chunk) in docs.chunks(segment_docs).enumerate() {
        let path = root.join(format!("segment-{segment:05}.prseg2"));
        bytes += write_vnext_segment(&path, chunk)
            .map_err(|error| error.to_string())?
            .file_bytes;
    }
    Ok(bytes)
}

fn txt_docs(paths: &[PathBuf]) -> Result<Vec<VNextDocumentInput>, String> {
    let mut docs = Vec::with_capacity(paths.len());
    for (index, path) in paths.iter().enumerate() {
        let mut content = fs::read(path).map_err(|error| error.to_string())?;
        content.make_ascii_lowercase();
        let display = path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| "non UTF-8 benchmark filename".to_owned())?;
        docs.push(VNextDocumentInput::new(index as u64 + 1, display, content));
    }
    Ok(docs)
}

fn old_production_office_docs(
    paths: &[PathBuf],
    spool_root: &Path,
) -> Result<Vec<VNextDocumentInput>, String> {
    if spool_root.exists() {
        fs::remove_dir_all(spool_root).map_err(|error| error.to_string())?;
    }
    fs::create_dir_all(spool_root).map_err(|error| error.to_string())?;
    let registry = ExtractorRegistry::new();
    let budget = ExtractionBudget::from_max_file_bytes(64 * 1024 * 1024);
    let mut prepared_paths = Vec::with_capacity(paths.len());
    for (index, path) in paths.iter().enumerate() {
        match registry.prepare(path, budget)? {
            PreparedContent::Extracted(document) => {
                let prepared = spool_root.join(format!("{index:08}.txt"));
                fs::write(&prepared, document.text.as_bytes())
                    .map_err(|error| error.to_string())?;
                prepared_paths.push(prepared);
            }
            PreparedContent::SourceFile => prepared_paths.push(path.clone()),
            PreparedContent::NameOnly => prepared_paths.push(PathBuf::new()),
        }
    }
    let mut docs = Vec::with_capacity(paths.len());
    for (index, (path, prepared)) in paths.iter().zip(prepared_paths.iter()).enumerate() {
        let mut content = if prepared.as_os_str().is_empty() {
            Vec::new()
        } else {
            fs::read(prepared).map_err(|error| error.to_string())?
        };
        content.make_ascii_lowercase();
        let display = path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| "non UTF-8 benchmark filename".to_owned())?;
        docs.push(VNextDocumentInput::new(index as u64 + 1, display, content));
    }
    Ok(docs)
}

fn cached_office_docs(
    paths: &[PathBuf],
    cache_root: &Path,
    workers: usize,
) -> Result<(Vec<VNextDocumentInput>, usize, usize), String> {
    let config = OfficeExtractionConfig {
        max_workers: workers.max(1),
        memory_budget_bytes: 512 * 1024 * 1024,
        ..OfficeExtractionConfig::default()
    };
    let service = OfficeExtractionService::new(
        cache_root.to_path_buf(),
        ExtractionBudget::from_max_file_bytes(64 * 1024 * 1024),
        config,
    );
    let requests = paths
        .iter()
        .enumerate()
        .map(|(source_index, path)| {
            Ok(OfficeExtractionRequest {
                source_index,
                source_bytes: fs::metadata(path).map_err(|error| error.to_string())?.len(),
                path: path.clone(),
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let (prepared, report) = service.prepare_many(&requests, &AtomicBool::new(false));
    let mut docs = Vec::with_capacity(prepared.len());
    for item in prepared {
        let (source_index, mut content) = match item {
            OfficePreparedContent::Cached {
                source_index, path, ..
            } => (
                source_index,
                fs::read(path).map_err(|error| error.to_string())?,
            ),
            OfficePreparedContent::Extracted {
                source_index, text, ..
            } => (source_index, text.into_bytes()),
            OfficePreparedContent::Failed { error, .. } => return Err(error),
        };
        content.make_ascii_lowercase();
        let display = paths[source_index]
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| "non UTF-8 benchmark filename".to_owned())?;
        docs.push(VNextDocumentInput::new(
            source_index as u64 + 1,
            display,
            content,
        ));
    }
    docs.sort_by_key(|document| document.logical_id);
    Ok((docs, report.cache_hits, report.workers))
}

fn bench_format(
    corpus: &Path,
    profile: &str,
    format: &str,
    output: &Path,
    workers: usize,
    segment_docs: usize,
    repeats: usize,
) -> Result<(), String> {
    let paths = paths_in(&corpus.join(profile).join(format))?;
    if paths.is_empty() {
        return Err(format!("empty corpus {profile}/{format}"));
    }
    let index_root = output.join(format!("index-{profile}-{format}"));
    if format == "txt" {
        let mut runs = Vec::with_capacity(repeats);
        for _ in 0..repeats {
            let start = Instant::now();
            let docs = txt_docs(&paths)?;
            let _ = build_segments(&index_root, &docs, segment_docs)?;
            runs.push(start.elapsed());
        }
        println!(
            "OFFICE_CACHE_BENCH profile={profile} format=txt files={} txt_e2e_ms={:.3}",
            paths.len(),
            median(runs).as_secs_f64() * 1000.0
        );
        return Ok(());
    }

    let old_spool_root = output.join(format!("old-spool-{profile}-{format}"));
    let mut old_runs = Vec::with_capacity(repeats);
    for _ in 0..repeats {
        let start = Instant::now();
        let docs = old_production_office_docs(&paths, &old_spool_root)?;
        let _ = build_segments(&index_root, &docs, segment_docs)?;
        old_runs.push(start.elapsed());
    }

    let cache_root = output.join(format!("cache-{profile}-{format}"));
    let mut cold_runs = Vec::with_capacity(repeats);
    let mut cold_workers = 0usize;
    for _ in 0..repeats {
        let _ = fs::remove_dir_all(&cache_root);
        let start = Instant::now();
        let (docs, hits, used_workers) = cached_office_docs(&paths, &cache_root, workers)?;
        if hits != 0 {
            return Err("cold cache unexpectedly hit".to_owned());
        }
        cold_workers = used_workers;
        let _ = build_segments(&index_root, &docs, segment_docs)?;
        cold_runs.push(start.elapsed());
    }

    // Prime once, then benchmark only warm-cache runs without deleting cache.
    let _ = fs::remove_dir_all(&cache_root);
    let (_, prime_hits, _) = cached_office_docs(&paths, &cache_root, workers)?;
    if prime_hits != 0 {
        return Err("cache prime unexpectedly hit".to_owned());
    }
    let mut warm_runs = Vec::with_capacity(repeats);
    let mut warm_hits = 0usize;
    for _ in 0..repeats {
        let start = Instant::now();
        let (docs, hits, _) = cached_office_docs(&paths, &cache_root, workers)?;
        warm_hits = hits;
        let _ = build_segments(&index_root, &docs, segment_docs)?;
        warm_runs.push(start.elapsed());
    }
    if warm_hits != paths.len() {
        return Err(format!("warm cache hits {warm_hits} != {}", paths.len()));
    }
    let old = median(old_runs);
    let cold = median(cold_runs);
    let warm = median(warm_runs);
    println!(
        "OFFICE_CACHE_BENCH profile={profile} format={format} files={} workers={} old_production_ms={:.3} cold_cache_ms={:.3} warm_cache_ms={:.3} cold_speedup={:.3} warm_speedup={:.3} warm_hits={}",
        paths.len(),
        cold_workers,
        old.as_secs_f64() * 1000.0,
        cold.as_secs_f64() * 1000.0,
        warm.as_secs_f64() * 1000.0,
        old.as_secs_f64() / cold.as_secs_f64(),
        old.as_secs_f64() / warm.as_secs_f64(),
        warm_hits,
    );
    Ok(())
}

fn main() -> Result<(), String> {
    let args = env::args().collect::<Vec<_>>();
    if args.len() < 2 {
        return Err("usage: office_cache_pipeline_bench <corpus> [workers] [segment_docs] [repeats] [output]".to_owned());
    }
    let corpus = PathBuf::from(&args[1]);
    let workers = args
        .get(2)
        .and_then(|value| value.parse().ok())
        .unwrap_or(4usize);
    let segment_docs = args
        .get(3)
        .and_then(|value| value.parse().ok())
        .unwrap_or(5000usize);
    let repeats = args
        .get(4)
        .and_then(|value| value.parse().ok())
        .unwrap_or(5usize)
        .max(1);
    let output = args
        .get(5)
        .map(PathBuf::from)
        .unwrap_or_else(|| env::temp_dir().join("personalrag-office-cache-bench"));
    fs::create_dir_all(&output).map_err(|error| error.to_string())?;
    for profile in ["single", "multipart"] {
        for format in ["txt", "docx", "xlsx", "pptx"] {
            bench_format(
                &corpus,
                profile,
                format,
                &output,
                workers,
                segment_docs,
                repeats,
            )?;
        }
    }
    println!("OFFICE_CACHE_BENCH_PASS");
    Ok(())
}
