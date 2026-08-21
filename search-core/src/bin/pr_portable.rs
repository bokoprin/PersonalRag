use std::env;
use std::error::Error;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use personalrag_portable_search::{
    BuildMode, BuildOptions, CatalogEntry, CatalogSnapshot, ChangeBatch, ChangeKind,
    DocumentChange, DocumentInput, IncrementalPolicy, LazyPersistentIndex, LogicalDocument,
    MergedIndex, MergedSearchSession, PersistentIndex, PooledLazyPersistentIndex, Pos3Policy,
    PosCodec, Positional2Index, Positional3Index, PositionalIndex, apply_update_plan,
    build_disk_corpus, build_disk_corpus_parallel, build_disk_index_pipelined,
    build_disk_index_pipelined_benchmark, build_index, build_index_benchmark,
    build_positional_sidecars, build_positional2_sidecars, build_positional3_sidecars,
    build_positional23_sidecars, build_q2_sidecars, compact_generation,
    detected_available_memory_bytes, fold_ascii, initialize_generation, plan_incremental_update,
    publish_incremental_update, recommend_build_tuning, recommend_system_build_tuning,
    verify_index, verify_positional_sidecars, verify_positional2_sidecars,
    verify_positional3_sidecars,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let args = env::args().collect::<Vec<_>>();
    let Some(command) = args.get(1).map(String::as_str) else {
        usage();
        return Ok(());
    };
    match command {
        "build-disk" => build_disk(&args[2..], false),
        "build-disk-parallel" => build_disk(&args[2..], true),
        "build-disk-pipelined" => build_disk_pipelined_cli(&args[2..], true),
        "build-disk-pipelined-fast" => build_disk_pipelined_cli(&args[2..], false),
        "verify" => {
            let dir = required(&args, 2, "INDEX_DIR")?;
            verify_index(dir)?;
            println!("VERIFY_PASS {dir}");
            Ok(())
        }
        "query" => query(&args[2..]),
        "query-lazy" => query_lazy(&args[2..]),
        "build-synthetic" => build_synthetic(&args[2..], true),
        "build-synthetic-fast" => build_synthetic(&args[2..], false),
        "profile-source" => profile_source(&args[2..]),
        "profile-pool" => profile_pool(&args[2..]),
        "profile-auto-query" => profile_auto_query(&args[2..]),
        "profile-q2" => profile_q2(&args[2..]),
        "build-q2-sidecars" => build_q2_sidecars_cli(&args[2..]),
        "build-pos-sidecars" => build_pos_sidecars_cli(&args[2..]),
        "build-pos2-sidecars" => build_pos2_sidecars_cli(&args[2..]),
        "build-pos23-sidecars" => build_pos23_sidecars_cli(&args[2..]),
        "build-pos3-sidecars" => build_pos3_sidecars_cli(&args[2..]),
        "verify-pos-sidecars" => verify_pos_sidecars_cli(&args[2..]),
        "verify-pos2-sidecars" => verify_pos2_sidecars_cli(&args[2..]),
        "verify-pos3-sidecars" => verify_pos3_sidecars_cli(&args[2..]),
        "query-pos" => query_pos_cli(&args[2..]),
        "query-pos2" => query_pos2_cli(&args[2..]),
        "query-pos3" => query_pos3_cli(&args[2..]),
        "first-pos3" => first_pos3_cli(&args[2..]),
        "profile-pos" => profile_pos_cli(&args[2..]),
        "profile-pos2" => profile_pos2_cli(&args[2..]),
        "profile-pos3" => profile_pos3_cli(&args[2..]),
        "profile-pos3-partitions" => profile_pos3_partitions_cli(&args[2..]),
        "profile-auto-partitions" => profile_auto_partitions_cli(&args[2..]),
        "profile-long-filter" => profile_long_filter_cli(&args[2..]),
        "profile-pos2-partitions" => profile_pos2_partitions_cli(&args[2..]),
        "profile-pos-partitions" => profile_pos_partitions_cli(&args[2..]),
        "profile-pos-auto" => profile_pos_auto_cli(&args[2..]),
        "tune-build" => tune_build(&args[2..]),
        "diagnose-content" => diagnose_content(&args[2..]),
        "profile-partitions" => profile_partitions(&args[2..]),
        "bench-incremental" => bench_incremental(&args[2..]),
        "compaction-status" => compaction_status(&args[2..]),
        "self-test" => self_test(),
        _ => {
            usage();
            Err(format!("unknown command: {command}").into())
        }
    }
}

fn build_disk(args: &[String], parallel_scan: bool) -> Result<(), Box<dyn Error>> {
    if args.len() < 5 {
        return Err(
            "build-disk ROOT MODE INDEX_DIR SEGMENT_DOCS WORKERS [MAX_DOCS] [MAX_FILE_BYTES]"
                .into(),
        );
    }
    let root = PathBuf::from(&args[0]);
    let mode = parse_mode(&args[1])?;
    let output = PathBuf::from(&args[2]);
    let segment_docs: usize = args[3].parse()?;
    let workers: usize = args[4].parse()?;
    let max_docs = args
        .get(5)
        .map(|value| value.parse::<usize>())
        .transpose()?
        .and_then(|value| (value != 0).then_some(value));
    let max_file_bytes = args
        .get(6)
        .map_or(Ok(8 * 1024 * 1024u64), |value| value.parse())?;
    let scan_workers = args.get(7).map_or(Ok(workers), |value| value.parse())?;
    let scan_started = Instant::now();
    let documents = if parallel_scan {
        build_disk_corpus_parallel(&root, max_docs, max_file_bytes, scan_workers)?
    } else {
        build_disk_corpus(&root, max_docs, max_file_bytes)?
    };
    let scan_ms = scan_started.elapsed().as_secs_f64() * 1000.0;
    let report = build_index(
        &documents,
        &output,
        &BuildOptions {
            mode,
            segment_docs,
            workers,
        },
    )?;
    println!(
        "BUILD_PASS docs={} segments={} index_bytes={} scan_ms={:.3} elapsed_ms={:.3}",
        report.docs,
        report.segments,
        report.index_bytes,
        scan_ms,
        report.elapsed.as_secs_f64() * 1000.0
    );
    Ok(())
}

fn build_disk_pipelined_cli(args: &[String], durable: bool) -> Result<(), Box<dyn Error>> {
    if args.len() < 6 {
        return Err("build-disk-pipelined ROOT MODE INDEX_DIR SEGMENT_DOCS BUILD_WORKERS SCAN_WORKERS [MAX_DOCS] [MAX_FILE_BYTES]".into());
    }
    let root = PathBuf::from(&args[0]);
    let mode = parse_mode(&args[1])?;
    let output = PathBuf::from(&args[2]);
    let segment_docs: usize = args[3].parse()?;
    let build_workers: usize = args[4].parse()?;
    let scan_workers: usize = args[5].parse()?;
    let max_docs = args
        .get(6)
        .map(|value| value.parse::<usize>())
        .transpose()?
        .and_then(|value| (value != 0).then_some(value));
    let max_file_bytes = args
        .get(7)
        .map_or(Ok(8 * 1024 * 1024u64), |value| value.parse())?;
    let options = BuildOptions {
        mode,
        segment_docs,
        workers: build_workers,
    };
    let report = if durable {
        build_disk_index_pipelined(
            &root,
            max_docs,
            max_file_bytes,
            &output,
            &options,
            scan_workers,
        )?
    } else {
        build_disk_index_pipelined_benchmark(
            &root,
            max_docs,
            max_file_bytes,
            &output,
            &options,
            scan_workers,
        )?
    };
    println!(
        "PIPELINE_BUILD_PASS docs={} segments={} index_bytes={} elapsed_ms={:.3} durable={durable}",
        report.docs,
        report.segments,
        report.index_bytes,
        report.elapsed.as_secs_f64() * 1000.0
    );
    Ok(())
}

fn query(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() < 3 {
        return Err("query INDEX_DIR content|name QUERY [LIMIT]".into());
    }
    let open_started = Instant::now();
    // Production query hot path: the published generation is checksummed at publish/verify
    // time; normal interactive open reads only the manifest and maps segments on first use.
    let index = LazyPersistentIndex::open(&args[0])?;
    let open_elapsed = open_started.elapsed();
    let names = match args[1].as_str() {
        "content" => false,
        "name" => true,
        other => return Err(format!("bad query kind: {other}").into()),
    };
    let limit = args
        .get(3)
        .map(|value| value.parse::<usize>())
        .transpose()?
        .and_then(|value| (value != 0).then_some(value));
    let query_started = Instant::now();
    let hits = if let Some(limit) = limit {
        index.first_n(args[2].as_bytes(), names, limit)?
    } else if names {
        index.search_name(args[2].as_bytes())?
    } else {
        index.search_content(args[2].as_bytes())?
    };
    let query_elapsed = query_started.elapsed();
    println!(
        "OPEN_MS {:.6} QUERY_MS {:.6}",
        open_elapsed.as_secs_f64() * 1000.0,
        query_elapsed.as_secs_f64() * 1000.0
    );
    println!("HITS {}", hits.len());
    for hit in hits {
        println!("{hit}");
    }
    Ok(())
}

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

