#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    fs,
    io::{BufReader, BufWriter},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering},
        Arc, Mutex, RwLock,
    },
    thread,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use personalrag_gui_bridge_core::{
    BackendReadiness, BackgroundRequest, BackgroundStatus, ContractInfo, DirectoryTrackingSnapshot,
    IncrementalCatalogState, IncrementalChangeSyncRequest, IncrementalSyncRequest,
    IncrementalSyncResult, IndexBuildDiagnosticLog, IndexBuildPhase, IndexEngine, IndexRequest,
    IndexResponse, PortableEngine, ProductionBackendMode, ProgressRateTracker, RebuildProgress,
    RebuildStatus, SearchBackendStatus, SearchCatalogView, SearchCoreBackendStatus, SearchEngine,
    SearchHit, SearchRequest, Settings, SnippetBatchRequest, SnippetBatchResult, SnippetHit,
    SnippetRequest, UsnCheckpoint, UsnScanResult, INGESTION_VERSION,
};
use serde::{Deserialize, Serialize};
use tauri::{Manager, State};

const DEFAULT_MAX_BYTES: u64 = 32 * 1024 * 1024;
const CATALOG_FILE: &str = "gui-catalog.json";
const CHANGE_TRACKER_FILE: &str = "change-tracker-v1.json";
const CHANGE_TRACKER_VERSION: u32 = 1;
const INDEX_PIPELINE_VERSION: u32 = 2;
const DIAGNOSTICS_DIR: &str = "diagnostics";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Catalog {
    root: PathBuf,
    paths: Vec<String>,
    #[serde(default)]
    logical_ids: Vec<u64>,
    #[serde(default)]
    generation: u64,
    #[serde(default = "default_next_logical_id")]
    next_logical_id: u64,
    #[serde(default)]
    ingestion_version: u32,
    #[serde(default)]
    pipeline_version: u32,
    #[serde(default)]
    scope_signature: String,
    #[serde(default)]
    size_bytes: Vec<u64>,
    #[serde(default)]
    modified_ns: Vec<u64>,
    #[serde(skip)]
    logical_to_row: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChangeTrackerStateFile {
    version: u32,
    root: PathBuf,
    scope_signature: String,
    generation: u64,
    checkpoint: UsnCheckpoint,
    directories: DirectoryTrackingSnapshot,
}

const fn default_next_logical_id() -> u64 {
    1
}

impl Catalog {
    fn rebuild_logical_inverse(&mut self) -> Result<(), String> {
        self.logical_to_row.clear();
        if self.logical_ids.is_empty() {
            return Ok(());
        }
        if self.logical_ids.len() != self.paths.len() {
            return Err("GUI logical catalog is not aligned with paths".to_owned());
        }
        let max_id = self.logical_ids.iter().copied().max().unwrap_or(0);
        let len = usize::try_from(max_id.saturating_add(1))
            .map_err(|_| "logical ID address space overflow".to_owned())?;
        self.logical_to_row = vec![u32::MAX; len];
        for (row, &logical_id) in self.logical_ids.iter().enumerate() {
            if logical_id == 0 {
                return Err("logical ID zero is reserved".to_owned());
            }
            let slot = self
                .logical_to_row
                .get_mut(logical_id as usize)
                .ok_or_else(|| "logical ID inverse map overflow".to_owned())?;
            if *slot != u32::MAX {
                return Err("duplicate logical ID in GUI catalog".to_owned());
            }
            *slot = u32::try_from(row).map_err(|_| "GUI catalog row overflow".to_owned())?;
        }
        Ok(())
    }

    fn incremental_state(&self) -> IncrementalCatalogState {
        IncrementalCatalogState {
            generation: self.generation,
            next_logical_id: self.next_logical_id,
            paths: self.paths.clone(),
            logical_ids: self.logical_ids.clone(),
            size_bytes: self.size_bytes.clone(),
            modified_ns: self.modified_ns.clone(),
        }
    }
}

struct AppState {
    app_data_dir: PathBuf,
    index_dir: PathBuf,
    settings_path: PathBuf,
    settings: Mutex<Settings>,
    rebuild: Arc<Mutex<Option<RebuildStatus>>>,
    cancel_rebuild: Arc<AtomicBool>,
    search_epoch: Arc<AtomicU64>,
    index_access: Arc<RwLock<()>>,
    catalog: Arc<Mutex<Option<Arc<Catalog>>>>,
    engine: Arc<PortableEngine>,
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn now_millis_u64() -> u64 {
    u64::try_from(now_millis()).unwrap_or(u64::MAX)
}

fn persist_settings(path: &Path, settings: &Settings) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let bytes = serde_json::to_vec_pretty(settings).map_err(|error| error.to_string())?;
    fs::write(path, bytes).map_err(|error| error.to_string())
}

fn load_settings_file(path: &Path) -> Settings {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Settings>(&bytes).ok())
        .unwrap_or_default()
}

fn save_catalog(index_dir: &Path, catalog: &Catalog) -> Result<(), String> {
    let file = fs::File::create(index_dir.join(CATALOG_FILE)).map_err(|error| error.to_string())?;
    serde_json::to_writer(BufWriter::new(file), catalog).map_err(|error| error.to_string())
}

fn load_catalog_file(index_dir: &Path) -> Result<Catalog, String> {
    let file = fs::File::open(index_dir.join(CATALOG_FILE)).map_err(|_| {
        "GUI用catalogがありません。GUIの『再index』で一度indexを作成してください".to_owned()
    })?;
    let mut catalog: Catalog =
        serde_json::from_reader(BufReader::new(file)).map_err(|error| error.to_string())?;
    catalog.rebuild_logical_inverse()?;
    Ok(catalog)
}

fn save_change_tracker(index_dir: &Path, tracker: &ChangeTrackerStateFile) -> Result<(), String> {
    let file =
        fs::File::create(index_dir.join(CHANGE_TRACKER_FILE)).map_err(|error| error.to_string())?;
    serde_json::to_writer(BufWriter::new(file), tracker).map_err(|error| error.to_string())
}

