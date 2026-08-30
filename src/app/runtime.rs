use super::{
    AppPaths, ContentBuildOptions, DiscoveredVolume, Result, VolumeKey, VolumePhase,
    begin_metadata_refresh, build_content_step, build_or_resume_metadata, content_progress,
    load_volume_manifest,
};
use crate::extraction::ExtractorConfig;
use std::collections::HashMap;
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::Duration;
#[cfg(any(windows, test))]
use std::time::Instant;

const WATCH_POLL_INTERVAL: Duration = Duration::from_millis(250);
#[cfg(windows)]
const FALLBACK_RECONCILE_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeVolumeStatus {
    pub key: VolumeKey,
    pub mount: std::path::PathBuf,
    pub phase: VolumePhase,
    pub metadata_records: usize,
    pub inaccessible_directories: usize,
    pub content_indexed_files: usize,
    pub content_total_files: usize,
    pub content_skipped_files: usize,
    pub content_shards: usize,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeSnapshot {
    pub revision: u64,
    pub running: bool,
    pub total_volumes: usize,
    pub metadata_ready_volumes: usize,
    pub content_ready_volumes: usize,
    pub filename_search_available: bool,
    pub content_search_available: bool,
    pub volumes: Vec<RuntimeVolumeStatus>,
}

impl RuntimeSnapshot {
    fn starting(volumes: &[DiscoveredVolume]) -> Self {
        Self {
            revision: 0,
            running: true,
            total_volumes: volumes.len(),
            metadata_ready_volumes: 0,
            content_ready_volumes: 0,
            filename_search_available: false,
            content_search_available: false,
            volumes: volumes
                .iter()
                .map(|volume| RuntimeVolumeStatus {
                    key: volume.key.clone(),
                    mount: volume.mount.clone(),
                    phase: VolumePhase::Discovered,
                    metadata_records: 0,
                    inaccessible_directories: 0,
                    content_indexed_files: 0,
                    content_total_files: 0,
                    content_skipped_files: 0,
                    content_shards: 0,
                    last_error: None,
                })
                .collect(),
        }
    }
}

#[derive(Clone)]
pub struct RuntimeReader {
    snapshot: Arc<RwLock<RuntimeSnapshot>>,
}

impl RuntimeReader {
    pub fn snapshot(&self) -> RuntimeSnapshot {
        self.snapshot
            .read()
            .map(|value| value.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    pub fn revision(&self) -> u64 {
        self.snapshot().revision
    }
}

pub struct AppRuntimeHandle {
    paths: AppPaths,
    volumes: Vec<DiscoveredVolume>,
    snapshot: Arc<RwLock<RuntimeSnapshot>>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl AppRuntimeHandle {
    pub fn start_default(extractor: ExtractorConfig) -> Result<Self> {
        let coordinator = super::AppCoordinator::new_default()?;
        Self::start_with(coordinator.paths, coordinator.volumes, extractor, true)
    }

    pub fn start_with(
        paths: AppPaths,
        volumes: Vec<DiscoveredVolume>,
        extractor: ExtractorConfig,
        watch_changes: bool,
    ) -> Result<Self> {
        paths.ensure()?;
        let snapshot = Arc::new(RwLock::new(RuntimeSnapshot::starting(&volumes)));
        publish_snapshot(&paths, &volumes, &snapshot, &HashMap::new(), true);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_paths = paths.clone();
        let thread_volumes = volumes.clone();
        let thread_snapshot = Arc::clone(&snapshot);
        let thread_stop = Arc::clone(&stop);
        let join = thread::Builder::new()
            .name("personalrag-index-coordinator".to_string())
            .spawn(move || {
                run_coordinator(
                    thread_paths,
                    thread_volumes,
                    extractor,
                    thread_snapshot,
                    thread_stop,
                    watch_changes,
                );
            })?;
        Ok(Self {
            paths,
            volumes,
            snapshot,
            stop,
            join: Some(join),
        })
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    pub fn volumes(&self) -> &[DiscoveredVolume] {
        &self.volumes
    }

    pub fn snapshot(&self) -> RuntimeSnapshot {
        self.snapshot
            .read()
            .map(|value| value.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    pub fn revision(&self) -> u64 {
        self.snapshot().revision
    }

    pub fn reader(&self) -> RuntimeReader {
        RuntimeReader {
            snapshot: Arc::clone(&self.snapshot),
        }
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    pub fn join(&mut self) {
        self.request_stop();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for AppRuntimeHandle {
    fn drop(&mut self) {
        self.request_stop();
        let _ = self.join.take();
    }
}

fn run_coordinator(
    paths: AppPaths,
    volumes: Vec<DiscoveredVolume>,
    extractor: ExtractorConfig,
    snapshot: Arc<RwLock<RuntimeSnapshot>>,
    stop: Arc<AtomicBool>,
    watch_changes: bool,
) {
    let mut errors = HashMap::<VolumeKey, String>::new();
    let mut watchers = RuntimeWatchers::new(&volumes, watch_changes);

    for volume in &volumes {
        if stop.load(Ordering::Acquire) {
            publish_snapshot(&paths, &volumes, &snapshot, &errors, false);
            return;
        }
        let store = paths.volume_store(&volume.key);
        let existing = load_volume_manifest(&store).ok().flatten();
        if existing
            .as_ref()
            .and_then(|manifest| manifest.metadata_file.as_ref())
            .is_some()
            && existing.as_ref().is_some_and(|manifest| {
                manifest.phase != VolumePhase::MetadataBuilding
            })
            && let Err(error) = begin_metadata_refresh(&paths, volume)
        {
            errors.insert(volume.key.clone(), error.to_string());
            publish_snapshot(&paths, &volumes, &snapshot, &errors, true);
            continue;
        }
        let mut should_stop = || stop.load(Ordering::Acquire);
        match build_or_resume_metadata(
            &paths,
            volume,
            std::slice::from_ref(&paths.root),
            super::METADATA_CHECKPOINT_RECORDS,
            &mut should_stop,
        ) {
            Ok(_) => {
                errors.remove(&volume.key);
            }
            Err(error) => {
                errors.insert(volume.key.clone(), error.to_string());
            }
        }
        publish_snapshot(&paths, &volumes, &snapshot, &errors, true);
    }

    if stop.load(Ordering::Acquire) {
        publish_snapshot(&paths, &volumes, &snapshot, &errors, false);
        return;
    }

    for volume in &volumes {
        if watchers.should_refresh(volume) {
            refresh_volume_metadata(
                &paths,
                volume,
                &stop,
                &mut errors,
                &snapshot,
                &volumes,
            );
            watchers.reopen(volume);
        }
    }

    let options = ContentBuildOptions::default();
    loop {
        if stop.load(Ordering::Acquire) {
            publish_snapshot(&paths, &volumes, &snapshot, &errors, false);
            return;
        }

        let mut all_terminal = true;
        let mut made_progress = false;
        for volume in &volumes {
            if stop.load(Ordering::Acquire) {
                break;
            }

            if watchers.should_refresh(volume) {
                refresh_volume_metadata(
                    &paths,
                    volume,
                    &stop,
                    &mut errors,
                    &snapshot,
                    &volumes,
                );
                watchers.reopen(volume);
                made_progress = true;
            }

            let store = paths.volume_store(&volume.key);
            let metadata_available = load_volume_manifest(&store)
                .ok()
                .flatten()
                .and_then(|manifest| manifest.metadata_file)
                .is_some();
            if !metadata_available {
                continue;
            }

            match content_progress(&paths, volume) {
                Ok(Some(progress)) if progress.complete => {}
                Ok(_) => {
                    all_terminal = false;
                    match build_content_step(&paths, volume, &extractor, options) {
                        Ok(_) => {
                            errors.remove(&volume.key);
                            made_progress = true;
                        }
                        Err(error) => {
                            errors.insert(volume.key.clone(), error.to_string());
                        }
                    }
                    publish_snapshot(&paths, &volumes, &snapshot, &errors, true);
                }
                Err(error) => {
                    all_terminal = false;
                    errors.insert(volume.key.clone(), error.to_string());
                }
            }
        }

        if all_terminal {
            break;
        }
        if !made_progress {
            thread::sleep(WATCH_POLL_INTERVAL);
        }
    }

    publish_snapshot(&paths, &volumes, &snapshot, &errors, true);

    while !stop.load(Ordering::Acquire) {
        let mut refreshed = false;
        for volume in &volumes {
            if stop.load(Ordering::Acquire) {
                break;
            }
            if !watchers.should_refresh(volume) {
                continue;
            }
            refresh_volume_metadata(
                &paths,
                volume,
                &stop,
                &mut errors,
                &snapshot,
                &volumes,
            );
            watchers.reopen(volume);
            refreshed = true;

            if stop.load(Ordering::Acquire) {
                break;
            }
            loop {
                match build_content_step(&paths, volume, &extractor, options) {
                    Ok(report) => {
                        errors.remove(&volume.key);
                        publish_snapshot(&paths, &volumes, &snapshot, &errors, true);
                        if report.complete || stop.load(Ordering::Acquire) {
                            break;
                        }
                    }
                    Err(error) => {
                        errors.insert(volume.key.clone(), error.to_string());
                        publish_snapshot(&paths, &volumes, &snapshot, &errors, true);
                        break;
                    }
                }
            }
        }
        if !refreshed {
            thread::sleep(WATCH_POLL_INTERVAL);
        }
    }

    publish_snapshot(&paths, &volumes, &snapshot, &errors, false);
}

fn refresh_volume_metadata(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
    stop: &AtomicBool,
    errors: &mut HashMap<VolumeKey, String>,
    snapshot: &Arc<RwLock<RuntimeSnapshot>>,
    all_volumes: &[DiscoveredVolume],
) {
    if let Err(error) = begin_metadata_refresh(paths, volume) {
        errors.insert(volume.key.clone(), error.to_string());
        publish_snapshot(paths, all_volumes, snapshot, errors, true);
        return;
    }
    publish_snapshot(paths, all_volumes, snapshot, errors, true);
    let mut should_stop = || stop.load(Ordering::Acquire);
    match build_or_resume_metadata(
        paths,
        volume,
        std::slice::from_ref(&paths.root),
        super::METADATA_CHECKPOINT_RECORDS,
        &mut should_stop,
    ) {
        Ok(_) => {
            errors.remove(&volume.key);
        }
        Err(error) => {
            errors.insert(volume.key.clone(), error.to_string());
        }
    }
    publish_snapshot(paths, all_volumes, snapshot, errors, true);
}

fn publish_snapshot(
    paths: &AppPaths,
    volumes: &[DiscoveredVolume],
    shared: &Arc<RwLock<RuntimeSnapshot>>,
    errors: &HashMap<VolumeKey, String>,
    running: bool,
) {
    let previous_revision = shared
        .read()
        .map(|value| value.revision)
        .unwrap_or_else(|poisoned| poisoned.into_inner().revision);
    let mut statuses = Vec::with_capacity(volumes.len());
    let mut metadata_ready = 0_usize;
    let mut content_ready = 0_usize;
    let mut content_available = false;

    for volume in volumes {
        let store = paths.volume_store(&volume.key);
        let manifest = load_volume_manifest(&store).ok().flatten();
        let progress = content_progress(paths, volume).ok().flatten();
        if manifest
            .as_ref()
            .and_then(|value| value.metadata_file.as_ref())
            .is_some()
        {
            metadata_ready += 1;
        }
        if progress.as_ref().is_some_and(|value| value.complete) {
            content_ready += 1;
        }
        if progress
            .as_ref()
            .is_some_and(|value| value.indexed_cursor > 0 || value.complete)
        {
            content_available = true;
        }

        let error = errors.get(&volume.key).cloned();
        statuses.push(RuntimeVolumeStatus {
            key: volume.key.clone(),
            mount: volume.mount.clone(),
            phase: if error.is_some() {
                VolumePhase::Degraded
            } else {
                manifest
                    .as_ref()
                    .map_or(VolumePhase::Discovered, |value| value.phase)
            },
            metadata_records: manifest.as_ref().map_or(0, |value| value.metadata_records),
            inaccessible_directories: manifest
                .as_ref()
                .map_or(0, |value| value.inaccessible_directories),
            content_indexed_files: progress.as_ref().map_or(0, |value| value.indexed_cursor),
            content_total_files: progress.as_ref().map_or(0, |value| value.total_files),
            content_skipped_files: progress.as_ref().map_or(0, |value| value.skipped_files),
            content_shards: progress.as_ref().map_or(0, |value| value.shard_count),
            last_error: error,
        });
    }

    let next = RuntimeSnapshot {
        revision: previous_revision.saturating_add(1),
        running,
        total_volumes: volumes.len(),
        metadata_ready_volumes: metadata_ready,
        content_ready_volumes: content_ready,
        filename_search_available: metadata_ready > 0,
        content_search_available: content_available,
        volumes: statuses,
    };
    match shared.write() {
        Ok(mut value) => *value = next,
        Err(poisoned) => *poisoned.into_inner() = next,
    }
}

struct RuntimeWatchers {
    enabled: bool,
    #[cfg(windows)]
    slots: HashMap<VolumeKey, WatchSlot>,
}

#[cfg(windows)]
struct WatchSlot {
    notification: Option<crate::windows_watch::live::ChangeNotification>,
    last_fallback: Instant,
}

impl RuntimeWatchers {
    fn new(volumes: &[DiscoveredVolume], enabled: bool) -> Self {
        #[cfg(windows)]
        {
            let mut slots = HashMap::new();
            if enabled {
                for volume in volumes {
                    slots.insert(
                        volume.key.clone(),
                        WatchSlot {
                            notification:
                                crate::windows_watch::live::ChangeNotification::open(&volume.mount)
                                    .ok(),
                            last_fallback: Instant::now(),
                        },
                    );
                }
            }
            Self { enabled, slots }
        }
        #[cfg(not(windows))]
        {
            let _ = volumes;
            Self { enabled }
        }
    }

    fn should_refresh(&mut self, volume: &DiscoveredVolume) -> bool {
        if !self.enabled {
            return false;
        }
        #[cfg(windows)]
        {
            let Some(slot) = self.slots.get_mut(&volume.key) else {
                return false;
            };
            if let Some(notification) = slot.notification.as_ref() {
                match notification.poll_changed() {
                    Ok(true) => {
                        slot.last_fallback = Instant::now();
                        return true;
                    }
                    Ok(false) => return false,
                    Err(_) => {
                        slot.notification = None;
                        slot.last_fallback = Instant::now() - FALLBACK_RECONCILE_INTERVAL;
                    }
                }
            }
            if slot.last_fallback.elapsed() >= FALLBACK_RECONCILE_INTERVAL {
                slot.last_fallback = Instant::now();
                return true;
            }
            false
        }
        #[cfg(not(windows))]
        {
            let _ = volume;
            false
        }
    }

    fn reopen(&mut self, volume: &DiscoveredVolume) {
        if !self.enabled {
            return;
        }
        #[cfg(windows)]
        {
            let Some(slot) = self.slots.get_mut(&volume.key) else {
                return;
            };
            if slot.notification.is_none() {
                slot.notification =
                    crate::windows_watch::live::ChangeNotification::open(&volume.mount).ok();
            }
            slot.last_fallback = Instant::now();
        }
        #[cfg(not(windows))]
        {
            let _ = volume;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{AppPaths, DiscoveredVolume, VolumeKey};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "personalrag-runtime-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn volume(root: &Path) -> DiscoveredVolume {
        DiscoveredVolume {
            key: VolumeKey("runtime-test".to_string()),
            mount: root.to_path_buf(),
            serial: 1,
        }
    }

    #[test]
    fn runtime_builds_metadata_then_content_in_background_and_stops_cleanly() {
        let base = temp_dir("background");
        let root = base.join("root");
        let app_root = base.join("app");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "runtime-needle").unwrap();

        let paths = AppPaths::for_root(&app_root);
        let volume = volume(&root);
        let mut runtime = AppRuntimeHandle::start_with(
            paths,
            vec![volume],
            ExtractorConfig::discover(),
            false,
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = runtime.snapshot();
            if status.content_ready_volumes == 1 {
                assert!(status.filename_search_available);
                assert!(status.content_search_available);
                break;
            }
            assert!(Instant::now() < deadline, "runtime did not become ready");
            thread::sleep(Duration::from_millis(20));
        }

        runtime.join();
        assert!(!runtime.snapshot().running);
        fs::remove_dir_all(base).unwrap();
    }
}