fn synthetic_source(n: usize) -> (Vec<DocumentInput>, u64) {
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
    let mut bytes = 0u64;
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
        let normalized_name = fold_ascii(name.as_bytes());
        let normalized_content = fold_ascii(text.as_bytes());
        bytes += (normalized_name.len() + normalized_content.len()) as u64;
        docs.push(DocumentInput::new(
            name.clone(),
            name,
            normalized_name,
            normalized_content,
        ));
    }
    (docs, bytes)
}

fn build_synthetic(args: &[String], durable: bool) -> Result<(), Box<dyn Error>> {
    if args.len() < 4 {
        return Err("build-synthetic DOCS INDEX_DIR SEGMENT_DOCS WORKERS".into());
    }
    let docs: usize = args[0].parse()?;
    let output = PathBuf::from(&args[1]);
    let segment_docs: usize = args[2].parse()?;
    let workers: usize = args[3].parse()?;
    let generation_started = Instant::now();
    let (documents, input_bytes) = synthetic_source(docs);
    let generation_elapsed = generation_started.elapsed();
    let options = BuildOptions {
        mode: BuildMode::Adaptive,
        segment_docs,
        workers,
    };
    let report = if durable {
        build_index(&documents, &output, &options)?
    } else {
        build_index_benchmark(&documents, &output, &options)?
    };
    println!(
        "SYNTHETIC_BUILD docs={} input_bytes={} generation_ms={:.3} build_ms={:.3} segments={} index_bytes={} durable={}",
        docs,
        input_bytes,
        generation_elapsed.as_secs_f64() * 1000.0,
        report.elapsed.as_secs_f64() * 1000.0,
        report.segments,
        report.index_bytes,
        durable,
    );
    Ok(())
}

fn percentile(samples: &mut [Duration], percentile: f64) -> f64 {
    samples.sort_unstable();
    if samples.is_empty() {
        return 0.0;
    }
    let index = ((samples.len() - 1) as f64 * percentile).ceil() as usize;
    samples[index].as_secs_f64() * 1000.0
}

fn measure_query<F>(rounds: usize, mut query: F) -> Result<(usize, f64, f64), Box<dyn Error>>
where
    F: FnMut() -> Result<usize, Box<dyn Error>>,
{
    let _ = query()?;
    let mut samples = Vec::with_capacity(rounds);
    let mut hits = 0usize;
    for _ in 0..rounds {
        let started = Instant::now();
        hits = query()?;
        samples.push(started.elapsed());
    }
    let mut p50 = samples.clone();
    Ok((
        hits,
        percentile(&mut p50, 0.50),
        percentile(&mut samples, 0.95),
    ))
}

fn profile_pool(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() {
        return Err("profile-pool INDEX_DIR [ROUNDS] [WORKERS]".into());
    }
    let root = &args[0];
    let rounds = args.get(1).map_or(Ok(40usize), |value| value.parse())?;
    let workers = args.get(2).map_or(Ok(4usize), |value| value.parse())?;
    let baseline = LazyPersistentIndex::open(root)?;
    let pooled = PooledLazyPersistentIndex::open(root, workers)?;
    let cases = [
        ("q1-a", "a"),
        ("q2-re", "re"),
        ("q3-ret", "ret"),
        ("long-return", "return"),
        ("long-namespace", "namespace"),
        ("long-timeout", "timeout"),
    ];
    println!(
        "POOL_HEADER docs={} rounds={rounds} workers={workers}",
        baseline.docs()
    );
    for (label, text) in cases {
        let expected = baseline.search_content_with_workers(text.as_bytes(), workers)?;
        let actual = pooled.search_content_with_workers(text.as_bytes(), workers)?;
        if expected != actual {
            return Err(format!("pooled query mismatch for {label}").into());
        }
        let (_, base_p50, base_p95) = measure_query(rounds, || {
            Ok(baseline
                .search_content_with_workers(text.as_bytes(), workers)?
                .len())
        })?;
        let (_, pool_p50, pool_p95) = measure_query(rounds, || {
            Ok(pooled
                .search_content_with_workers(text.as_bytes(), workers)?
                .len())
        })?;
        let (_, auto_p50, auto_p95) =
            measure_query(rounds, || Ok(pooled.search_content(text.as_bytes())?.len()))?;
        println!(
            "POOL_CASE label={label} spawn_p50_ms={base_p50:.6} spawn_p95_ms={base_p95:.6} pooled_p50_ms={pool_p50:.6} pooled_p95_ms={pool_p95:.6} auto_p50_ms={auto_p50:.6} auto_p95_ms={auto_p95:.6}"
        );
    }
    Ok(())
}

fn directory_file_bytes(root: &std::path::Path) -> Result<u64, Box<dyn Error>> {
    let mut total = 0u64;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            total = total
                .checked_add(entry.metadata()?.len())
                .ok_or("directory size overflow")?;
        }
    }
    Ok(total)
}

fn profile_auto_query(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() < 2 {
        return Err("profile-auto-query INDEX_DIR QUERY [ROUNDS=50] [WORKERS=4]".into());
    }
    let rounds = args.get(2).map_or(Ok(50usize), |v| v.parse())?;
    let workers = args.get(3).map_or(Ok(4usize), |v| v.parse())?;
    let session = personalrag_portable_search::SearchSession::open(&args[0], workers)?;
    let query = args[1].as_bytes();
    let expected = session.search_content(query)?.len();
    let (_, p50, p95) = measure_query(rounds, || Ok(session.search_content(query)?.len()))?;
    println!(
        "AUTO_QUERY query={} hits={} rounds={} workers={} p50_ms={p50:.6} p95_ms={p95:.6}",
        args[1], expected, rounds, workers
    );
    Ok(())
}

fn profile_q2(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() {
        return Err("profile-q2 INDEX_DIR [ROUNDS]".into());
    }
    let root = PathBuf::from(&args[0]);
    let rounds = args.get(1).map_or(Ok(30usize), |value| value.parse())?;
    let index = PersistentIndex::open_with_workers(&root, false, 4)?;
    let build_started = Instant::now();
    let prototype = index.build_q2_prototype()?;
    let build_ms = build_started.elapsed().as_secs_f64() * 1000.0;
    let index_bytes = directory_file_bytes(&root)?;
    println!(
        "Q2_HEADER docs={} index_bytes={} prototype_bytes={} prototype_ratio={:.6} records={} dense_records={} build_ms={build_ms:.3} rounds={rounds}",
        index.docs(),
        index_bytes,
        prototype.persisted_bytes(),
        prototype.persisted_bytes() as f64 / index_bytes.max(1) as f64,
        prototype.records(),
        prototype.dense_records(),
    );
    for text in ["re", "in", "er", "::", "on", "st", "co", "fi"] {
        let expected = index.search_content_with_workers(text.as_bytes(), 4)?;
        let accelerated = index.search_content_q2_prototype(&prototype, text.as_bytes())?;
        if expected != accelerated {
            return Err(format!("q2 prototype mismatch for {text}").into());
        }
        let (_, current_p50, current_p95) = measure_query(rounds, || {
            Ok(index.search_content_with_workers(text.as_bytes(), 4)?.len())
        })?;
        let (_, q2_p50, q2_p95) = measure_query(rounds, || {
            Ok(index
                .search_content_q2_prototype(&prototype, text.as_bytes())?
                .len())
        })?;
        println!(
            "Q2_CASE query={text} hits={} current_p50_ms={current_p50:.6} current_p95_ms={current_p95:.6} q2_p50_ms={q2_p50:.6} q2_p95_ms={q2_p95:.6}",
            expected.len()
        );
    }
    Ok(())
}

fn build_q2_sidecars_cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() {
        return Err("build-q2-sidecars INDEX_DIR [DURABLE=1]".into());
    }
    let durable = args.get(1).is_none_or(|value| value != "0");
    let started = Instant::now();
    let report = build_q2_sidecars(&args[0], durable)?;
    println!(
        "Q2_SIDECAR_BUILD segments={} bytes={} records={} dense_records={} elapsed_ms={:.3}",
        report.segments,
        report.bytes,
        report.records,
        report.dense_records,
        started.elapsed().as_secs_f64() * 1000.0,
    );
    Ok(())
}