fn load_change_tracker(index_dir: &Path) -> Result<ChangeTrackerStateFile, String> {
    let file =
        fs::File::open(index_dir.join(CHANGE_TRACKER_FILE)).map_err(|error| error.to_string())?;
    serde_json::from_reader(BufReader::new(file)).map_err(|error| error.to_string())
}

fn remove_change_tracker(index_dir: &Path) {
    let _ = fs::remove_file(index_dir.join(CHANGE_TRACKER_FILE));
}

fn change_tracker_eligible(
    tracker: &ChangeTrackerStateFile,
    catalog: &Catalog,
    root: &Path,
    settings: &Settings,
) -> bool {
    tracker.version == CHANGE_TRACKER_VERSION
        && tracker.root == root
        && tracker.generation == catalog.generation
        && tracker.directories.complete
        && scope_signature(root, settings)
            .is_ok_and(|signature| signature == tracker.scope_signature)
}

fn change_tracker_from_parts(
    root: &Path,
    settings: &Settings,
    generation: u64,
    checkpoint: UsnCheckpoint,
    directories: DirectoryTrackingSnapshot,
) -> Result<ChangeTrackerStateFile, String> {
    Ok(ChangeTrackerStateFile {
        version: CHANGE_TRACKER_VERSION,
        root: root.to_path_buf(),
        scope_signature: scope_signature(root, settings)?,
        generation,
        checkpoint,
        directories,
    })
}

fn scope_signature(root: &Path, settings: &Settings) -> Result<String, String> {
    serde_json::to_string(&(
        root,
        settings.max_bytes,
        settings.scanner_mode.as_str(),
        &settings.exclusions,
    ))
    .map_err(|error| error.to_string())
}

fn incremental_eligible(
    index_dir: &Path,
    catalog: &Catalog,
    root: &Path,
    settings: &Settings,
) -> bool {
    index_dir.join("CURRENT").exists()
        && (settings.search_core_backend == "perf12"
            || index_dir.join("vnext-store").join("CURRENT").exists())
        && catalog.root == root
        && catalog.ingestion_version == INGESTION_VERSION
        && catalog.pipeline_version == INDEX_PIPELINE_VERSION
        && catalog.logical_ids.len() == catalog.paths.len()
        && catalog.size_bytes.len() == catalog.paths.len()
        && catalog.modified_ns.len() == catalog.paths.len()
        && scope_signature(root, settings)
            .is_ok_and(|signature| signature == catalog.scope_signature)
}

fn catalog_from_build(
    root: &Path,
    settings: &Settings,
    build: personalrag_gui_bridge_core::IndexBuildOutcome,
) -> Result<Catalog, String> {
    let mut catalog = Catalog {
        root: root.to_path_buf(),
        paths: build.paths,
        logical_ids: build.logical_ids,
        generation: build.generation,
        next_logical_id: build.next_logical_id,
        ingestion_version: INGESTION_VERSION,
        pipeline_version: INDEX_PIPELINE_VERSION,
        scope_signature: scope_signature(root, settings)?,
        size_bytes: build.size_bytes,
        modified_ns: build.modified_ns,
        logical_to_row: Vec::new(),
    };
    catalog.rebuild_logical_inverse()?;
    Ok(catalog)
}

fn cached_catalog(state: &AppState) -> Result<Arc<Catalog>, String> {
    if let Some(catalog) = state
        .catalog
        .lock()
        .map_err(|_| "catalog lock poisoned".to_owned())?
        .as_ref()
        .cloned()
    {
        return Ok(catalog);
    }
    let catalog = Arc::new(load_catalog_file(&state.index_dir)?);
    *state
        .catalog
        .lock()
        .map_err(|_| "catalog lock poisoned".to_owned())? = Some(Arc::clone(&catalog));
    Ok(catalog)
}

fn update_rebuild<F>(rebuild: &Arc<Mutex<Option<RebuildStatus>>>, mut update: F)
where
    F: FnMut(&mut RebuildStatus),
{
    if let Ok(mut guard) = rebuild.lock() {
        if let Some(status) = guard.as_mut() {
            update(status);
        }
    }
}

fn background_status_value(state: &AppState) -> Result<BackgroundStatus, String> {
    let enabled = state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_owned())?
        .background_enabled;
    let rebuild = state
        .rebuild
        .lock()
        .map_err(|_| "rebuild lock poisoned".to_owned())?
        .clone();
    let active = rebuild.as_ref().is_some_and(|status| {
        matches!(
            status.state.as_str(),
            "starting" | "scanning" | "reconciling" | "catching_up" | "cancelling"
        )
    });
    let failed = rebuild.as_ref().and_then(|status| {
        (status.state == "failed")
            .then(|| status.error.clone())
            .flatten()
    });
    Ok(BackgroundStatus {
        running: enabled,
        mode: "portable-core".to_owned(),
        sync_state: if failed.is_some() {
            "error".to_owned()
        } else if active {
            "rebuilding".to_owned()
        } else if enabled {
            "up_to_date".to_owned()
        } else {
            "stopped".to_owned()
        },
        pending_changes: 0,
        last_sync_at: rebuild
            .as_ref()
            .and_then(|status| status.finished_at.clone()),
        last_error: failed,
        rebuild,
    })
}

fn publish_index(build_dir: &Path, index_dir: &Path) -> Result<(), String> {
    let backup = index_dir.with_extension("previous");
    if backup.exists() {
        fs::remove_dir_all(&backup).map_err(|error| error.to_string())?;
    }
    if index_dir.exists() {
        fs::rename(index_dir, &backup).map_err(|error| error.to_string())?;
    }
    match fs::rename(build_dir, index_dir) {
        Ok(()) => {
            if backup.exists() {
                let _ = fs::remove_dir_all(backup);
            }
            Ok(())
        }
        Err(error) => {
            if backup.exists() && !index_dir.exists() {
                let _ = fs::rename(&backup, index_dir);
            }
            Err(error.to_string())
        }
    }
}

