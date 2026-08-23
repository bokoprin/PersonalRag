use std::{
    env,
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Arc},
    time::{Duration, Instant},
};

use personalrag_gui_bridge_core::{
    resolve_production_build_config, scan_files, ProductionBuildConfig, ScanExclusions, ScannerMode,
};
use personalrag_portable_search::{
    build_disk_path_inputs_index_unified, verify_index, AccelerationProfile, BuildMode,
    BuildOptions, DiskPathBuildConfig, DiskPathBuildReport, DiskPathInput,
};
use serde_json::{json, Value};

const MIB: u64 = 1024 * 1024;
const DEFAULT_MAX_FILE_BYTES: u64 = 32 * MIB;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProfileMode {
    Config,
    Warm,
    Cold,
}

impl ProfileMode {
    fn parse(value: &str) -> Result<Self, Box<dyn Error>> {
        match value.to_ascii_lowercase().as_str() {
            "config" => Ok(Self::Config),
            "warm" => Ok(Self::Warm),
            "cold" => Ok(Self::Cold),
            _ => Err("MODE must be config, warm, or cold".into()),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Config => "config",
            Self::Warm => "warm",
            Self::Cold => "cold",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FrozenProfileConfig {
    hydration_workers: usize,
    build_workers: usize,
    segment_docs: usize,
    max_file_bytes: u64,
    hydration_batch_bytes: u64,
    scanner_mode: ScannerMode,
    acceleration_profile: AccelerationProfile,
}

impl From<ProductionBuildConfig> for FrozenProfileConfig {
    fn from(config: ProductionBuildConfig) -> Self {
        Self {
            hydration_workers: config.hydration_workers,
            build_workers: config.build_workers,
            segment_docs: config.segment_docs,
            max_file_bytes: config.max_file_bytes,
            hydration_batch_bytes: config.hydration_batch_bytes,
            scanner_mode: config.scanner_mode,
            acceleration_profile: config.acceleration_profile,
        }
    }
}

impl FrozenProfileConfig {
    fn build_options(self) -> BuildOptions {
        BuildOptions {
            mode: BuildMode::Adaptive,
            segment_docs: self.segment_docs,
            workers: self.build_workers,
        }
    }

    fn json(self) -> Value {
        json!({
            "hydrationWorkers": self.hydration_workers,
            "buildWorkers": self.build_workers,
            "segmentDocs": self.segment_docs,
            "maxFileBytes": self.max_file_bytes,
            "hydrationBatchBytes": self.hydration_batch_bytes,
            "scannerMode": self.scanner_mode.as_str(),
            "accelerationProfile": acceleration_profile_name(self.acceleration_profile),
        })
    }
}

struct ScannedInputs {
    inputs: Vec<DiskPathInput>,
    discovered_entries: usize,
    discovered_file_entries: usize,
    discovered_directory_entries: usize,
    discovered_other_entries: usize,
    unselected_file_entries: usize,
    pruned_entries: usize,
    error_entries: usize,
    selected_bytes: u64,
    scan_wall: Duration,
    content_files: usize,
    content_bytes: u64,
}

struct RunBuildConfig<'a> {
    root: &'a Path,
    inputs: &'a [DiskPathInput],
    output: PathBuf,
    options: &'a BuildOptions,
    frozen: FrozenProfileConfig,
}

fn duration_ms(value: Duration) -> f64 {
    value.as_secs_f64() * 1000.0
}

fn acceleration_profile_name(profile: AccelerationProfile) -> &'static str {
    match profile {
        AccelerationProfile::Full => "full",
        AccelerationProfile::Balanced => "balanced",
        AccelerationProfile::AdaptiveDelta => "adaptive_delta",
        AccelerationProfile::None => "none",
    }
}

fn parse_acceleration_profile(value: &str) -> Result<AccelerationProfile, Box<dyn Error>> {
    match value.to_ascii_lowercase().as_str() {
        "full" => Ok(AccelerationProfile::Full),
        "balanced" => Ok(AccelerationProfile::Balanced),
        "adaptive_delta" => Ok(AccelerationProfile::AdaptiveDelta),
        "none" => Ok(AccelerationProfile::None),
        _ => Err("ACCELERATION_PROFILE must be full, balanced, adaptive_delta, or none".into()),
    }
}

fn parse_required<T>(args: &[String], index: usize, name: &str) -> Result<T, Box<dyn Error>>
where
    T: std::str::FromStr,
    T::Err: Error + 'static,
{
    args.get(index)
        .ok_or_else(|| format!("missing {name}"))?
        .parse::<T>()
        .map_err(Into::into)
}

fn parse_frozen_config(args: &[String]) -> Result<FrozenProfileConfig, Box<dyn Error>> {
    if args.len() != 10 {
        return Err(
            "warm/cold usage: MODE ROOT OUTPUT_ROOT HYDRATION_WORKERS BUILD_WORKERS SEGMENT_DOCS MAX_FILE_BYTES HYDRATION_BATCH_BYTES SCANNER_MODE ACCELERATION_PROFILE"
                .into(),
        );
    }
    let hydration_workers = parse_required(args, 3, "HYDRATION_WORKERS")?;
    let build_workers = parse_required(args, 4, "BUILD_WORKERS")?;
    let segment_docs = parse_required(args, 5, "SEGMENT_DOCS")?;
    let max_file_bytes = parse_required(args, 6, "MAX_FILE_BYTES")?;
    let hydration_batch_bytes = parse_required(args, 7, "HYDRATION_BATCH_BYTES")?;
    let scanner_mode = ScannerMode::parse(args.get(8).ok_or("missing SCANNER_MODE")?.as_str());
    let acceleration_profile =
        parse_acceleration_profile(args.get(9).ok_or("missing ACCELERATION_PROFILE")?.as_str())?;
    if hydration_workers == 0
        || build_workers == 0
        || segment_docs == 0
        || max_file_bytes == 0
        || hydration_batch_bytes == 0
    {
        return Err("all frozen numeric configuration values must be greater than zero".into());
    }
    Ok(FrozenProfileConfig {
        hydration_workers,
        build_workers,
        segment_docs,
        max_file_bytes,
        hydration_batch_bytes,
        scanner_mode,
        acceleration_profile,
    })
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

fn scan_inputs(root: &Path, frozen: FrozenProfileConfig) -> Result<ScannedInputs, Box<dyn Error>> {
    let scan_started = Instant::now();
    let scan = scan_files(
        root,
        frozen.max_file_bytes,
        frozen.scanner_mode,
        &ScanExclusions::default(),
        Arc::new(AtomicBool::new(false)),
        Arc::new(|_| {}),
    )
    .map_err(io::Error::other)?;
    let scan_wall = scan_started.elapsed();
    let progress = scan.progress;
    let mut files = scan.files;
    files.sort_by(|left, right| left.display_path.cmp(&right.display_path));
    let content_files = files.iter().filter(|file| file.index_content).count();
    let content_bytes = files
        .iter()
        .filter(|file| file.index_content)
        .map(|file| file.size_bytes)
        .sum::<u64>();
    let inputs = files
        .into_iter()
        .map(|file| DiskPathInput {
            path: file.path,
            display_path: file.display_path,
            size_bytes: file.size_bytes,
            content_path: None,
            index_content: file.index_content,
        })
        .collect();
    Ok(ScannedInputs {
        inputs,
        discovered_entries: progress.discovered_entries,
        discovered_file_entries: progress.file_entries,
        discovered_directory_entries: progress.directory_entries,
        discovered_other_entries: progress.other_entries,
        unselected_file_entries: progress.unselected_file_entries(),
        pruned_entries: progress.pruned_entries,
        error_entries: progress.error_entries,
        selected_bytes: progress.selected_bytes,
        scan_wall,
        content_files,
        content_bytes,
    })
}

fn configuration_json(
    mode: ProfileMode,
    root: &Path,
    output_root: &Path,
    frozen: FrozenProfileConfig,
    scan: &ScannedInputs,
) -> Value {
    json!({
        "schemaVersion": 2,
        "mode": mode.as_str(),
        "root": root,
        "outputRoot": output_root,
        "frozenBenchmarkConfig": frozen.json(),
        "hydrationWorkers": frozen.hydration_workers,
        "buildWorkers": frozen.build_workers,
        "segmentDocs": frozen.segment_docs,
        "maxFileBytes": frozen.max_file_bytes,
        "hydrationBatchBytes": frozen.hydration_batch_bytes,
        "scannerMode": frozen.scanner_mode.as_str(),
        "accelerationProfile": acceleration_profile_name(frozen.acceleration_profile),
        "scanWallMs": duration_ms(scan.scan_wall),
        "discoveredEntries": scan.discovered_entries,
        "discoveredFileEntries": scan.discovered_file_entries,
        "discoveredDirectoryEntries": scan.discovered_directory_entries,
        "discoveredOtherEntries": scan.discovered_other_entries,
        "unselectedFileEntries": scan.unselected_file_entries,
        "prunedEntries": scan.pruned_entries,
        "errorEntries": scan.error_entries,
        "selectedFiles": scan.inputs.len(),
        "selectedContentFiles": scan.content_files,
        "selectedBytes": scan.selected_bytes,
        "selectedContentBytes": scan.content_bytes,
        "scanExclusions": "default",
        "profileInstrumentation": {
            "compiled": cfg!(feature = "profile-build"),
            "enabled": env::var_os("PR_PROFILE_BUILD").is_some(),
            "combinedRead": true,
            "unavailableBreakdown": ["file_open", "allocation_copy", "worker_idle"],
        },
        "note": "BUILD_HYDRATION combined_read_ms includes the unchanged fs::read primitive rather than falsely separating open/read/allocation. BUILD_SEGMENT_WALL/BUILD_PHASE and *_work fields are profile-only; summed worker time can exceed end-to-end wall time."
    })
}

fn report_json(
    label: &str,
    report: &DiskPathBuildReport,
    call_wall: Duration,
    verify_wall: Duration,
    frozen: FrozenProfileConfig,
) -> Value {
    let timings = report.timings;
    json!({
        "schemaVersion": 2,
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
        "accelerationProfile": acceleration_profile_name(frozen.acceleration_profile),
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
            "segmentWritePrepareWorkMs": duration_ms(timings.segment_write_prepare_work),
            "segmentWriteOpenWorkMs": duration_ms(timings.segment_write_open_work),
            "segmentWriteBodyWorkMs": duration_ms(timings.segment_write_body_work),
            "segmentWriteMetadataWorkMs": duration_ms(timings.segment_write_metadata_work),
            "segmentWriteSyncWorkMs": duration_ms(timings.segment_write_sync_work),
            "segmentWriteFinalizeWorkMs": duration_ms(timings.segment_write_finalize_work),
            "accelerationWorkMs": duration_ms(timings.acceleration_work),
            "manifestWriteWallMs": duration_ms(timings.manifest_write_wall),
            "workerTimesAreSummed": true
        }
    })
}

fn run_build(label: &str, config: RunBuildConfig<'_>) -> Result<Value, Box<dyn Error>> {
    let RunBuildConfig {
        root,
        inputs,
        output,
        options,
        frozen,
    } = config;
    prepare_output(&output)?;
    println!(
        "PROFILE_RUN_BEGIN label={label} hydration_workers={} output={}",
        frozen.hydration_workers,
        output.display()
    );

    let started = Instant::now();
    let report = build_disk_path_inputs_index_unified(
        root,
        inputs.to_vec(),
        &output,
        DiskPathBuildConfig {
            max_docs: None,
            max_file_bytes: frozen.max_file_bytes,
            build: options,
            scan_workers: frozen.hydration_workers,
            hydration_batch_bytes: frozen.hydration_batch_bytes,
            cancel: None,
        },
        frozen.acceleration_profile,
        |_| {},
    )?;
    let call_wall = started.elapsed();

    let verify_started = Instant::now();
    verify_index(&output)?;
    let verify_wall = verify_started.elapsed();

    let payload = report_json(label, &report, call_wall, verify_wall, frozen);
    println!("PROFILE_RUN_JSON {}", serde_json::to_string(&payload)?);
    Ok(payload)
}

fn run() -> Result<(), Box<dyn Error>> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.len() < 3 {
        return Err(
            "usage: index_build_profile CONFIG|WARM|COLD ROOT OUTPUT_ROOT [CONFIG_ARGS]".into(),
        );
    }

