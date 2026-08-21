use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use personalrag_portable_search::{
    AccelerationProfile, BuildMode, BuildOptions, DiskPathBuildConfig, DiskPathInput,
    LogicalDocumentIdentity, MergedIndex, MergedSearchSession, VNextDocumentInput,
    build_disk_path_inputs_index_unified, initialize_generation_from_built_index,
    initialize_vnext_generation_store, open_vnext_published_generation, verify_generation,
    verify_index, verify_vnext_generation_store,
};

#[derive(Clone, Copy, Debug)]
struct RunTimes {
    perf_build: Duration,
    perf_finalize: Duration,
    materialize: Duration,
    vnext_build: Duration,
    open_and_query: Duration,
    total: Duration,
}

fn temp_root(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "personalrag-system-full-{label}-{}-{nonce}",
        std::process::id()
    ))
}

fn make_inputs(root: &Path, docs: usize, payload_bytes: usize) -> Vec<DiskPathInput> {
    fs::create_dir_all(root).unwrap();
    let mut inputs = Vec::with_capacity(docs);
    for row in 0..docs {
        let group = root.join(format!("g{:03}", row % 127));
        fs::create_dir_all(&group).unwrap();
        let path = group.join(format!("doc_{row:07}.txt"));
        let prefix = format!(
            "timeout marker row={row:07} group={:03} alpha={} beta={} ",
            row % 127,
            row.wrapping_mul(2_654_435_761usize) % 1_000_003,
            row.wrapping_mul(1_146_067_499usize) % 1_000_033,
        );
        let mut bytes = prefix.into_bytes();
        while bytes.len() < payload_bytes {
            bytes.push(b'a' + ((row + bytes.len()) % 26) as u8);
        }
        bytes.truncate(payload_bytes.max(1));
        fs::write(&path, &bytes).unwrap();
        let display = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        inputs.push(DiskPathInput {
            path: path.clone(),
            display_path: display,
            size_bytes: bytes.len() as u64,
            content_path: Some(path),
            index_content: true,
        });
    }
    inputs
}

fn median(mut values: Vec<Duration>) -> Duration {
    values.sort_unstable();
    values[values.len() / 2]
}

fn run_once(
    corpus: &Path,
    inputs: Vec<DiskPathInput>,
    acceleration: AccelerationProfile,
    include_vnext: bool,
    label: &str,
) -> Result<RunTimes, Box<dyn std::error::Error>> {
    let out = temp_root(label);
    let base = out.join("base-index");
    let vnext = out.join("vnext-store");
    let options = BuildOptions {
        mode: BuildMode::Adaptive,
        segment_docs: 5_000,
        workers: 4,
    };
    let cancel = AtomicBool::new(false);
    let config = DiskPathBuildConfig {
        max_docs: None,
        max_file_bytes: 64 * 1024 * 1024,
        build: &options,
        scan_workers: 8,
        hydration_batch_bytes: 128 * 1024 * 1024,
        cancel: Some(&cancel),
    };

    let total_started = Instant::now();
    let started = Instant::now();
    let report =
        build_disk_path_inputs_index_unified(corpus, inputs, &base, config, acceleration, |_| {})?;
    let perf_build = started.elapsed();

    let started = Instant::now();
    verify_index(&base)?;
    let identities = report
        .display_paths
        .iter()
        .enumerate()
        .map(|(row, path)| LogicalDocumentIdentity::new(row as u64 + 1, path.clone(), path.clone()))
        .collect::<Vec<_>>();
    initialize_generation_from_built_index(&out, &base, &identities)?;
    verify_generation(&out)?;
    let perf_finalize = started.elapsed();

    let (materialize, vnext_build) = if include_vnext {
        let started = Instant::now();
        let perf = MergedIndex::open(&out, true)?;
        let documents = perf
            .live_documents()?
            .into_iter()
            .map(|document| {
                VNextDocumentInput::new(
                    document.logical_id,
                    document.document.display_path,
                    document.document.normalized_content,
                )
            })
            .collect::<Vec<_>>();
        let materialize = started.elapsed();

        let started = Instant::now();
        initialize_vnext_generation_store(&vnext, &documents, 5_000)?;
        verify_vnext_generation_store(&vnext)?;
        (materialize, started.elapsed())
    } else {
        (Duration::ZERO, Duration::ZERO)
    };

    let started = Instant::now();
    let perf_session = MergedSearchSession::open(&out, false, 4)?;
    let perf_hits = perf_session.search_content(b"timeout")?;
    if include_vnext {
        let vnext_hits = open_vnext_published_generation(&vnext)?.search_content(b"timeout")?;
        assert_eq!(perf_hits, vnext_hits);
    }
    assert_eq!(perf_hits.len(), report.build.docs);
    let open_and_query = started.elapsed();

    let total = total_started.elapsed();
    fs::remove_dir_all(&out)?;
    Ok(RunTimes {
        perf_build,
        perf_finalize,
        materialize,
        vnext_build,
        open_and_query,
        total,
    })
}