fn start_rebuild(
    state: &AppState,
    settings: Settings,
    force_full: bool,
) -> Result<(String, String), String> {
    if settings.roots.len() != 1 {
        return Err("Portable GUI bridgeでは対象rootを1つ指定してください".to_owned());
    }
    let root = settings.roots[0].clone();
    if !root.is_dir() {
        return Err(format!("rootが存在しません: {}", root.display()));
    }
    {
        let guard = state
            .rebuild
            .lock()
            .map_err(|_| "rebuild lock poisoned".to_owned())?;
        if guard.as_ref().is_some_and(|status| {
            matches!(
                status.state.as_str(),
                "starting" | "scanning" | "reconciling" | "catching_up" | "cancelling"
            )
        }) {
            return Err("index作成はすでに実行中です".to_owned());
        }
    }

    let started_at_ms = now_millis_u64();
    let job_id = format!("portable-{started_at_ms}");
    let started_at = started_at_ms.to_string();
    *state
        .rebuild
        .lock()
        .map_err(|_| "rebuild lock poisoned".to_owned())? = Some(RebuildStatus {
        job_id: job_id.clone(),
        state: "starting".to_owned(),
        progress: RebuildProgress {
            total_files: 0,
            discovered_files: 0,
            remaining_files: None,
            ..RebuildProgress::default()
        },
        started_at: Some(started_at),
        finished_at: None,
        error: None,
    });
    state.cancel_rebuild.store(false, AtomicOrdering::Release);

    let app_data_dir = state.app_data_dir.clone();
    let index_dir = state.index_dir.clone();
    let rebuild = Arc::clone(&state.rebuild);
    let cancel = Arc::clone(&state.cancel_rebuild);
    let index_access = Arc::clone(&state.index_access);
    let catalog_cache = Arc::clone(&state.catalog);
    let engine = Arc::clone(&state.engine);
    let build_job_id = job_id.clone();
    thread::spawn(move || {
        let started = Instant::now();
        let diagnostics_dir = app_data_dir.join(DIAGNOSTICS_DIR);
        let mut diagnostics = IndexBuildDiagnosticLog::new(
            build_job_id.clone(),
            &root,
            force_full,
            settings.scanner_mode.clone(),
            settings.search_core_backend.clone(),
            settings.max_bytes,
            started_at_ms,
        );
        let build_dir = app_data_dir.join(format!("portable-index-build-{build_job_id}"));
        let result = (|| -> Result<Catalog, String> {
            if build_dir.exists() {
                fs::remove_dir_all(&build_dir).map_err(|error| error.to_string())?;
            }

            if !force_full {
                if let (Ok(previous_catalog), Ok(mut tracker)) = (
                    load_catalog_file(&index_dir),
                    load_change_tracker(&index_dir),
                ) {
                    if incremental_eligible(&index_dir, &previous_catalog, &root, &settings)
                        && change_tracker_eligible(&tracker, &previous_catalog, &root, &settings)
                    {
                        update_rebuild(&rebuild, |status| {
                            status.state = "reconciling".to_owned();
                            status.progress.phase = "journal".to_owned();
                            status.progress.total_files = previous_catalog.paths.len();
                            status.progress.processed_files = 0;
                            status.progress.unchanged_files = previous_catalog.paths.len();
                            status.progress.current_path = Some("NTFS USN Journal".to_owned());
                        });
                        let journal_scan_started = Instant::now();
                        let journal_scan = engine.scan_changes(
                            &root,
                            tracker.checkpoint,
                            &tracker.directories,
                            settings.max_bytes,
                            &settings.exclusions,
                        );
                        diagnostics.record_stage(
                            "journal.scan_changes",
                            journal_scan_started.elapsed().as_secs_f64() * 1_000.0,
                        );
                        match journal_scan? {
                            UsnScanResult::NoChanges { checkpoint } => {
                                diagnostics.mode = "incremental_usn_no_changes".to_owned();
                                diagnostics.source_files = previous_catalog.paths.len();
                                diagnostics.indexed_files = previous_catalog.paths.len();
                                tracker.checkpoint = checkpoint;
                                let tracker_save_started = Instant::now();
                                save_change_tracker(&index_dir, &tracker)?;
                                diagnostics.record_stage(
                                    "journal.save_change_tracker",
                                    tracker_save_started.elapsed().as_secs_f64() * 1_000.0,
                                );
                                *catalog_cache
                                    .lock()
                                    .map_err(|_| "catalog lock poisoned".to_owned())? =
                                    Some(Arc::new(previous_catalog.clone()));
                                update_rebuild(&rebuild, |status| {
                                    status.progress.processed_files = 0;
                                    status.progress.unchanged_files = previous_catalog.paths.len();
                                    status.progress.remaining_files = Some(0);
                                    status.progress.queue_files = 0;
                                    status.progress.phase = "verifying".to_owned();
                                    status.progress.current_path = None;
                                });
                                return Ok(previous_catalog);
                            }
                            UsnScanResult::Changes(changes) => {
                                let next_checkpoint = changes.checkpoint;
                                let next_directories = changes.directories.clone();
                                let journal_records = changes.journal_records;
                                let write_guard = index_access
                                    .write()
                                    .map_err(|_| "index access lock poisoned".to_owned())?;
                                let fast_rebuild = Arc::clone(&rebuild);
                                let fast_started = started;
                                let previous_total = previous_catalog.paths.len();
                                let mut on_fast_progress = move |progress: personalrag_gui_bridge_core::IndexBuildProgress| {
                                    let elapsed = fast_started.elapsed().as_secs_f64() * 1_000.0;
                                    update_rebuild(&fast_rebuild, |status| {
                                        status.state = if progress.phase == Some(IndexBuildPhase::Verifying) {
                                            "catching_up".to_owned()
                                        } else {
                                            "reconciling".to_owned()
                                        };
                                        status.progress.phase = progress.phase.map_or_else(
                                            || "journal".to_owned(),
                                            |phase| phase.as_str().to_owned(),
                                        );
                                        status.progress.total_files = previous_total;
                                        status.progress.processed_files = progress.processed_files;
                                        status.progress.indexed_files = progress.indexed_files;
                                        status.progress.bytes_read = progress.bytes_read;
                                        status.progress.current_path = progress.current_path.as_ref().map(|path| path.to_string_lossy().into_owned());
                                        status.progress.elapsed_ms = elapsed;
                                    });
                                };
                                let journal_sync_started = Instant::now();
                                let journal_sync = engine.sync_incremental_changes(
                                    IncrementalChangeSyncRequest {
                                        root: &root,
                                        upserts: &changes.upserts,
                                        deleted_paths: &changes.deleted_paths,
                                        index_dir: &index_dir,
                                        previous: previous_catalog.incremental_state(),
                                        max_file_bytes: settings.max_bytes,
                                    },
                                    cancel.as_ref(),
                                    &mut on_fast_progress,
                                );
                                diagnostics.record_stage(
                                    "journal.incremental_sync",
                                    journal_sync_started.elapsed().as_secs_f64() * 1_000.0,
                                );
                                match journal_sync? {
                                    IncrementalSyncResult::Applied(build)
                                    | IncrementalSyncResult::Unchanged(build) => {
                                        diagnostics.mode = "incremental_usn".to_owned();
                                        diagnostics.source_files = build.source_files;
                                        diagnostics.processed_files = build.processed_files;
                                        diagnostics.indexed_files = build.indexed_files;
                                        diagnostics.skipped_files = build.skipped_files;
                                        diagnostics.bytes_read = build.bytes_read;
                                        let processed = build.processed_files;
                                        let source_files = build.source_files;
                                        let catalog = catalog_from_build(&root, &settings, build)?;
                                        let catalog_save_started = Instant::now();
                                        save_catalog(&index_dir, &catalog)?;
                                        diagnostics.record_stage(
                                            "journal.save_catalog",
                                            catalog_save_started.elapsed().as_secs_f64() * 1_000.0,
                                        );
                                        let next_tracker = change_tracker_from_parts(
                                            &root,
                                            &settings,
                                            catalog.generation,
                                            next_checkpoint,
                                            next_directories,
                                        )?;
                                        let tracker_save_started = Instant::now();
                                        save_change_tracker(&index_dir, &next_tracker)?;
                                        diagnostics.record_stage(
                                            "journal.save_change_tracker",
                                            tracker_save_started.elapsed().as_secs_f64() * 1_000.0,
                                        );
                                        *catalog_cache
                                            .lock()
                                            .map_err(|_| "catalog lock poisoned".to_owned())? =
                                            Some(Arc::new(catalog.clone()));
                                        update_rebuild(&rebuild, |status| {
                                            status.progress.total_files = source_files;
                                            status.progress.processed_files = processed;
                                            status.progress.unchanged_files =
                                                source_files.saturating_sub(processed);
                                            status.progress.discovered_files = journal_records;
                                            status.progress.remaining_files = Some(0);
                                            status.progress.queue_files = 0;
                                            status.progress.phase = "verifying".to_owned();
                                            status.progress.current_path = None;
                                        });
                                        drop(write_guard);
                                        return Ok(catalog);
                                    }
                                    IncrementalSyncResult::FullRebuildRequired {
                                        changed_files,
                                    } => {
                                        update_rebuild(&rebuild, |status| {
                                            status.progress.phase = "full_rebuild".to_owned();
                                            status.progress.current_path = Some(format!(
                                                "USN差分 {changed_files} 件のためフル再構築へ切り替え"
                                            ));
                                        });
                                        drop(write_guard);
                                    }
                                }
                            }
                            UsnScanResult::FullScanRequired { reason }
                            | UsnScanResult::Unsupported { reason } => {
                                update_rebuild(&rebuild, |status| {
                                    status.progress.phase = "full_scan_fallback".to_owned();
                                    status.progress.current_path = Some(reason.clone());
                                });
                            }
                        }
                    }
                }
            }

            let checkpoint_started = Instant::now();
            let scan_checkpoint = engine.capture_change_checkpoint(&root).ok().flatten();
            diagnostics.record_stage(
                "scan.capture_checkpoint",
                checkpoint_started.elapsed().as_secs_f64() * 1_000.0,
            );
            update_rebuild(&rebuild, |status| {
                status.state = "scanning".to_owned();
                status.progress.phase = "scanning".to_owned();
                status.progress.current_path = None;
            });

            let scan_rebuild = Arc::clone(&rebuild);
            let scan_started = started;
            let scan_rate_started = Instant::now();
            let scan_rates = Arc::new(Mutex::new(ProgressRateTracker::new()));
            let scan_rates_for_progress = Arc::clone(&scan_rates);
            let scan_progress =
                Arc::new(move |progress: personalrag_gui_bridge_core::ScanProgress| {
                    let elapsed = scan_started.elapsed().as_secs_f64() * 1_000.0;
                    let rate_elapsed = scan_rate_started.elapsed().as_secs_f64() * 1_000.0;
                    let rate = scan_rates_for_progress
                        .lock()
                        .map(|mut tracker| {
                            tracker.update(rate_elapsed, progress.discovered_entries, 0, None)
                        })
                        .unwrap_or_default();
                    update_rebuild(&scan_rebuild, |status| {
                        status.state = "scanning".to_owned();
                        status.progress.phase = "scanning".to_owned();
                        status.progress.total_files = progress.selected_files;
                        status.progress.processed_files = 0;
                        status.progress.indexed_files = 0;
                        status.progress.discovered_files = progress.discovered_entries;
                        status.progress.pruned_files = progress.pruned_entries;
                        status.progress.error_files = progress.error_entries;
                        status.progress.current_path = progress
                            .current_path
                            .as_ref()
                            .map(|path| path.to_string_lossy().into_owned());
                        status.progress.elapsed_ms = elapsed;
                        status.progress.files_per_second = rate.files_per_second;
                        status.progress.mib_per_second = 0.0;
                        status.progress.remaining_files = None;
                        status.progress.eta_ms = None;
                        status.progress.estimated_completion_at_ms = None;
                        status.progress.queue_files = progress.selected_files;
                        status.progress.prepared_bytes = 0;
                    });
                });
            let full_scan_started = Instant::now();
            let scan_result = engine.scan(
                &root,
                settings.max_bytes,
                &settings.scanner_mode,
                &settings.exclusions,
                Arc::clone(&cancel),
                scan_progress,
            );
            diagnostics.record_stage(
                "scan.full",
                full_scan_started.elapsed().as_secs_f64() * 1_000.0,
            );
            let scan = scan_result?;
            diagnostics.discovered_entries = scan.progress.discovered_entries;
            diagnostics.discovered_file_entries = scan.progress.file_entries;
            diagnostics.discovered_directory_entries = scan.progress.directory_entries;
            diagnostics.discovered_other_entries = scan.progress.other_entries;
            diagnostics.unselected_file_entries = scan.progress.unselected_file_entries();
            diagnostics.source_files = scan.files.len();
            diagnostics.pruned_files = scan.progress.pruned_entries;
            diagnostics.error_files = scan.progress.error_entries;
            if cancel.load(AtomicOrdering::Acquire) {
                return Err("cancelled".to_owned());
            }

            let scan_directory_tracking = scan.directory_tracking.clone();
            let discovered_files = scan.progress.discovered_entries;
            let pruned_files = scan.progress.pruned_entries;
            let scan_errors = scan.progress.error_entries;
            let build_rebuild = Arc::clone(&rebuild);
            let build_started = started;
            let build_rate_started = Instant::now();
            let mut build_rates = ProgressRateTracker::new();
            let mut on_build_progress =
                move |progress: personalrag_gui_bridge_core::IndexBuildProgress| {
                    let elapsed = build_started.elapsed().as_secs_f64() * 1_000.0;
                    let phase = progress.phase.unwrap_or(IndexBuildPhase::Building);
                    let remaining = progress
                        .source_files
                        .saturating_sub(progress.processed_files);
                    let rate_elapsed = build_rate_started.elapsed().as_secs_f64() * 1_000.0;
                    let rate = build_rates.update(
                        rate_elapsed,
                        progress.processed_files,
                        progress.bytes_read,
                        (phase == IndexBuildPhase::Building).then_some(remaining),
                    );
                    update_rebuild(&build_rebuild, |status| {
                        status.state = if phase == IndexBuildPhase::Verifying {
                            "catching_up".to_owned()
                        } else {
                            "reconciling".to_owned()
                        };
                        status.progress.phase = phase.as_str().to_owned();
                        status.progress.total_files = progress.source_files;
                        status.progress.processed_files = progress.processed_files;
                        status.progress.indexed_files = progress.indexed_files;
                        status.progress.skipped_files = progress.skipped_files;
                        status.progress.bytes_read = progress.bytes_read;
                        status.progress.discovered_files = discovered_files;
                        status.progress.pruned_files = pruned_files;
                        status.progress.error_files = scan_errors;
                        status.progress.current_path = progress
                            .current_path
                            .as_ref()
                            .map(|path| path.to_string_lossy().into_owned());
                        status.progress.elapsed_ms = elapsed;
                        status.progress.remaining_files = Some(remaining);
                        status.progress.queue_files = remaining;
                        status.progress.files_per_second = rate.files_per_second;
                        status.progress.mib_per_second = rate.mib_per_second;
                        status.progress.eta_ms = rate.eta_ms;
                        status.progress.estimated_completion_at_ms =
                            rate.eta_ms.map(|eta_ms| now_millis() as f64 + eta_ms);
                        status.progress.prepared_bytes = progress.prepared_bytes;
                    });
                };
            if !force_full {
                if let Ok(previous_catalog) = load_catalog_file(&index_dir) {
                    if incremental_eligible(&index_dir, &previous_catalog, &root, &settings) {
                        let previous = previous_catalog.incremental_state();
                        let write_guard = index_access
                            .write()
                            .map_err(|_| "index access lock poisoned".to_owned())?;
                        let incremental_sync_started = Instant::now();
                        let incremental_sync = engine.sync_incremental(
                            IncrementalSyncRequest {
                                root: &root,
                                files: &scan.files,
                                index_dir: &index_dir,
                                previous,
                                max_file_bytes: settings.max_bytes,
                            },
                            cancel.as_ref(),
                            &mut on_build_progress,
                        );
                        diagnostics.record_stage(
                            "incremental.sync_after_scan",
                            incremental_sync_started.elapsed().as_secs_f64() * 1_000.0,
                        );
                        match incremental_sync? {
                            IncrementalSyncResult::Applied(build)
                            | IncrementalSyncResult::Unchanged(build) => {
                                diagnostics.mode = "incremental_full_scan".to_owned();
                                diagnostics.source_files = build.source_files;
                                diagnostics.processed_files = build.processed_files;
                                diagnostics.indexed_files = build.indexed_files;
                                diagnostics.skipped_files = build.skipped_files;
                                diagnostics.bytes_read = build.bytes_read;
                                if cancel.load(AtomicOrdering::Acquire) {
                                    return Err("cancelled".to_owned());
                                }
                                let unchanged =
                                    build.source_files.saturating_sub(build.processed_files);
                                update_rebuild(&rebuild, |status| {
                                    status.progress.total_files = build.source_files;
                                    status.progress.processed_files = build.processed_files;
                                    status.progress.indexed_files = build.indexed_files;
                                    status.progress.unchanged_files = unchanged;
                                    status.progress.skipped_files = build.skipped_files;
                                    status.progress.bytes_read = build.bytes_read;
                                    status.progress.remaining_files = Some(0);
                                    status.progress.queue_files = 0;
                                    status.progress.phase = "verifying".to_owned();
                                    status.progress.current_path = None;
                                });
                                let catalog = catalog_from_build(&root, &settings, build)?;
                                let catalog_save_started = Instant::now();
                                save_catalog(&index_dir, &catalog)?;
                                diagnostics.record_stage(
                                    "incremental.save_catalog",
                                    catalog_save_started.elapsed().as_secs_f64() * 1_000.0,
                                );
                                if let (Some(checkpoint), Some(directories)) =
                                    (scan_checkpoint, scan_directory_tracking.clone())
                                {
                                    let tracker = change_tracker_from_parts(
                                        &root,
                                        &settings,
                                        catalog.generation,
                                        checkpoint,
                                        directories,
                                    )?;
                                    let tracker_save_started = Instant::now();
                                    save_change_tracker(&index_dir, &tracker)?;
                                    diagnostics.record_stage(
                                        "incremental.save_change_tracker",
                                        tracker_save_started.elapsed().as_secs_f64() * 1_000.0,
                                    );
                                } else {
                                    remove_change_tracker(&index_dir);
                                }
                                *catalog_cache
                                    .lock()
                                    .map_err(|_| "catalog lock poisoned".to_owned())? =
                                    Some(Arc::new(catalog.clone()));
                                drop(write_guard);
                                return Ok(catalog);
                            }
                            IncrementalSyncResult::FullRebuildRequired { changed_files } => {
                                update_rebuild(&rebuild, |status| {
                                    status.progress.phase = "full_rebuild".to_owned();
                                    status.progress.current_path = Some(format!(
                                        "差分 {changed_files} 件のためフル再構築へ切り替え"
                                    ));
                                });
                                drop(write_guard);
                            }
                        }
                    }
                }
            }

            diagnostics.mode = "full_rebuild".to_owned();
            let engine_build_started = Instant::now();
            let build_result = engine.build(
                &root,
                scan.files,
                &build_dir,
                settings.max_bytes,
                cancel.as_ref(),
                &mut on_build_progress,
            );
            let engine_build_ms = engine_build_started.elapsed().as_secs_f64() * 1_000.0;
            let build = match build_result {
                Ok(build) => build,
                Err(error) => {
                    diagnostics.record_stage("build.failed_wall", engine_build_ms);
                    return Err(error);
                }
            };
            diagnostics.extend_stages(build.stage_timings.clone());
            diagnostics.source_files = build.source_files;
            diagnostics.processed_files = build.processed_files;
            diagnostics.indexed_files = build.indexed_files;
            diagnostics.skipped_files = build.skipped_files;
            diagnostics.bytes_read = build.bytes_read;
            if cancel.load(AtomicOrdering::Acquire) {
                return Err("cancelled".to_owned());
            }

            let source_files = build.source_files;
            let processed_files = build.processed_files;
            let indexed_files = build.indexed_files;
            let skipped_files = build.skipped_files;
            let bytes_read = build.bytes_read;
            let catalog = catalog_from_build(&root, &settings, build)?;
            update_rebuild(&rebuild, |status| {
                status.progress.total_files = source_files;
                status.progress.processed_files = processed_files;
                status.progress.indexed_files = indexed_files;
                status.progress.unchanged_files = 0;
                status.progress.skipped_files = skipped_files;
                status.progress.bytes_read = bytes_read;
                status.progress.remaining_files = Some(0);
                status.progress.queue_files = 0;
                status.progress.phase = "verifying".to_owned();
                status.progress.current_path = None;
            });
            let catalog_save_started = Instant::now();
            save_catalog(&build_dir, &catalog)?;
            diagnostics.record_stage(
                "build.save_catalog",
                catalog_save_started.elapsed().as_secs_f64() * 1_000.0,
            );
            if let (Some(checkpoint), Some(directories)) =
                (scan_checkpoint, scan_directory_tracking)
            {
                let tracker = change_tracker_from_parts(
                    &root,
                    &settings,
                    catalog.generation,
                    checkpoint,
                    directories,
                )?;
                let tracker_save_started = Instant::now();
                save_change_tracker(&build_dir, &tracker)?;
                diagnostics.record_stage(
                    "build.save_change_tracker",
                    tracker_save_started.elapsed().as_secs_f64() * 1_000.0,
                );
            }

            update_rebuild(&rebuild, |status| {
                status.progress.phase = "publishing".to_owned();
                status.progress.current_path = None;
            });
            let publish_started = Instant::now();
            let _write_guard = index_access
                .write()
                .map_err(|_| "index access lock poisoned".to_owned())?;
            engine.invalidate_search_cache()?;
            publish_index(&build_dir, &index_dir)?;
            diagnostics.record_stage(
                "build.publish_index",
                publish_started.elapsed().as_secs_f64() * 1_000.0,
            );
            *catalog_cache
                .lock()
                .map_err(|_| "catalog lock poisoned".to_owned())? = Some(Arc::new(catalog.clone()));
            Ok(catalog)
        })();

        let elapsed = started.elapsed().as_secs_f64() * 1_000.0;
        let finished_at_ms = now_millis_u64();
        match result {
            Ok(_catalog) => {
                diagnostics.finish("completed", finished_at_ms, elapsed, None);
                update_rebuild(&rebuild, |status| {
                    status.state = "completed".to_owned();
                    status.progress.phase = "completed".to_owned();
                    status.progress.elapsed_ms = elapsed;
                    status.progress.processed_files = status.progress.total_files;
                    status.progress.remaining_files = Some(0);
                    status.progress.queue_files = 0;
                    status.progress.eta_ms = Some(0.0);
                    status.progress.estimated_completion_at_ms = Some(finished_at_ms as f64);
                    status.progress.current_path = None;
                    status.finished_at = Some(finished_at_ms.to_string());
                    status.error = None;
                });
            }
            Err(error) if error == "cancelled" => {
                diagnostics.finish("cancelled", finished_at_ms, elapsed, None);
                let _ = fs::remove_dir_all(&build_dir);
                update_rebuild(&rebuild, |status| {
                    status.state = "cancelled".to_owned();
                    status.progress.elapsed_ms = elapsed;
                    status.progress.current_path = None;
                    status.finished_at = Some(finished_at_ms.to_string());
                    status.error = None;
                });
            }
            Err(error) => {
                diagnostics.finish("failed", finished_at_ms, elapsed, Some(error.clone()));
                let _ = fs::remove_dir_all(&build_dir);
                update_rebuild(&rebuild, |status| {
                    status.state = "failed".to_owned();
                    status.progress.elapsed_ms = elapsed;
                    status.progress.current_path = None;
                    status.finished_at = Some(finished_at_ms.to_string());
                    status.error = Some(error.clone());
                });
            }
        }
        match diagnostics.write_json(&diagnostics_dir) {
            Ok(path) => eprintln!("INDEX_BUILD_DIAGNOSTIC path={}", path.display()),
            Err(error) => eprintln!("INDEX_BUILD_DIAGNOSTIC_WRITE_FAILED error={error}"),
        }
    });

    Ok((
        job_id,
        "Portable Search Coreでindex作成を開始しました".to_owned(),
    ))
}

