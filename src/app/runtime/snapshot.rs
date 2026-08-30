use super::super::incremental_runtime::incremental_metadata_status;
use super::super::{
    AppPaths, DiscoveredVolume, VolumeKey, VolumePhase, content_progress, load_volume_manifest,
};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

pub(super) type SharedSnapshot = Arc<RwLock<RuntimeSnapshot>>;

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
    pub metadata_delta_changes: usize,
    pub content_dirty_files: usize,
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
                    metadata_delta_changes: 0,
                    content_dirty_files: 0,
                    last_error: None,
                })
                .collect(),
        }
    }
}

#[derive(Clone)]
pub struct RuntimeReader {
    snapshot: SharedSnapshot,
}

impl RuntimeReader {
    pub(super) fn new(snapshot: SharedSnapshot) -> Self {
        Self { snapshot }
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
}

pub(super) fn starting_snapshot(volumes: &[DiscoveredVolume]) -> SharedSnapshot {
    Arc::new(RwLock::new(RuntimeSnapshot::starting(volumes)))
}

pub(super) fn publish_snapshot(
    paths: &AppPaths,
    volumes: &[DiscoveredVolume],
    shared: &SharedSnapshot,
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
        let incremental = incremental_metadata_status(paths, volume).unwrap_or_default();
        if manifest
            .as_ref()
            .and_then(|value| value.metadata_file.as_ref())
            .is_some()
        {
            metadata_ready += 1;
        }
        if progress.as_ref().is_some_and(|value| value.complete)
            && manifest
                .as_ref()
                .is_some_and(|value| value.phase == VolumePhase::Ready)
            && !incremental.content_dirty
        {
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
            metadata_delta_changes: incremental.change_count,
            content_dirty_files: incremental.content_dirty_files,
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