    let mode = ProfileMode::parse(&args[0])?;
    let root = PathBuf::from(&args[1]);
    let output_root = PathBuf::from(&args[2]);
    if !root.is_dir() {
        return Err(format!("root is not a directory: {}", root.display()).into());
    }

    if mode == ProfileMode::Config {
        if args.len() > 5 {
            return Err(
                "config usage: CONFIG ROOT OUTPUT_ROOT [MAX_FILE_BYTES] [SCANNER_MODE]".into(),
            );
        }
        let max_file_bytes = args
            .get(3)
            .map(|value| value.parse::<u64>())
            .transpose()?
            .unwrap_or(DEFAULT_MAX_FILE_BYTES)
            .max(1);
        let scanner_mode = args
            .get(4)
            .map_or(ScannerMode::Auto, |value| ScannerMode::parse(value));
        let scan = scan_inputs(
            &root,
            FrozenProfileConfig {
                hydration_workers: 1,
                build_workers: 1,
                segment_docs: 1,
                max_file_bytes,
                hydration_batch_bytes: 1,
                scanner_mode,
                acceleration_profile: AccelerationProfile::Balanced,
            },
        )?;
        let frozen = FrozenProfileConfig::from(resolve_production_build_config(
            max_file_bytes,
            scanner_mode,
            scan.content_files,
            scan.content_bytes,
        ));
        let configuration = configuration_json(mode, &root, &output_root, frozen, &scan);
        println!(
            "PROFILE_CONFIG_JSON {}",
            serde_json::to_string(&configuration)?
        );
        return Ok(());
    }