#[tauri::command]
fn search(request: SearchRequest, state: State<'_, AppState>) -> Result<Vec<SearchHit>, String> {
    let _read_guard = state
        .index_access
        .read()
        .map_err(|_| "index access lock poisoned".to_owned())?;
    if !state.index_dir.join("manifest.txt").exists() && !state.index_dir.join("CURRENT").exists() {
        return Err(
            "indexがありません。対象rootを指定して『再index』を実行してください".to_owned(),
        );
    }
    let catalog = cached_catalog(&state)?;
    let (backend, max_file_bytes) = {
        let settings = state
            .settings
            .lock()
            .map_err(|_| "settings lock poisoned".to_owned())?;
        (settings.search_backend.clone(), settings.max_bytes)
    };
    let epoch = state.search_epoch.load(AtomicOrdering::Acquire);
    let search_epoch = Arc::clone(&state.search_epoch);
    state.engine.search(
        &state.index_dir,
        SearchCatalogView {
            root: &catalog.root,
            paths: &catalog.paths,
            size_bytes: &catalog.size_bytes,
            modified_ns: &catalog.modified_ns,
            logical_ids: &catalog.logical_ids,
            logical_to_row: &catalog.logical_to_row,
            generation: catalog.generation,
            max_file_bytes,
        },
        request,
        &backend,
        &move || search_epoch.load(AtomicOrdering::Acquire) != epoch,
    )
}

