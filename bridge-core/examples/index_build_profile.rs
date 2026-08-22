use std::{
    env,
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Arc},
    time::{Duration, Instant},
};

use personalrag_gui_bridge_core::{scan_files, ScanExclusions, ScannerMode};
use personalrag_portable_search::{
    build_disk_path_inputs_index_unified, recommend_system_build_tuning, verify_index,
    AccelerationProfile, BuildMode, BuildOptions, DiskPathBuildConfig, DiskPathBuildReport,
    DiskPathInput,
};
use serde_json::{json, Value};

const MIB: u64 = 1024 * 1024;
const DEFAULT_MAX_FILE_BYTES: u64 = 32 * MIB;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProfileMode {
    Warm,
    Cold,
}

impl ProfileMode {
    fn parse(value: &str) -> Result<Self, Box<dyn Error>> {
        match value.to_ascii_lowercase().as_str() {
            "warm" => Ok(Self::Warm),
            "cold" => Ok(Self::Cold),
            _ => Err("MODE must be warm or cold".into()),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Warm => "warm",
            Self::Cold => "cold",
        }
    }
}

fn duration_ms(value: Duration) -> f64 {
    value.as_secs_f64() * 1000.0
}

fn parse_optional<T>(args: &[String], index: usize, default: T) -> Result<T, Box<dyn Error>>
where
    T: std::str::FromStr,
    T::Err: Error + 'static,
{
    args.get(index)
        .map_or(Ok(default), |value| Ok(value.parse::<T>()?))
}