fn build_pos2_sidecars_cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() {
        return Err("build-pos2-sidecars INDEX_DIR [Q3_THRESHOLD_PPM=500000] [CHILD_THRESHOLD_PPM=100000] [DURABLE=1]".into());
    }
    let q3 = args.get(1).map_or(Ok(500_000u32), |v| v.parse())?;
    let child = args.get(2).map_or(Ok(100_000u32), |v| v.parse())?;
    let durable = args.get(3).is_none_or(|v| v != "0");
    let r = build_positional2_sidecars(&args[0], q3, child, durable)?;
    println!(
        "POS2_BUILD q3_threshold_ppm={} child_threshold_ppm={} segments={} records={} units={} occurrences={} bytes={} elapsed_ms={:.3}",
        q3, child, r.segments, r.records, r.units, r.occurrences, r.bytes, r.elapsed_ms
    );
    Ok(())
}

fn build_pos23_sidecars_cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() {
        return Err("build-pos23-sidecars INDEX_DIR [Q3_THRESHOLD_PPM=500000] [POS2_CHILD_THRESHOLD_PPM=500000] [POS3_CHILD_THRESHOLD_PPM=500000] [MAX_GRAM=16] [POLICY=adaptive] [DURABLE=1]".into());
    }
    let q3 = args.get(1).map_or(Ok(500_000u32), |v| v.parse())?;
    let pos2_child = args.get(2).map_or(Ok(500_000u32), |v| v.parse())?;
    let pos3_child = args.get(3).map_or(Ok(500_000u32), |v| v.parse())?;
    let max_gram = args.get(4).map_or(Ok(16usize), |v| v.parse())?;
    let policy = parse_pos3_policy(args.get(5).map_or("adaptive", String::as_str))?;
    let durable = args.get(6).is_none_or(|v| v != "0");
    let r = build_positional23_sidecars(
        &args[0], q3, pos2_child, pos3_child, max_gram, policy, durable,
    )?;
    println!(
        "POS23_BUILD q3_threshold_ppm={} pos2_child_threshold_ppm={} pos3_child_threshold_ppm={} max_gram={} policy={} segments={} pos2_records={} pos2_units={} pos2_occurrences={} pos2_bytes={} pos3_records={} pos3_units={} pos3_bytes={} elapsed_ms={:.3} delta={} bitmap={} complement={} all={} runs={} bp128={}",
        q3,
        pos2_child,
        pos3_child,
        max_gram,
        policy.name(),
        r.pos2.segments,
        r.pos2.records,
        r.pos2.units,
        r.pos2.occurrences,
        r.pos2.bytes,
        r.pos3.records,
        r.pos3.units,
        r.pos3.bytes,
        r.elapsed_ms,
        r.pos3.delta_records,
        r.pos3.bitmap_records,
        r.pos3.complement_records,
        r.pos3.all_records,
        r.pos3.run_records,
        r.pos3.bp128_records,
    );
    Ok(())
}

fn verify_pos2_sidecars_cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() {
        return Err("verify-pos2-sidecars INDEX_DIR".into());
    }
    let started = Instant::now();
    verify_positional2_sidecars(&args[0])?;
    println!(
        "POS2_VERIFY_PASS elapsed_ms={:.3}",
        started.elapsed().as_secs_f64() * 1000.0
    );
    Ok(())
}

fn query_pos2_cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() < 2 {
        return Err("query-pos2 INDEX_DIR QUERY [WORKERS=4]".into());
    }
    let workers = args.get(2).map_or(Ok(4usize), |v| v.parse())?;
    let open = Instant::now();
    let index = Positional2Index::open(&args[0])?;
    let open_ms = open.elapsed().as_secs_f64() * 1000.0;
    let started = Instant::now();
    let hits = index.search_content(args[1].as_bytes(), workers)?;
    let query_ms = started.elapsed().as_secs_f64() * 1000.0;
    println!(
        "POS2_QUERY query={} hits={} open_ms={open_ms:.6} query_ms={query_ms:.6}",
        args[1],
        hits.len()
    );
    Ok(())
}

fn profile_pos2_cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() {
        return Err("profile-pos2 INDEX_DIR [ROUNDS=5] [WORKERS=4]".into());
    }
    let rounds = args.get(1).map_or(Ok(5usize), |v| v.parse())?;
    let workers = args.get(2).map_or(Ok(4usize), |v| v.parse())?;
    let p2 = Positional2Index::open(&args[0])?;
    let control = LazyPersistentIndex::open(&args[0])?;
    println!("POS2_PROFILE rounds={} workers={}", rounds, workers);
    for text in [
        "return",
        "timeout",
        "config",
        "error",
        "namespace",
        "include",
    ] {
        let expected = control.search_content(text.as_bytes())?;
        let actual = p2.search_content(text.as_bytes(), workers)?;
        if expected != actual {
            return Err(format!("PRPOS002 mismatch for {text}").into());
        }
        let (_, c50, c95) = measure_query(rounds, || {
            Ok(control
                .search_content_with_workers(text.as_bytes(), workers)?
                .len())
        })?;
        let (_, p50, p95) = measure_query(rounds, || {
            Ok(p2.search_content(text.as_bytes(), workers)?.len())
        })?;
        println!(
            "POS2_CASE query={text} hits={} control_p50_ms={c50:.6} control_p95_ms={c95:.6} pos2_p50_ms={p50:.6} pos2_p95_ms={p95:.6}",
            expected.len()
        );
    }
    Ok(())
}

fn profile_pos2_partitions_cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() < 3 {
        return Err("profile-pos2-partitions ROUNDS TOTAL_WORKERS INDEX_DIR...".into());
    }
    let rounds: usize = args[0].parse()?;
    let total_workers: usize = args[1].parse()?;
    let dirs = &args[2..];
    let per = (total_workers / dirs.len().max(1)).max(1);
    let mut p2 = Vec::new();
    let mut ctrl = Vec::new();
    for d in dirs {
        p2.push(Positional2Index::open(d)?);
        ctrl.push(LazyPersistentIndex::open(d)?)
    }
    println!(
        "POS2_PARTITIONS partitions={} rounds={} total_workers={} per_partition_workers={}",
        dirs.len(),
        rounds,
        total_workers,
        per
    );
    for text in [
        "return",
        "timeout",
        "config",
        "error",
        "namespace",
        "include",
    ] {
        let expected: usize = std::thread::scope(|scope| {
            let mut hs = Vec::new();
            for x in &ctrl {
                hs.push(scope.spawn(|| {
                    x.search_content_with_workers(text.as_bytes(), per)
                        .map(|h| h.len())
                }))
            }
            let mut total = 0;
            for h in hs {
                total += h.join().expect("ctrl p2 partition panicked")?;
            }
            Ok::<usize, personalrag_portable_search::SearchError>(total)
        })?;
        let actual: usize = std::thread::scope(|scope| {
            let mut hs = Vec::new();
            for x in &p2 {
                hs.push(scope.spawn(|| x.search_content(text.as_bytes(), per).map(|h| h.len())))
            }
            let mut total = 0;
            for h in hs {
                total += h.join().expect("p2 partition panicked")?;
            }
            Ok::<usize, personalrag_portable_search::SearchError>(total)
        })?;
        if expected != actual {
            return Err(format!("PRPOS002 partition mismatch for {text}").into());
        }
        let (_, c50, c95) = measure_query(rounds, || {
            std::thread::scope(|scope| {
                let mut hs = Vec::new();
                for x in &ctrl {
                    hs.push(scope.spawn(|| {
                        x.search_content_with_workers(text.as_bytes(), per)
                            .map(|h| h.len())
                    }))
                }
                let mut total = 0;
                for h in hs {
                    total += h.join().expect("ctrl p2 partition panicked")?;
                }
                Ok(total)
            })
        })?;
        let (_, p50, p95) = measure_query(rounds, || {
            std::thread::scope(|scope| {
                let mut hs = Vec::new();
                for x in &p2 {
                    hs.push(scope.spawn(|| x.search_content(text.as_bytes(), per).map(|h| h.len())))
                }
                let mut total = 0;
                for h in hs {
                    total += h.join().expect("p2 partition panicked")?;
                }
                Ok(total)
            })
        })?;
        println!(
            "POS2_PARTITION_CASE query={text} hits={expected} control_p50_ms={c50:.6} control_p95_ms={c95:.6} pos2_p50_ms={p50:.6} pos2_p95_ms={p95:.6}"
        );
    }
    Ok(())
}

fn parse_pos3_policy(value: &str) -> Result<Pos3Policy, Box<dyn Error>> {
    match value {
        "delta" => Ok(Pos3Policy::Delta),
        "adaptive" => Ok(Pos3Policy::Adaptive),
        "bitmap" => Ok(Pos3Policy::Bitmap),
        "bp128" => Ok(Pos3Policy::Bp128),
        _ => Err(format!("unknown PRPOS003 policy: {value}").into()),
    }
}