#[tauri::command]
fn cancel_search(state: State<'_, AppState>) -> Result<(), String> {
    state.search_epoch.fetch_add(1, AtomicOrdering::AcqRel);
    Ok(())
}

#[tauri::command]
fn index(request: IndexRequest, state: State<'_, AppState>) -> Result<IndexResponse, String> {
    if request.roots.is_empty() {
        return Err("少なくとも1つのindex rootが必要です".to_owned());
    }
    let mut settings = state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_owned())?
        .clone();
    settings.roots = request.roots;
    settings.max_bytes = request.max_bytes.unwrap_or(DEFAULT_MAX_BYTES).max(1);
    if let Some(scanner_mode) = request.scanner_mode {
        settings.scanner_mode = scanner_mode;
    }
    if let Some(exclusions) = request.exclusions {
        settings.exclusions = exclusions;
    }
    persist_settings(&state.settings_path, &settings)?;
    *state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_owned())? = settings.clone();
    *state
        .catalog
        .lock()
        .map_err(|_| "catalog lock poisoned".to_owned())? = None;
    let (job_id, message) = start_rebuild(&state, settings.clone(), false)?;
    Ok(IndexResponse {
        accepted: true,
        job_id: Some(job_id),
        state: "starting".to_owned(),
        message,
        status: background_status_value(&state)?,
        settings,
    })
}