fn median_field(values: &[RunTimes], field: fn(RunTimes) -> Duration) -> Duration {
    median(values.iter().copied().map(field).collect())
}

fn print_summary(mode: &str, docs: usize, payload_bytes: usize, values: &[RunTimes]) {
    println!(
        "SYSTEM_FULL_BUILD mode={mode} docs={docs} payload_bytes={payload_bytes} perf_build_ms={:.3} perf_finalize_ms={:.3} materialize_ms={:.3} vnext_build_ms={:.3} open_query_ms={:.3} total_ms={:.3}",
        median_field(values, |v| v.perf_build).as_secs_f64() * 1e3,
        median_field(values, |v| v.perf_finalize).as_secs_f64() * 1e3,
        median_field(values, |v| v.materialize).as_secs_f64() * 1e3,
        median_field(values, |v| v.vnext_build).as_secs_f64() * 1e3,
        median_field(values, |v| v.open_and_query).as_secs_f64() * 1e3,
        median_field(values, |v| v.total).as_secs_f64() * 1e3,
    );
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let docs = std::env::var("PR_BENCH_DOCS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000usize);
    let payload_bytes = std::env::var("PR_BENCH_PAYLOAD_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4_096usize);
    let rounds = std::env::var("PR_BENCH_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3usize)
        .max(1);
    let include_vnext = std::env::var("PR_BENCH_VNEXT")
        .ok()
        .is_none_or(|value| value != "0");
    let corpus = temp_root("corpus");
    let inputs = make_inputs(&corpus, docs, payload_bytes);

    let _ = run_once(
        &corpus,
        inputs[..inputs.len().min(2_000)].to_vec(),
        AccelerationProfile::None,
        include_vnext,
        "warm",
    )?;

    let mut full = Vec::with_capacity(rounds);
    let mut balanced = Vec::with_capacity(rounds);
    let mut minimal = Vec::with_capacity(rounds);
    for round in 0..rounds {
        let order = match round % 3 {
            0 => [0u8, 1, 2],
            1 => [1u8, 2, 0],
            _ => [2u8, 0, 1],
        };
        for mode in order {
            match mode {
                0 => full.push(run_once(
                    &corpus,
                    inputs.clone(),
                    AccelerationProfile::Full,
                    include_vnext,
                    "full",
                )?),
                1 => balanced.push(run_once(
                    &corpus,
                    inputs.clone(),
                    AccelerationProfile::Balanced,
                    include_vnext,
                    "balanced",
                )?),
                _ => minimal.push(run_once(
                    &corpus,
                    inputs.clone(),
                    AccelerationProfile::None,
                    include_vnext,
                    "minimal",
                )?),
            }
        }
    }

    print_summary("full", docs, payload_bytes, &full);
    print_summary("balanced", docs, payload_bytes, &balanced);
    print_summary("minimal", docs, payload_bytes, &minimal);
    let full_total = median_field(&full, |v| v.total).as_secs_f64();
    let balanced_total = median_field(&balanced, |v| v.total).as_secs_f64();
    let minimal_total = median_field(&minimal, |v| v.total).as_secs_f64();
    println!(
        "SYSTEM_FULL_BUILD_AB mode=balanced docs={docs} payload_bytes={payload_bytes} speedup={:.3} reduction_pct={:.2}",
        full_total / balanced_total.max(f64::EPSILON),
        (1.0 - balanced_total / full_total.max(f64::EPSILON)) * 100.0,
    );
    println!(
        "SYSTEM_FULL_BUILD_AB mode=minimal docs={docs} payload_bytes={payload_bytes} speedup={:.3} reduction_pct={:.2}",
        full_total / minimal_total.max(f64::EPSILON),
        (1.0 - minimal_total / full_total.max(f64::EPSILON)) * 100.0,
    );
    fs::remove_dir_all(corpus)?;
    Ok(())
}