fn build_pos3_sidecars_cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() {
        return Err("build-pos3-sidecars INDEX_DIR [Q3_THRESHOLD_PPM=500000] [CHILD_THRESHOLD_PPM=500000] [MAX_GRAM=16] [POLICY=adaptive] [DURABLE=1]".into());
    }
    let q3 = args.get(1).map_or(Ok(500_000u32), |v| v.parse())?;
    let child = args.get(2).map_or(Ok(500_000u32), |v| v.parse())?;
    let max_gram = args.get(3).map_or(Ok(16usize), |v| v.parse())?;
    let policy = parse_pos3_policy(args.get(4).map_or("adaptive", String::as_str))?;
    let durable = args.get(5).is_none_or(|v| v != "0");
    let r = build_positional3_sidecars(&args[0], q3, child, max_gram, policy, durable)?;
    println!(
        "POS3_BUILD q3_threshold_ppm={} child_threshold_ppm={} max_gram={} policy={} segments={} records={} units={} bytes={} elapsed_ms={:.3} delta={} bitmap={} complement={} all={} runs={} bp128={}",
        q3,
        child,
        max_gram,
        policy.name(),
        r.segments,
        r.records,
        r.units,
        r.bytes,
        r.elapsed_ms,
        r.delta_records,
        r.bitmap_records,
        r.complement_records,
        r.all_records,
        r.run_records,
        r.bp128_records,
    );
    Ok(())
}

fn verify_pos3_sidecars_cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() {
        return Err("verify-pos3-sidecars INDEX_DIR".into());
    }
    let started = Instant::now();
    verify_positional3_sidecars(&args[0])?;
    println!(
        "POS3_VERIFY_PASS elapsed_ms={:.3}",
        started.elapsed().as_secs_f64() * 1000.0
    );
    Ok(())
}

fn query_pos3_cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() < 2 {
        return Err("query-pos3 INDEX_DIR QUERY [WORKERS=4]".into());
    }
    let workers = args.get(2).map_or(Ok(4usize), |v| v.parse())?;
    let open = Instant::now();
    let index = Positional3Index::open(&args[0])?;
    let open_ms = open.elapsed().as_secs_f64() * 1000.0;
    let started = Instant::now();
    let hits = index.search_content(args[1].as_bytes(), workers)?;
    let query_ms = started.elapsed().as_secs_f64() * 1000.0;
    println!(
        "POS3_QUERY query={} hits={} open_ms={open_ms:.6} query_ms={query_ms:.6}",
        args[1],
        hits.len()
    );
    Ok(())
}

fn first_pos3_cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() < 2 {
        return Err("first-pos3 INDEX_DIR QUERY [LIMIT=100]".into());
    }
    let limit = args.get(2).map_or(Ok(100usize), |v| v.parse())?;
    let index = Positional3Index::open(&args[0])?;
    let started = Instant::now();
    let hits = index.first_n(args[1].as_bytes(), limit)?;
    println!(
        "POS3_FIRST query={} limit={} hits={} query_ms={:.6}",
        args[1],
        limit,
        hits.len(),
        started.elapsed().as_secs_f64() * 1000.0
    );
    Ok(())
}

fn profile_pos3_cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() {
        return Err("profile-pos3 INDEX_DIR [ROUNDS=7] [WORKERS=4]".into());
    }
    let rounds = args.get(1).map_or(Ok(7usize), |v| v.parse())?;
    let workers = args.get(2).map_or(Ok(4usize), |v| v.parse())?;
    let pos3 = Positional3Index::open(&args[0])?;
    let control = LazyPersistentIndex::open(&args[0])?;
    println!("POS3_PROFILE rounds={} workers={}", rounds, workers);
    for text in [
        "return",
        "timeout",
        "config",
        "error",
        "namespace",
        "include",
        "indexmanager",
        "personalrag",
        "unique_marker_970",
    ] {
        let expected = control.search_content(text.as_bytes())?;
        let actual = pos3.search_content(text.as_bytes(), workers)?;
        if expected != actual {
            return Err(format!("PRPOS003 mismatch for {text}").into());
        }
        let (_, c50, c95) = measure_query(rounds, || {
            Ok(control
                .search_content_with_workers(text.as_bytes(), workers)?
                .len())
        })?;
        let (_, p50, p95) = measure_query(rounds, || {
            Ok(pos3.search_content(text.as_bytes(), workers)?.len())
        })?;
        let expected_first = control.first_n(text.as_bytes(), false, 100)?;
        let actual_first = pos3.first_n(text.as_bytes(), 100)?;
        if expected_first != actual_first {
            return Err(format!("PRPOS003 First100 mismatch for {text}").into());
        }
        let (_, f50, f95) =
            measure_query(rounds, || Ok(pos3.first_n(text.as_bytes(), 100)?.len()))?;
        println!(
            "POS3_CASE query={text} hits={} control_p50_ms={c50:.6} control_p95_ms={c95:.6} pos3_p50_ms={p50:.6} pos3_p95_ms={p95:.6} first100_p50_ms={f50:.6} first100_p95_ms={f95:.6}",
            expected.len()
        );
    }
    Ok(())
}

fn intersect_doc_lists(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len().min(b.len()));
    let (mut i, mut j) = (0usize, 0usize);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

fn profile_long_filter_cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() < 2 {
        return Err("profile-long-filter INDEX_DIR QUERY [ROUNDS=15] [WORKERS=4]".into());
    }
    let query = args[1].as_bytes();
    if query.len() <= 16 {
        return Err("profile-long-filter requires a query longer than 16 bytes".into());
    }
    let rounds = args.get(2).map_or(Ok(15usize), |v| v.parse())?;
    let workers = args.get(3).map_or(Ok(4usize), |v| v.parse())?;
    let control = LazyPersistentIndex::open(&args[0])?;
    let pos3 = Positional3Index::open(&args[0])?;

    let filter = || -> Result<Vec<u32>, Box<dyn Error>> {
        let mut lists = Vec::with_capacity(query.len() - 15);
        for window in query.windows(16) {
            lists.push(pos3.search_content(window, workers)?);
        }
        lists.sort_by_key(Vec::len);
        let mut candidates = lists.first().cloned().unwrap_or_default();
        for list in lists.iter().skip(1) {
            candidates = intersect_doc_lists(&candidates, list);
            if candidates.is_empty() {
                break;
            }
        }
        let mut hits = Vec::new();
        for doc in candidates {
            if control.document_contains(doc, query, false)? {
                hits.push(doc);
            }
        }
        Ok(hits)
    };

    let expected = control.search_content(query)?;
    let actual = filter()?;
    if expected != actual {
        return Err(format!(
            "long filter mismatch: {} != {}",
            expected.len(),
            actual.len()
        )
        .into());
    }
    let (_, c50, c95) = measure_query(rounds, || Ok(control.search_content(query)?.len()))?;
    let (_, f50, f95) = measure_query(rounds, || Ok(filter()?.len()))?;
    println!(
        "LONG_FILTER query={} hits={} control_p50_ms={c50:.6} control_p95_ms={c95:.6} filter_p50_ms={f50:.6} filter_p95_ms={f95:.6}",
        args[1],
        expected.len()
    );
    Ok(())
}

fn profile_auto_partitions_cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() < 6 || !(args.len() - 2).is_multiple_of(2) {
        return Err("profile-auto-partitions ROUNDS TOTAL_WORKERS ACCEL_DIR CONTROL_DIR [ACCEL_DIR CONTROL_DIR...]".into());
    }
    let rounds: usize = args[0].parse()?;
    let total_workers: usize = args[1].parse()?;
    let pairs = args[2..].chunks_exact(2).collect::<Vec<_>>();
    let per = (total_workers / pairs.len().max(1)).max(1);
    let mut accelerated = Vec::new();
    let mut control = Vec::new();
    for pair in &pairs {
        accelerated.push(LazyPersistentIndex::open(&pair[0])?);
        control.push(LazyPersistentIndex::open(&pair[1])?);
    }
    println!(
        "AUTO_PARTITIONS partitions={} rounds={} total_workers={} per_partition_workers={}",
        pairs.len(),
        rounds,
        total_workers,
        per
    );
    for text in [
        "return",
        "timeout",
        "config",
        "error",
        "indexmanager",
        "personalrag",
        "namespace",
        "include",
    ] {
        let run = |indexes: &[LazyPersistentIndex]| -> Result<usize, personalrag_portable_search::SearchError> {
            std::thread::scope(|scope| {
                let mut handles = Vec::new();
                for index in indexes {
                    handles.push(scope.spawn(|| {
                        index
                            .search_content_with_worker_budget(text.as_bytes(), per)
                            .map(|hits| hits.len())
                    }));
                }
                let mut total = 0usize;
                for handle in handles {
                    total += handle.join().expect("auto partition panicked")?;
                }
                Ok(total)
            })
        };
        let expected = run(&control)?;
        let actual = run(&accelerated)?;
        if expected != actual {
            return Err(
                format!("auto partition mismatch for {text}: {expected} != {actual}").into(),
            );
        }
        let (_, c50, c95) = measure_query(rounds, || Ok(run(&control)?))?;
        let (_, a50, a95) = measure_query(rounds, || Ok(run(&accelerated)?))?;
        println!(
            "AUTO_PARTITION_CASE query={text} hits={expected} control_p50_ms={c50:.6} control_p95_ms={c95:.6} accelerated_p50_ms={a50:.6} accelerated_p95_ms={a95:.6}"
        );
    }
    Ok(())
}