#[tauri::command]
fn load_settings(state: State<'_, AppState>) -> Result<Settings, String> {
    state
        .settings
        .lock()
        .map(|settings| settings.clone())
        .map_err(|_| "settings lock poisoned".to_owned())
}

#[tauri::command]
fn set_search_backend(
    backend: String,
    state: State<'_, AppState>,
) -> Result<SearchBackendStatus, String> {
    let mut settings = state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_owned())?
        .clone();
    settings.search_backend = backend.clone();
    persist_settings(&state.settings_path, &settings)?;
    *state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_owned())? = settings;
    Ok(SearchBackendStatus {
        requested: backend,
        active: "portable".to_owned(),
        readiness: BackendReadiness {
            search_v2_ready: true,
            state: "portable-core".to_owned(),
        },
    })
}

#[tauri::command]
fn set_search_core_backend(
    backend: String,
    state: State<'_, AppState>,
) -> Result<SearchCoreBackendStatus, String> {
    let mode = ProductionBackendMode::parse(&backend)?;
    state.engine.set_production_backend(mode)?;
    let mut settings = state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_owned())?
        .clone();
    settings.search_core_backend = mode.as_str().to_owned();
    persist_settings(&state.settings_path, &settings)?;
    *state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_owned())? = settings;
    search_core_backend_status(state)
}