fn prepare_output(path: &Path) -> Result<(), Box<dyn Error>> {
    if path.exists() {
        fs::remove_dir_all(path)?;
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    Ok(())
}

fn report_json(
    label: &str,
    report: &DiskPathBuildReport,
    call_wall: Duration,
    verify_wall: Duration,
) -> Value {
    let timings = report.timings;
    json!({
        "label": label,
        "sourceFiles": report.source_files,
        "processedFiles": report.processed_files,
        "indexedFiles": report.build.docs,
        "skippedFiles": report.skipped_files,
        "bytesRead": report.bytes_read,
        "segments": report.build.segments,
        "indexBytes": report.build.index_bytes,
        "callWallMs": duration_ms(call_wall),
        "buildElapsedMs": duration_ms(report.build.elapsed),
        "verifyWallMs": duration_ms(verify_wall),
        "timings": {
            "hydrationWallMs": duration_ms(timings.hydration_wall),
            "segmentSampleWorkMs": duration_ms(timings.segment_sample_work),
            "segmentCoreWorkMs": duration_ms(timings.segment_core_work),
            "nameGramsWorkMs": duration_ms(timings.name_grams_work),
            "dedupWorkMs": duration_ms(timings.dedup_work),
            "contentGramsWorkMs": duration_ms(timings.content_grams_work),
            "contentPostWorkMs": duration_ms(timings.content_post_work),
            "namePostWorkMs": duration_ms(timings.name_post_work),
            "segmentWriteWorkMs": duration_ms(timings.segment_write_work),
            "accelerationWorkMs": duration_ms(timings.acceleration_work),
            "manifestWriteWallMs": duration_ms(timings.manifest_write_wall),
            "workerTimesAreSummed": true
        }
    })
}

fn run_build(
    label: &str,
    root: &Path,
    inputs: &[DiskPathInput],
    output: &Path,
    options: &BuildOptions,
    hydration_workers: usize,
    max_file_bytes: u64,
    hydration_batch_bytes: u64,
) -> Result<Value, Box<dyn Error>> {
    prepare_output(output)?;
    println!(
        "PROFILE_RUN_BEGIN label={label} hydration_workers={hydration_workers} output={}",
        output.display()
    );

    let started = Instant::now();
    let report = build_disk_path_inputs_index_unified(
        root,
        inputs.to_vec(),
        output,
        DiskPathBuildConfig {
            max_docs: None,
            max_file_bytes,
            build: options,
            scan_workers: hydration_workers,
            hydration_batch_bytes,
            cancel: None,
        },
        AccelerationProfile::Balanced,
        |_| {},
    )?;
    let call_wall = started.elapsed();

    let verify_started = Instant::now();
    verify_index(output)?;
    let verify_wall = verify_started.elapsed();

    let payload = report_json(label, &report, call_wall, verify_wall);
    println!("PROFILE_RUN_JSON {}", serde_json::to_string(&payload)?);
    Ok(payload)
}

fn run() -> Result<(), Box<dyn Error>> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.len() < 4 {
        return Err(
            "usage: index_build_profile MODE ROOT OUTPUT_ROOT HYDRATION_WORKERS [BUILD_WORKERS] [SEGMENT_DOCS] [MAX_FILE_BYTES] [HYDRATION_BATCH_BYTES]"
                .into(),
        );
    }

    let mode = ProfileMode::parse(&args[0])?;
    let root = PathBuf::from(&args[1]);
    let output_root = PathBuf::from(&args[2]);
    let hydration_workers = args[3].parse::<usize>()?;
    if hydration_workers == 0 {
        return Err("HYDRATION_WORKERS must be greater than zero".into());
    }

    let tuning = recommend_system_build_tuning();
    let build_workers = parse_optional(&args, 4, tuning.build_workers)?.max(1);
    let segment_docs = parse_optional(&args, 5, tuning.segment_docs)?.max(1);
    let max_file_bytes = parse_optional(&args, 6, DEFAULT_MAX_FILE_BYTES)?;
    let default_batch = (tuning.memory_budget_bytes / 8).clamp(32 * MIB, 128 * MIB);
    let hydration_batch_bytes = parse_optional(&args, 7, default_batch)?;

    if !root.is_dir() {
        return Err(format!("root is not a directory: {}", root.display()).into());
    }

    if mode == ProfileMode::Cold {
        eprintln!(
            "COLD_RUN_ISOLATION_REQUIRED: run exactly one hydration worker count per cold-cache session; reboot or otherwise establish a verified cold cache before the next candidate."
        );
    }

    let scan_started = Instant::now();
    let scan = scan_files(
        &root,
        max_file_bytes,
        ScannerMode::Auto,
        &ScanExclusions::default(),
        Arc::new(AtomicBool::new(false)),
        Arc::new(|_| {}),
    )
    .map_err(io::Error::other)?;
    let scan_wall = scan_started.elapsed();
    let scan_progress = scan.progress;
    let mut files = scan.files;
    files.sort_by(|left, right| left.display_path.cmp(&right.display_path));
    let inputs = files
        .into_iter()
        .map(|file| DiskPathInput {
            path: file.path,
            display_path: file.display_path,
            size_bytes: file.size_bytes,
            content_path: None,
            index_content: file.index_content,
        })
        .collect::<Vec<_>>();

    let configuration = json!({
        "mode": mode.as_str(),
        "root": root,
        "outputRoot": output_root,
        "hydrationWorkers": hydration_workers,
        "buildWorkers": build_workers,
        "segmentDocs": segment_docs,
        "maxFileBytes": max_file_bytes,
        "hydrationBatchBytes": hydration_batch_bytes,
        "productionAccelerationProfile": "balanced",
        "scanWallMs": duration_ms(scan_wall),
        "discoveredEntries": scan_progress.discovered_entries,
        "discoveredFileEntries": scan_progress.file_entries,
        "selectedFiles": inputs.len(),
        "selectedBytes": scan_progress.selected_bytes,
        "note": "PR_PROFILE_BUILD=1 adds per-segment BUILD_SEGMENT_WALL detail; summed worker timings can exceed wall time."
    });
    println!(
        "PROFILE_CONFIG_JSON {}",
        serde_json::to_string(&configuration)?
    );

    let options = BuildOptions {
        mode: BuildMode::Adaptive,
        segment_docs,
        workers: build_workers,
    };

    let measured = match mode {
        ProfileMode::Cold => run_build(
            "cold",
            &root,
            &inputs,
            &output_root.join("cold"),
            &options,
            hydration_workers,
            max_file_bytes,
            hydration_batch_bytes,
        )?,
        ProfileMode::Warm => {
            let _prime = run_build(
                "warm-prime",
                &root,
                &inputs,
                &output_root.join("warm-prime"),
                &options,
                hydration_workers,
                max_file_bytes,
                hydration_batch_bytes,
            )?;
            run_build(
                "warm-measured",
                &root,
                &inputs,
                &output_root.join("warm-measured"),
                &options,
                hydration_workers,
                max_file_bytes,
                hydration_batch_bytes,
            )?
        }
    };

    println!(
        "PROFILE_SUMMARY_JSON {}",
        serde_json::to_string(&json!({
            "configuration": configuration,
            "measured": measured
        }))?
    );
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