fn profile_pos3_partitions_cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() < 6 || !(args.len() - 2).is_multiple_of(2) {
        return Err("profile-pos3-partitions ROUNDS TOTAL_WORKERS POS3_DIR CONTROL_DIR [POS3_DIR CONTROL_DIR...]".into());
    }
    let rounds: usize = args[0].parse()?;
    let total_workers: usize = args[1].parse()?;
    let pairs = args[2..].chunks_exact(2).collect::<Vec<_>>();
    let per = (total_workers / pairs.len().max(1)).max(1);
    let mut p3 = Vec::new();
    let mut ctrl = Vec::new();
    for pair in &pairs {
        p3.push(Positional3Index::open(&pair[0])?);
        ctrl.push(LazyPersistentIndex::open(&pair[1])?);
    }
    println!(
        "POS3_PARTITIONS partitions={} rounds={} total_workers={} per_partition_workers={}",
        pairs.len(),
        rounds,
        total_workers,
        per
    );
    for text in [
        "return",
        "timeout",
        "config",
        "error",
        "indexmanager",
        "personalrag",
        "namespace",
        "include",
    ] {
        let expected: usize = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for index in &ctrl {
                handles.push(scope.spawn(|| {
                    index
                        .search_content_with_worker_budget(text.as_bytes(), per)
                        .map(|hits| hits.len())
                }));
            }
            let mut total = 0usize;
            for handle in handles {
                total += handle.join().expect("control partition panicked")?;
            }
            Ok::<usize, personalrag_portable_search::SearchError>(total)
        })?;
        let actual: usize = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for index in &p3 {
                handles.push(scope.spawn(|| {
                    index
                        .search_content(text.as_bytes(), per)
                        .map(|hits| hits.len())
                }));
            }
            let mut total = 0usize;
            for handle in handles {
                total += handle.join().expect("PRPOS003 partition panicked")?;
            }
            Ok::<usize, personalrag_portable_search::SearchError>(total)
        })?;
        if expected != actual {
            return Err(
                format!("PRPOS003 partition mismatch for {text}: {expected} != {actual}").into(),
            );
        }
        let (_, c50, c95) = measure_query(rounds, || {
            std::thread::scope(|scope| {
                let mut handles = Vec::new();
                for index in &ctrl {
                    handles.push(scope.spawn(|| {
                        index
                            .search_content_with_worker_budget(text.as_bytes(), per)
                            .map(|hits| hits.len())
                    }));
                }
                let mut total = 0usize;
                for handle in handles {
                    total += handle.join().expect("control partition panicked")?;
                }
                Ok(total)
            })
        })?;
        let (_, p50, p95) = measure_query(rounds, || {
            std::thread::scope(|scope| {
                let mut handles = Vec::new();
                for index in &p3 {
                    handles.push(scope.spawn(|| {
                        index
                            .search_content(text.as_bytes(), per)
                            .map(|hits| hits.len())
                    }));
                }
                let mut total = 0usize;
                for handle in handles {
                    total += handle.join().expect("PRPOS003 partition panicked")?;
                }
                Ok(total)
            })
        })?;
        println!(
            "POS3_PARTITION_CASE query={text} hits={expected} control_p50_ms={c50:.6} control_p95_ms={c95:.6} pos3_p50_ms={p50:.6} pos3_p95_ms={p95:.6}"
        );
    }
    Ok(())
}

fn build_pos_sidecars_cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() < 2 {
        return Err(
            "build-pos-sidecars INDEX_DIR delta|svb|ef|block256 [THRESHOLD_PPM=500000] [DURABLE=1]"
                .into(),
        );
    }
    let codec = PosCodec::parse(&args[1])?;
    let threshold_ppm = args.get(2).map_or(Ok(500_000u32), |v| v.parse())?;
    let durable = args.get(3).is_none_or(|v| v != "0");
    let report = build_positional_sidecars(&args[0], codec, threshold_ppm, durable)?;
    println!(
        "POS_SIDECAR_BUILD codec={} threshold_ppm={} segments={} records={} occurrences={} bytes={} elapsed_ms={:.3}",
        codec.tag(),
        threshold_ppm,
        report.segments,
        report.records,
        report.occurrences,
        report.bytes,
        report.elapsed_ms
    );
    Ok(())
}

fn verify_pos_sidecars_cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() {
        return Err("verify-pos-sidecars INDEX_DIR [CODEC=ef]".into());
    }
    let codec = args
        .get(1)
        .map_or(Ok(PosCodec::production()), |value| PosCodec::parse(value))?;
    let started = Instant::now();
    verify_positional_sidecars(&args[0], codec)?;
    println!(
        "POS_VERIFY_PASS codec={} elapsed_ms={:.3}",
        codec.tag(),
        started.elapsed().as_secs_f64() * 1000.0
    );
    Ok(())
}

fn query_pos_cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() < 3 {
        return Err("query-pos INDEX_DIR CODEC QUERY [WORKERS=4]".into());
    }
    let codec = PosCodec::parse(&args[1])?;
    let workers = args.get(3).map_or(Ok(4usize), |v| v.parse())?;
    let open = Instant::now();
    let index = PositionalIndex::open(&args[0], codec)?;
    let open_ms = open.elapsed().as_secs_f64() * 1000.0;
    let started = Instant::now();
    let hits = index.search_content(args[2].as_bytes(), workers)?;
    let query_ms = started.elapsed().as_secs_f64() * 1000.0;
    println!(
        "POS_QUERY codec={} query={} hits={} open_ms={open_ms:.6} query_ms={query_ms:.6}",
        codec.tag(),
        args[2],
        hits.len()
    );
    Ok(())
}

fn profile_pos_cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() < 2 {
        return Err("profile-pos INDEX_DIR CODEC [ROUNDS=5] [WORKERS=4]".into());
    }
    let codec = PosCodec::parse(&args[1])?;
    let rounds = args.get(2).map_or(Ok(5usize), |v| v.parse())?;
    let workers = args.get(3).map_or(Ok(4usize), |v| v.parse())?;
    let positional = PositionalIndex::open(&args[0], codec)?;
    let control = LazyPersistentIndex::open(&args[0])?;
    println!(
        "POS_PROFILE codec={} rounds={} workers={}",
        codec.tag(),
        rounds,
        workers
    );
    for text in [
        "return",
        "timeout",
        "config",
        "error",
        "namespace",
        "include",
    ] {
        let expected = control.search_content(text.as_bytes())?;
        let actual = positional.search_content(text.as_bytes(), workers)?;
        if expected != actual {
            return Err(format!("positional mismatch for {text}").into());
        }
        let (_, control_p50, control_p95) = measure_query(rounds, || {
            Ok(control
                .search_content_with_workers(text.as_bytes(), workers)?
                .len())
        })?;
        let (_, pos_p50, pos_p95) = measure_query(rounds, || {
            Ok(positional.search_content(text.as_bytes(), workers)?.len())
        })?;
        println!(
            "POS_CASE query={text} hits={} control_p50_ms={control_p50:.6} control_p95_ms={control_p95:.6} pos_p50_ms={pos_p50:.6} pos_p95_ms={pos_p95:.6}",
            expected.len()
        );
    }
    Ok(())
}