#[tauri::command]
fn search_core_backend_status(
    state: State<'_, AppState>,
) -> Result<SearchCoreBackendStatus, String> {
    let requested = state.engine.production_backend()?.as_str().to_owned();
    let generation = state
        .catalog
        .lock()
        .map_err(|_| "catalog lock poisoned".to_owned())?
        .as_ref()
        .map(|catalog| catalog.generation)
        .or_else(|| {
            load_catalog_file(&state.index_dir)
                .ok()
                .map(|catalog| catalog.generation)
        })
        .unwrap_or(0);
    let vnext_ready = state.index_dir.join("CURRENT").exists()
        && state.engine.vnext_ready(&state.index_dir, generation);
    let active = if state.index_dir.join("CURRENT").exists() {
        state
            .engine
            .active_production_backend(&state.index_dir, generation)?
            .to_owned()
    } else {
        "perf12".to_owned()
    };
    let telemetry = state.engine.production_backend_telemetry()?;
    Ok(SearchCoreBackendStatus {
        requested,
        active,
        vnext_ready,
        generation,
        searches: telemetry.searches,
        fallbacks: telemetry.vnext_fallbacks,
        shadow_comparisons: telemetry.shadow_comparisons,
        shadow_mismatches: telemetry.shadow_mismatches,
        shadow_queued: telemetry.shadow_queued,
        shadow_coalesced: telemetry.shadow_coalesced,
        shadow_dropped: telemetry.shadow_dropped,
        shadow_failures: telemetry.shadow_failures,
        common_result_searches: telemetry.common_result_searches,
        common_result_total_micros: telemetry.common_result_total_micros,
        common_result_max_micros: telemetry.common_result_max_micros,
        last_search_micros: telemetry.last_search_micros,
    })
}