    let frozen = parse_frozen_config(&args)?;
    if mode == ProfileMode::Cold {
        eprintln!(
            "COLD_RUN_ISOLATION_REQUIRED: run exactly one frozen configuration per verified cold-cache session."
        );
    }
    let scan = scan_inputs(&root, frozen)?;
    let configuration = configuration_json(mode, &root, &output_root, frozen, &scan);
    println!(
        "PROFILE_CONFIG_JSON {}",
        serde_json::to_string(&configuration)?
    );
    let options = frozen.build_options();

    let measured = match mode {
        ProfileMode::Config => unreachable!(),
        ProfileMode::Cold => run_build(
            "cold",
            RunBuildConfig {
                root: &root,
                inputs: &scan.inputs,
                output: output_root.join("cold"),
                options: &options,
                frozen,
            },
        )?,
        ProfileMode::Warm => {
            let _prime = run_build(
                "warm-prime",
                RunBuildConfig {
                    root: &root,
                    inputs: &scan.inputs,
                    output: output_root.join("warm-prime"),
                    options: &options,
                    frozen,
                },
            )?;
            run_build(
                "warm-measured",
                RunBuildConfig {
                    root: &root,
                    inputs: &scan.inputs,
                    output: output_root.join("warm-measured"),
                    options: &options,
                    frozen,
                },
            )?
        }
    };

    println!(
        "PROFILE_SUMMARY_JSON {}",
        serde_json::to_string(&json!({
            "schemaVersion": 2,
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