fn profile_pos_partitions_cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() < 4 {
        return Err("profile-pos-partitions CODEC ROUNDS TOTAL_WORKERS INDEX_DIR...".into());
    }
    let codec = PosCodec::parse(&args[0])?;
    let rounds: usize = args[1].parse()?;
    let total_workers: usize = args[2].parse()?;
    let dirs = &args[3..];
    let per_partition_workers = (total_workers / dirs.len().max(1)).max(1);
    let mut positional = Vec::with_capacity(dirs.len());
    let mut control = Vec::with_capacity(dirs.len());
    for dir in dirs {
        positional.push(PositionalIndex::open(dir, codec)?);
        control.push(LazyPersistentIndex::open(dir)?);
    }
    println!(
        "POS_PARTITIONS codec={} partitions={} rounds={} total_workers={} per_partition_workers={}",
        codec.tag(),
        dirs.len(),
        rounds,
        total_workers,
        per_partition_workers
    );
    for text in [
        "return",
        "timeout",
        "config",
        "error",
        "namespace",
        "include",
    ] {
        let expected: usize = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for index in &control {
                handles.push(scope.spawn(|| {
                    index
                        .search_content_with_workers(text.as_bytes(), per_partition_workers)
                        .map(|hits| hits.len())
                }));
            }
            let mut total = 0usize;
            for handle in handles {
                total += handle.join().expect("control partition worker panicked")?;
            }
            Ok::<usize, personalrag_portable_search::SearchError>(total)
        })?;
        let actual: usize = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for index in &positional {
                handles.push(scope.spawn(|| {
                    index
                        .search_content(text.as_bytes(), per_partition_workers)
                        .map(|hits| hits.len())
                }));
            }
            let mut total = 0usize;
            for handle in handles {
                total += handle
                    .join()
                    .expect("positional partition worker panicked")?;
            }
            Ok::<usize, personalrag_portable_search::SearchError>(total)
        })?;
        if expected != actual {
            return Err(format!(
                "partition positional mismatch for {text}: {expected} vs {actual}"
            )
            .into());
        }
        let (_, control_p50, control_p95) = measure_query(rounds, || {
            std::thread::scope(|scope| {
                let mut handles = Vec::new();
                for index in &control {
                    handles.push(scope.spawn(|| {
                        index
                            .search_content_with_workers(text.as_bytes(), per_partition_workers)
                            .map(|hits| hits.len())
                    }));
                }
                let mut total = 0usize;
                for handle in handles {
                    total += handle.join().expect("control partition worker panicked")?;
                }
                Ok(total)
            })
        })?;
        let (_, pos_p50, pos_p95) = measure_query(rounds, || {
            std::thread::scope(|scope| {
                let mut handles = Vec::new();
                for index in &positional {
                    handles.push(scope.spawn(|| {
                        index
                            .search_content(text.as_bytes(), per_partition_workers)
                            .map(|hits| hits.len())
                    }));
                }
                let mut total = 0usize;
                for handle in handles {
                    total += handle
                        .join()
                        .expect("positional partition worker panicked")?;
                }
                Ok(total)
            })
        })?;
        println!(
            "POS_PARTITION_CASE query={text} hits={expected} control_p50_ms={control_p50:.6} control_p95_ms={control_p95:.6} pos_p50_ms={pos_p50:.6} pos_p95_ms={pos_p95:.6}"
        );
    }
    Ok(())
}

fn profile_pos_auto_cli(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() < 2 {
        return Err("profile-pos-auto INDEX_DIR QUERY [ROUNDS=10]".into());
    }
    let rounds = args.get(2).map_or(Ok(10usize), |v| v.parse())?;
    let lazy = LazyPersistentIndex::open(&args[0])?;
    let pos = PositionalIndex::open(&args[0], PosCodec::production())?;
    let expected = lazy.search_content_with_workers(args[1].as_bytes(), 4)?;
    let a = pos.search_content(args[1].as_bytes(), 4)?;
    let b = lazy.search_content(args[1].as_bytes())?;
    if expected != a || expected != b {
        return Err("pos auto mismatch".into());
    }
    let (_, explicit_p50, explicit_p95) = measure_query(rounds, || {
        Ok(pos.search_content(args[1].as_bytes(), 4)?.len())
    })?;
    let (_, auto_p50, auto_p95) = measure_query(rounds, || {
        Ok(lazy.search_content(args[1].as_bytes())?.len())
    })?;
    println!(
        "POS_AUTO query={} explicit_p50_ms={explicit_p50:.6} explicit_p95_ms={explicit_p95:.6} auto_p50_ms={auto_p50:.6} auto_p95_ms={auto_p95:.6}",
        args[1]
    );
    Ok(())
}

fn tune_build(args: &[String]) -> Result<(), Box<dyn Error>> {
    let detected = detected_available_memory_bytes();
    let tuning = if let Some(memory_mib) = args.first() {
        let memory_mib: u64 = memory_mib.parse()?;
        let cpus = args.get(1).map_or_else(
            || std::thread::available_parallelism().map_or(1, usize::from),
            |value| value.parse::<usize>().unwrap_or(1),
        );
        recommend_build_tuning(memory_mib.saturating_mul(1024 * 1024), cpus)
    } else {
        recommend_system_build_tuning()
    };
    println!(
        "BUILD_TUNING detected_available_bytes={} budget_bytes={} cpus={} segment_docs={} build_workers={} scan_workers={}",
        detected.map_or(0, |value| value),
        tuning.memory_budget_bytes,
        tuning.logical_cpus,
        tuning.segment_docs,
        tuning.build_workers,
        tuning.scan_workers,
    );
    Ok(())
}

fn diagnose_content(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() < 2 {
        return Err("diagnose-content INDEX_DIR QUERY".into());
    }
    let lazy = LazyPersistentIndex::open(&args[0])?;
    let plan = lazy.plan_content_query(args[1].as_bytes(), None)?;
    println!(
        "PLAN query={} mode={:?} workers={} estimated_candidates={} density_ppm={} positional_sidecars={}",
        args[1],
        plan.mode,
        plan.workers,
        plan.estimated_candidates,
        plan.estimated_density_ppm,
        lazy.positional_sidecars_available()
    );
    let index = PersistentIndex::open_with_workers(&args[0], false, 4)?;
    let diagnostics = index.diagnose_content(args[1].as_bytes())?;
    let total_ns = diagnostics.candidate_ns + diagnostics.verify_ns + diagnostics.expand_ns;
    let pct = |value: u128| -> f64 {
        if total_ns == 0 {
            0.0
        } else {
            value as f64 * 100.0 / total_ns as f64
        }
    };
    println!(
        "DIAG query={} segments={} candidates={} matched_units={} hit_docs={} candidate_ms={:.6} candidate_pct={:.2} verify_ms={:.6} verify_pct={:.2} expand_ms={:.6} expand_pct={:.2} best_q3={} second_q3={} best2_intersection={}",
        args[1],
        diagnostics.segments,
        diagnostics.candidates,
        diagnostics.matched_units,
        diagnostics.hit_docs,
        diagnostics.candidate_ns as f64 / 1_000_000.0,
        pct(diagnostics.candidate_ns),
        diagnostics.verify_ns as f64 / 1_000_000.0,
        pct(diagnostics.verify_ns),
        diagnostics.expand_ns as f64 / 1_000_000.0,
        pct(diagnostics.expand_ns),
        diagnostics.best_q3_candidates,
        diagnostics.second_q3_candidates,
        diagnostics.best2_q3_intersection,
    );
    Ok(())
}

fn profile_source(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() {
        return Err("profile-source INDEX_DIR [ROUNDS]".into());
    }
    let root = &args[0];
    let rounds = args.get(1).map_or(Ok(12usize), |value| value.parse())?;
    let query_workers = args.get(2).map_or(Ok(1usize), |value| value.parse())?;
    let open_workers = args.get(3).map_or(Ok(1usize), |value| value.parse())?;
    let fast_started = Instant::now();
    let index = PersistentIndex::open_with_workers(root, false, open_workers)?;
    let fast_open_ms = fast_started.elapsed().as_secs_f64() * 1000.0;
    let verified_started = Instant::now();
    let verified = PersistentIndex::open_with_workers(root, true, open_workers)?;
    let verified_open_ms = verified_started.elapsed().as_secs_f64() * 1000.0;
    drop(verified);
    println!(
        "PROFILE_HEADER docs={} fast_open_ms={fast_open_ms:.6} verified_open_ms={verified_open_ms:.6} rounds={rounds} query_workers={query_workers} open_workers={open_workers}",
        index.docs()
    );
    let cases = [
        ("q1-a", "a", false, None),
        ("q2-re", "re", false, None),
        ("q3-ret", "ret", false, None),
        ("long-return", "return", false, None),
        ("long-namespace", "namespace", false, None),
        ("long-timeout", "timeout", false, None),
        ("long-config", "config", false, None),
        ("long-error", "error", false, None),
        ("long-include", "include", false, None),
        ("long-struct", "struct", false, None),
        ("long-rare", "unique_marker_970", false, None),
        ("q1-a-first100", "a", false, Some(100usize)),
        ("q2-re-first100", "re", false, Some(100usize)),
        ("q3-ret-first100", "ret", false, Some(100usize)),
        ("long-return-first100", "return", false, Some(100usize)),
        (
            "long-namespace-first100",
            "namespace",
            false,
            Some(100usize),
        ),
        ("long-timeout-first100", "timeout", false, Some(100usize)),
        (
            "long-rare-first100",
            "unique_marker_970",
            false,
            Some(100usize),
        ),
        ("name-module-first100", "module_", true, Some(100usize)),
    ];
    for (label, text, names, limit) in cases {
        let (hits, p50, p95) = measure_query(rounds, || {
            let result = if let Some(limit) = limit {
                index.first_n(text.as_bytes(), names, limit)?
            } else if names {
                index.search_name(text.as_bytes())?
            } else {
                index.search_content_with_workers(text.as_bytes(), query_workers)?
            };
            Ok(result.len())
        })?;
        println!("PROFILE_CASE label={label} hits={hits} p50_ms={p50:.6} p95_ms={p95:.6}");
    }
    Ok(())
}