#[tauri::command]
fn background_status(state: State<'_, AppState>) -> Result<BackgroundStatus, String> {
    background_status_value(&state)
}

#[tauri::command]
fn background_enable(
    request: BackgroundRequest,
    state: State<'_, AppState>,
) -> Result<BackgroundStatus, String> {
    let mut settings = state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_owned())?
        .clone();
    if !request.roots.is_empty() {
        settings.roots = request.roots;
    }
    settings.max_bytes = request.max_bytes.unwrap_or(settings.max_bytes).max(1);
    if let Some(scanner_mode) = request.scanner_mode {
        settings.scanner_mode = scanner_mode;
    }
    if let Some(exclusions) = request.exclusions {
        settings.exclusions = exclusions;
    }
    settings.background_enabled = true;
    persist_settings(&state.settings_path, &settings)?;
    *state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_owned())? = settings;
    background_status_value(&state)
}

#[tauri::command]
fn background_disable(state: State<'_, AppState>) -> Result<BackgroundStatus, String> {
    let mut settings = state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_owned())?
        .clone();
    settings.background_enabled = false;
    persist_settings(&state.settings_path, &settings)?;
    *state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_owned())? = settings;
    background_status_value(&state)
}

#[tauri::command]
fn background_sync_now(state: State<'_, AppState>) -> Result<BackgroundStatus, String> {
    let settings = state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_owned())?
        .clone();
    start_rebuild(&state, settings, false)?;
    background_status_value(&state)
}

#[tauri::command]
fn background_rebuild(state: State<'_, AppState>) -> Result<BackgroundStatus, String> {
    let settings = state
        .settings
        .lock()
        .map_err(|_| "settings lock poisoned".to_owned())?
        .clone();
    start_rebuild(&state, settings, true)?;
    background_status_value(&state)
}

#[tauri::command]
fn background_cancel(state: State<'_, AppState>) -> Result<BackgroundStatus, String> {
    state.cancel_rebuild.store(true, AtomicOrdering::Release);
    update_rebuild(&state.rebuild, |status| {
        if matches!(
            status.state.as_str(),
            "starting" | "scanning" | "reconciling" | "catching_up"
        ) {
            status.state = "cancelling".to_owned();
        }
    });
    background_status_value(&state)
}

#[tauri::command]
fn snippets(
    request: SnippetRequest,
    state: State<'_, AppState>,
) -> Result<Vec<SnippetHit>, String> {
    state.engine.snippets(&request)
}

#[tauri::command]
fn snippets_batch(
    request: SnippetBatchRequest,
    state: State<'_, AppState>,
) -> Result<Vec<SnippetBatchResult>, String> {
    state.engine.snippets_batch(&request.items)
}

#[tauri::command]
fn contract_info() -> ContractInfo {
    ContractInfo::default()
}

fn ensure_existing_path(path: &Path) -> Result<(), String> {
    if path.exists() {
        Ok(())
    } else {
        Err(format!("pathが存在しません: {}", path.display()))
    }
}

#[tauri::command]
fn open_file(path: PathBuf) -> Result<(), String> {
    ensure_existing_path(&path)?;
    #[cfg(windows)]
    {
        Command::new("cmd")
            .args(["/C", "start", ""])
            .arg(&path)
            .spawn()
            .map_err(|error| error.to_string())?;
    }
    #[cfg(not(windows))]
    {
        Command::new("xdg-open")
            .arg(&path)
            .spawn()
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn open_parent(path: PathBuf) -> Result<(), String> {
    ensure_existing_path(&path)?;
    #[cfg(windows)]
    {
        Command::new("explorer.exe")
            .arg(format!("/select,{}", path.display()))
            .spawn()
            .map_err(|error| error.to_string())?;
    }
    #[cfg(not(windows))]
    {
        let parent = path.parent().unwrap_or(Path::new("."));
        Command::new("xdg-open")
            .arg(parent)
            .spawn()
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let app_data_dir = if let Ok(path) = std::env::var("PERSONALRAG_APP_DATA_DIR") {
                PathBuf::from(path)
            } else {
                app.path().app_data_dir()?
            };
            fs::create_dir_all(&app_data_dir)?;
            let settings_path = app_data_dir.join("settings.json");
            let mut settings = load_settings_file(&settings_path);
            let production_backend = std::env::var("PERSONALRAG_SEARCH_CORE_BACKEND")
                .ok()
                .as_deref()
                .map(ProductionBackendMode::parse)
                .transpose()
                .unwrap_or_else(|error| {
                    eprintln!("invalid PERSONALRAG_SEARCH_CORE_BACKEND: {error}; using settings");
                    None
                })
                .or_else(|| ProductionBackendMode::parse(&settings.search_core_backend).ok())
                .unwrap_or(ProductionBackendMode::Perf12);
            // Keep the in-memory setting aligned with the effective backend so
            // incremental eligibility and the UI observe an environment override.
            // The override is intentionally not persisted to settings.json.
            settings.search_core_backend = production_backend.as_str().to_owned();
            app.manage(AppState {
                index_dir: app_data_dir.join("portable-index"),
                app_data_dir,
                settings_path,
                settings: Mutex::new(settings),
                rebuild: Arc::new(Mutex::new(None)),
                cancel_rebuild: Arc::new(AtomicBool::new(false)),
                search_epoch: Arc::new(AtomicU64::new(0)),
                index_access: Arc::new(RwLock::new(())),
                catalog: Arc::new(Mutex::new(None)),
                engine: Arc::new(PortableEngine::with_production_backend(production_backend)),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            contract_info,
            search,
            cancel_search,
            index,
            load_settings,
            set_search_backend,
            set_search_core_backend,
            search_core_backend_status,
            snippets,
            snippets_batch,
            open_file,
            open_parent,
            background_status,
            background_enable,
            background_disable,
            background_sync_now,
            background_rebuild,
            background_cancel,
        ])
        .run(tauri::generate_context!())
        .expect("error while running PersonalRag");
}