fn partition_query(
    indexes: &[LazyPersistentIndex],
    text: &[u8],
    names: bool,
    limit: Option<usize>,
) -> Result<usize, Box<dyn Error>> {
    if let Some(limit) = limit {
        let mut remaining = limit;
        let mut hits = 0usize;
        for index in indexes {
            if remaining == 0 {
                break;
            }
            let part = index.first_n(text, names, remaining)?;
            hits += part.len();
            remaining = limit.saturating_sub(hits);
        }
        return Ok(hits);
    }
    if names {
        let mut hits = 0usize;
        for index in indexes {
            hits += index.search_name(text)?.len();
        }
        return Ok(hits);
    }
    let per_partition_workers = (4 / indexes.len().max(1)).max(1);
    let result = std::thread::scope(|scope| {
        let handles = indexes
            .iter()
            .map(|index| {
                scope.spawn(move || {
                    index
                        .search_content_with_worker_budget(text, per_partition_workers)
                        .map(|hits| hits.len())
                })
            })
            .collect::<Vec<_>>();
        let mut total = 0usize;
        for handle in handles {
            let value = handle
                .join()
                .map_err(|_| "partition query worker panicked".to_owned())??;
            total = total
                .checked_add(value)
                .ok_or_else(|| "partition hit count overflow".to_owned())?;
        }
        Ok::<usize, Box<dyn Error>>(total)
    })?;
    Ok(result)
}

fn profile_partitions(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() < 2 {
        return Err("profile-partitions ROUNDS INDEX_DIR...".into());
    }
    let rounds: usize = args[0].parse()?;
    let roots = &args[1..];
    if roots.len() > 4 {
        return Err("profile-partitions supports at most 4 partitions".into());
    }
    let open_started = Instant::now();
    let mut indexes = Vec::with_capacity(roots.len());
    for root in roots {
        indexes.push(LazyPersistentIndex::open(root)?);
    }
    let fast_open_ms = open_started.elapsed().as_secs_f64() * 1000.0;
    let docs = indexes.iter().map(LazyPersistentIndex::docs).sum::<u64>();
    println!(
        "PARTITION_HEADER docs={} partitions={} fast_open_ms={fast_open_ms:.6} total_worker_budget=4 rounds={rounds}",
        docs,
        indexes.len(),
    );
    let cases = [
        ("q1-a", "a", false, None),
        ("q2-re", "re", false, None),
        ("q3-ret", "ret", false, None),
        ("long-return", "return", false, None),
        ("long-namespace", "namespace", false, None),
        ("long-timeout", "timeout", false, None),
        ("long-config", "config", false, None),
        ("long-error", "error", false, None),
        ("long-include", "include", false, None),
        ("long-struct", "struct", false, None),
        ("long-rare", "unique_marker_970", false, None),
        ("q1-a-first100", "a", false, Some(100usize)),
        ("q2-re-first100", "re", false, Some(100usize)),
        ("q3-ret-first100", "ret", false, Some(100usize)),
        ("long-return-first100", "return", false, Some(100usize)),
        (
            "long-namespace-first100",
            "namespace",
            false,
            Some(100usize),
        ),
        ("long-timeout-first100", "timeout", false, Some(100usize)),
        (
            "long-rare-first100",
            "unique_marker_970",
            false,
            Some(100usize),
        ),
        ("name-module-first100", "module_", true, Some(100usize)),
    ];
    for (label, text, names, limit) in cases {
        let (hits, p50, p95) = measure_query(rounds, || {
            partition_query(&indexes, text.as_bytes(), names, limit)
        })?;
        println!("PARTITION_CASE label={label} hits={hits} p50_ms={p50:.6} p95_ms={p95:.6}");
    }
    Ok(())
}

fn incremental_profile(store: &PathBuf, rounds: usize) -> Result<(), Box<dyn Error>> {
    let open_started = Instant::now();
    let index = MergedIndex::open(store, false)?;
    let open_ms = open_started.elapsed().as_secs_f64() * 1000.0;
    let mut samples = Vec::with_capacity(rounds);
    let mut single_samples = Vec::with_capacity(rounds);
    let mut four_samples = Vec::with_capacity(rounds);
    let session = MergedSearchSession::open(store, false, 4)?;
    let mut pooled_samples = Vec::with_capacity(rounds);
    let mut hits = 0usize;
    for _ in 0..rounds {
        let started = Instant::now();
        hits = index.search_content(b"return")?.len();
        samples.push(started.elapsed());
        let started = Instant::now();
        let single = index.search_content_with_workers(b"return", 1)?;
        single_samples.push(started.elapsed());
        let started = Instant::now();
        let four = index.search_content_with_workers(b"return", 4)?;
        four_samples.push(started.elapsed());
        let started = Instant::now();
        let pooled = session.search_content_with_workers(b"return", 4)?;
        pooled_samples.push(started.elapsed());
        if single.len() != hits || four.len() != hits || pooled.len() != hits {
            return Err("incremental worker result mismatch".into());
        }
    }
    let query_p95 = percentile(&mut samples, 0.95);
    let single_p95 = percentile(&mut single_samples, 0.95);
    let four_p95 = percentile(&mut four_samples, 0.95);
    let pooled_p95 = percentile(&mut pooled_samples, 0.95);
    let (_, q3_auto_p50, q3_auto_p95) =
        measure_query(rounds, || Ok(index.search_content(b"ret")?.len()))?;
    let (_, q3_pool_p50, q3_pool_p95) =
        measure_query(rounds, || Ok(session.search_content(b"ret")?.len()))?;
    let rare_query = b"__personalrag_never_existing_rare_marker__";
    let (_, rare_first_p50, rare_first_p95) =
        measure_query(rounds, || Ok(index.first_n(rare_query, false, 100)?.len()))?;
    println!(
        "INCR_PROFILE generation={} deltas={} live_docs={} open_ms={open_ms:.6} return_hits={hits} query_p95_ms={query_p95:.6} single_p95_ms={single_p95:.6} four_p95_ms={four_p95:.6} pooled4_p95_ms={pooled_p95:.6} q3_auto_p50_ms={q3_auto_p50:.6} q3_auto_p95_ms={q3_auto_p95:.6} q3_pool_p50_ms={q3_pool_p50:.6} q3_pool_p95_ms={q3_pool_p95:.6} rare_first100_p50_ms={rare_first_p50:.6} rare_first100_p95_ms={rare_first_p95:.6}",
        index.generation(),
        index.delta_count(),
        index.live_docs(),
    );
    Ok(())
}

fn bench_incremental(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() < 4 {
        return Err("bench-incremental DOCS STORE DELTAS CHANGES_PER_DELTA".into());
    }
    let docs: usize = args[0].parse()?;
    let store = PathBuf::from(&args[1]);
    let deltas: usize = args[2].parse()?;
    let changes_per_delta: usize = args[3].parse()?;
    if docs == 0 || changes_per_delta == 0 || changes_per_delta > docs {
        return Err("invalid incremental benchmark sizes".into());
    }
    let _ = std::fs::remove_dir_all(&store);
    let (base_documents, _) = synthetic_source(docs);
    let logical = base_documents
        .iter()
        .enumerate()
        .map(|(index, document)| LogicalDocument::new(index as u64 + 1, document.clone()))
        .collect::<Vec<_>>();
    let options = BuildOptions {
        mode: BuildMode::Adaptive,
        segment_docs: 5_000,
        workers: 2,
    };
    let init_started = Instant::now();
    initialize_generation(&store, &logical, &options)?;
    println!(
        "INCR_INIT docs={} elapsed_ms={:.3}",
        docs,
        init_started.elapsed().as_secs_f64() * 1000.0
    );
    let mut catalog = CatalogSnapshot {
        generation: 0,
        next_logical_id: docs as u64 + 1,
        ..CatalogSnapshot::default()
    };
    for (index, document) in base_documents.iter().enumerate() {
        catalog.live.insert(
            document.key.clone(),
            CatalogEntry {
                logical_id: index as u64 + 1,
                key: document.key.clone(),
                last_generation: 0,
            },
        );
    }
    incremental_profile(&store, 5)?;
    for delta in 1..=deltas {
        let start = (delta * changes_per_delta * 17) % docs;
        let mut changes = Vec::with_capacity(changes_per_delta);
        for offset in 0..changes_per_delta {
            let index = (start + offset) % docs;
            let key = base_documents[index].key.clone();
            let mut document = base_documents[index].clone();
            document.normalized_content.extend_from_slice(
                format!(" incremental_generation_{delta} changed_return ").as_bytes(),
            );
            changes.push(DocumentChange {
                kind: ChangeKind::Upsert,
                key,
                document: Some(document),
            });
        }
        let batch = ChangeBatch {
            expected_base_generation: catalog.generation,
            changes,
        };
        let plan = plan_incremental_update(&catalog, &batch, IncrementalPolicy::default())?;
        let started = Instant::now();
        publish_incremental_update(&store, &plan, &options)?;
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        catalog = apply_update_plan(&catalog, &plan)?;
        println!(
            "INCR_PUBLISH generation={} upserts={} tombstones={} elapsed_ms={elapsed_ms:.3}",
            plan.next_generation,
            plan.upserts.len(),
            plan.tombstones.len(),
        );
        if delta == 1 || delta == 5 || delta == deltas {
            incremental_profile(&store, 5)?;
        }
    }
    let compact_started = Instant::now();
    let compact = compact_generation(&store, &options)?;
    println!(
        "INCR_COMPACT generation={} live_docs={} elapsed_ms={:.3}",
        compact.generation,
        compact.live_docs,
        compact_started.elapsed().as_secs_f64() * 1000.0
    );
    incremental_profile(&store, 5)?;
    Ok(())
}

fn query_lazy(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.len() < 3 {
        return Err("query-lazy INDEX_DIR content|name QUERY [LIMIT] [WORKERS]".into());
    }
    let open_started = Instant::now();
    let index = LazyPersistentIndex::open(&args[0])?;
    let open_elapsed = open_started.elapsed();
    let names = match args[1].as_str() {
        "content" => false,
        "name" => true,
        other => return Err(format!("bad query kind: {other}").into()),
    };
    let limit = args
        .get(3)
        .map(|value| value.parse::<usize>())
        .transpose()?
        .and_then(|value| (value != 0).then_some(value));
    let workers = args
        .get(4)
        .map(|value| value.parse::<usize>())
        .transpose()?;
    let query_started = Instant::now();
    let hits = if let Some(limit) = limit {
        index.first_n(args[2].as_bytes(), names, limit)?
    } else if names {
        index.search_name(args[2].as_bytes())?
    } else if let Some(workers) = workers {
        index.search_content_with_workers(args[2].as_bytes(), workers)?
    } else {
        index.search_content(args[2].as_bytes())?
    };
    let query_elapsed = query_started.elapsed();
    println!(
        "LAZY_OPEN_MS {:.6} QUERY_MS {:.6} OPENED_SEGMENTS {} HITS {}",
        open_elapsed.as_secs_f64() * 1000.0,
        query_elapsed.as_secs_f64() * 1000.0,
        index.opened_segments(),
        hits.len(),
    );
    Ok(())
}

fn compaction_status(args: &[String]) -> Result<(), Box<dyn Error>> {
    if args.is_empty() {
        return Err("compaction-status STORE".into());
    }
    let index = MergedIndex::open(&args[0], false)?;
    let decision = index.auto_compaction_decision()?;
    let m = decision.metrics;
    let p = decision.policy;
    println!(
        "COMPACTION_STATUS generation={} live_docs={} deltas={} base_bytes={} delta_bytes={} tombstones={} max_deltas={} max_delta_ratio={:.3} max_tombstone_ratio={:.3} reason_count={} reason_bytes={} reason_tombstones={} recommended={}",
        index.generation(),
        m.live_docs,
        m.delta_count,
        m.base_bytes,
        m.delta_bytes,
        m.tombstone_events,
        p.max_delta_count,
        p.max_delta_bytes_ratio,
        p.max_tombstone_ratio,
        decision.reasons.delta_count,
        decision.reasons.delta_bytes,
        decision.reasons.tombstones,
        decision.recommended,
    );
    Ok(())
}

fn self_test() -> Result<(), Box<dyn Error>> {
    let root = env::temp_dir().join(format!("personalrag-rust-selftest-{}", std::process::id()));
    let corpus = root.join("corpus");
    let index_dir = root.join("index");
    std::fs::create_dir_all(corpus.join("sub"))?;
    std::fs::write(corpus.join("Alpha.txt"), b"Return TIMEOUT handler")?;
    std::fs::write(
        corpus.join("sub/Beta.rs"),
        "日本語の検索システム timeout".as_bytes(),
    )?;
    std::fs::write(
        corpus.join("sub/Duplicate.rs"),
        "日本語の検索システム timeout".as_bytes(),
    )?;
    let docs = build_disk_corpus(&corpus, None, 1024 * 1024)?;
    build_index(
        &docs,
        &index_dir,
        &BuildOptions {
            mode: BuildMode::Adaptive,
            segment_docs: 2,
            workers: 2,
        },
    )?;
    let index = PersistentIndex::open(&index_dir, true)?;
    assert_eq!(index.search_content(b"timeout")?, vec![0, 1, 2]);
    assert_eq!(index.search_content("検索".as_bytes())?, vec![1, 2]);
    assert_eq!(index.search_content(b"re")?, vec![0]);
    assert_eq!(index.search_name(b"beta")?, vec![1]);
    assert_eq!(index.first_n(b"timeout", false, 2)?, vec![0, 1]);
    std::fs::remove_dir_all(root)?;
    println!("SELF_TEST_PASS");
    Ok(())
}

fn parse_mode(value: &str) -> Result<BuildMode, Box<dyn Error>> {
    match value {
        "direct" => Ok(BuildMode::Direct),
        "dedup" => Ok(BuildMode::Dedup),
        "adaptive" => Ok(BuildMode::Adaptive),
        _ => Err(format!("bad build mode: {value}").into()),
    }
}

fn required<'a>(args: &'a [String], index: usize, name: &str) -> Result<&'a str, Box<dyn Error>> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| format!("missing {name}").into())
}

fn usage() {
    eprintln!(
        "pr_portable build-disk ROOT direct|dedup|adaptive INDEX_DIR SEGMENT_DOCS WORKERS [MAX_DOCS] [MAX_FILE_BYTES]"
    );
    eprintln!(
        "pr_portable build-disk-parallel ROOT direct|dedup|adaptive INDEX_DIR SEGMENT_DOCS WORKERS [MAX_DOCS] [MAX_FILE_BYTES] [SCAN_WORKERS]"
    );
    eprintln!("pr_portable verify INDEX_DIR");
    eprintln!("pr_portable query INDEX_DIR content|name QUERY [LIMIT]");
    eprintln!("pr_portable query-lazy INDEX_DIR content|name QUERY [LIMIT]");
    eprintln!("pr_portable build-synthetic DOCS INDEX_DIR SEGMENT_DOCS WORKERS");
    eprintln!("pr_portable build-synthetic-fast DOCS INDEX_DIR SEGMENT_DOCS WORKERS");
    eprintln!("pr_portable profile-source INDEX_DIR [ROUNDS] [QUERY_WORKERS] [OPEN_WORKERS]");
    eprintln!("pr_portable profile-pool INDEX_DIR [ROUNDS] [WORKERS]");
    eprintln!("pr_portable profile-auto-query INDEX_DIR QUERY [ROUNDS=50] [WORKERS=4]");
    eprintln!("pr_portable profile-q2 INDEX_DIR [ROUNDS]");
    eprintln!("pr_portable build-q2-sidecars INDEX_DIR [DURABLE=1]");
    eprintln!(
        "pr_portable build-pos2-sidecars INDEX_DIR [Q3_THRESHOLD_PPM=500000] [CHILD_THRESHOLD_PPM=100000] [DURABLE=1]"
    );
    eprintln!(
        "pr_portable build-pos23-sidecars INDEX_DIR [Q3_THRESHOLD_PPM=500000] [POS2_CHILD_THRESHOLD_PPM=500000] [POS3_CHILD_THRESHOLD_PPM=500000] [MAX_GRAM=16] [POLICY=adaptive] [DURABLE=1]"
    );
    eprintln!(
        "pr_portable build-pos3-sidecars INDEX_DIR [Q3_THRESHOLD_PPM=500000] [CHILD_THRESHOLD_PPM=500000] [MAX_GRAM=16] [POLICY=adaptive] [DURABLE=1]"
    );
    eprintln!(
        "pr_portable build-pos-sidecars INDEX_DIR delta|svb|ef|block256 [THRESHOLD_PPM=500000] [DURABLE=1]"
    );
    eprintln!("pr_portable verify-pos-sidecars INDEX_DIR [CODEC=ef]");
    eprintln!("pr_portable query-pos INDEX_DIR CODEC QUERY [WORKERS=4]");
    eprintln!("pr_portable profile-pos INDEX_DIR CODEC [ROUNDS=5] [WORKERS=4]");
    eprintln!("pr_portable profile-pos-partitions CODEC ROUNDS TOTAL_WORKERS INDEX_DIR...");
    eprintln!("pr_portable tune-build [MEMORY_MIB] [CPUS]");
    eprintln!("pr_portable diagnose-content INDEX_DIR QUERY");
    eprintln!("pr_portable profile-partitions ROUNDS INDEX_DIR...");
    eprintln!("pr_portable bench-incremental DOCS STORE DELTAS CHANGES_PER_DELTA");
    eprintln!("pr_portable compaction-status STORE");
    eprintln!("pr_portable self-test");
}
