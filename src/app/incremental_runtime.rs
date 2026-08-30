#[cfg(any(windows, test))]
use super::metadata_build::{directory_is_reparse_point, metadata_record_for_existing_path};
use super::state_io::{atomic_write_new, next_number, numbered_files, parse_key_values, parse_u64};
use super::{AppPaths, DiscoveredVolume, Result, VolumeManifest, load_volume_manifest};
use super::{VolumePhase, write_volume_manifest};
use crate::incremental::{
    DeltaOverlay, DeltaSnapshot, IncrementalState, load_delta_generation, load_state_generation,
    write_delta_generation, write_state_generation,
};
#[cfg(any(windows, test))]
use crate::metadata::MetadataFileKind;
use crate::metadata::MetadataIndex;
#[cfg(windows)]
use crate::usn::CheckpointStatus;
#[cfg(any(windows, test))]
use crate::usn::NormalizedFsChange;
use crate::usn::UsnCheckpoint;
#[cfg(any(windows, test))]
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

const PAIR_VERSION: u64 = 1;
const PAIR_PREFIX: &str = "incremental-pair-";
const PAIR_SUFFIX: &str = ".state";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IncrementalCheckpointStatus {
    Missing,
    Valid,
    ReconcileRequired,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IncrementalMetadataStatus {
    pub change_count: usize,
    pub content_dirty: bool,
    pub content_dirty_files: usize,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct CatchUpCursor {
    journal_id: u64,
    next_usn: i64,
    incremental_generation: u64,
    initialized: bool,
}

impl CatchUpCursor {
    pub(super) fn reset(&mut self, checkpoint: UsnCheckpoint, incremental_generation: u64) {
        self.journal_id = checkpoint.journal_id;
        self.next_usn = checkpoint.next_usn;
        self.incremental_generation = incremental_generation;
        self.initialized = true;
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    fn usable_for(
        self,
        checkpoint: UsnCheckpoint,
        incremental_generation: u64,
        journal_next_usn: i64,
    ) -> bool {
        self.initialized
            && self.journal_id == checkpoint.journal_id
            && self.incremental_generation == incremental_generation
            && self.next_usn >= checkpoint.next_usn
            && self.next_usn <= journal_next_usn
    }
}

#[derive(Clone, Debug)]
struct IncrementalPair {
    generation: u64,
    metadata_generation: u64,
    delta_generation: u64,
    state_generation: u64,
}

pub(super) struct LoadedVolumeIncremental {
    pub delta: DeltaOverlay,
    pub state: IncrementalState,
}

#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum CatchUpResult {
    NoChanges {
        content_dirty: bool,
    },
    Applied {
        metadata_changes: usize,
        content_dirty: bool,
    },
    NeedsReconcile {
        reason: String,
    },
}

#[cfg(any(windows, test))]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct AppliedChanges {
    count: usize,
    requires_reconcile: bool,
    dirty_add: std::collections::BTreeSet<u64>,
    dirty_remove: std::collections::BTreeSet<u64>,
}

pub(super) fn incremental_dir(paths: &AppPaths, volume: &DiscoveredVolume) -> PathBuf {
    paths.volume_store(&volume.key).join("incremental")
}

pub(super) fn load_volume_incremental(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
    manifest: &VolumeManifest,
) -> Result<Option<LoadedVolumeIncremental>> {
    let metadata = load_manifest_metadata(paths, volume, manifest)?;
    load_volume_incremental_with_base(paths, volume, manifest, &metadata)
}

pub(super) fn load_volume_incremental_with_base(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
    manifest: &VolumeManifest,
    metadata: &MetadataIndex,
) -> Result<Option<LoadedVolumeIncremental>> {
    let Some((snapshot, state)) = load_incremental_parts(paths, volume, manifest)? else {
        return Ok(None);
    };
    Ok(Some(LoadedVolumeIncremental {
        delta: DeltaOverlay::from_snapshot(metadata, snapshot),
        state,
    }))
}

fn load_incremental_parts(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
    manifest: &VolumeManifest,
) -> Result<Option<(DeltaSnapshot, IncrementalState)>> {
    let store = incremental_dir(paths, volume);
    let mut pairs = numbered_files(&store, PAIR_PREFIX, PAIR_SUFFIX)?;
    pairs.sort_unstable_by_key(|(generation, _)| std::cmp::Reverse(*generation));
    for (generation, path) in pairs {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        let values = parse_key_values(&text);
        if parse_u64(&values, "version") != Some(PAIR_VERSION) {
            continue;
        }
        let Some(metadata_generation) = parse_u64(&values, "metadata_generation") else {
            continue;
        };
        if metadata_generation != manifest.metadata_generation {
            continue;
        }
        let Some(delta_generation) = parse_u64(&values, "delta_generation") else {
            continue;
        };
        let Some(state_generation) = parse_u64(&values, "state_generation") else {
            continue;
        };
        let pair = IncrementalPair {
            generation,
            metadata_generation,
            delta_generation,
            state_generation,
        };
        let Ok(snapshot) = load_delta_generation(&store, pair.delta_generation) else {
            continue;
        };
        if snapshot.generation != pair.delta_generation {
            continue;
        }
        let Ok(state) = load_state_generation(&store, pair.state_generation) else {
            continue;
        };
        if state.generation != pair.state_generation {
            continue;
        }
        return Ok(Some((snapshot, state)));
    }
    Ok(None)
}

pub(super) fn incremental_metadata_status(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
) -> Result<IncrementalMetadataStatus> {
    let store = paths.volume_store(&volume.key);
    let Some(manifest) = load_volume_manifest(&store)? else {
        return Ok(IncrementalMetadataStatus::default());
    };
    let snapshot =
        load_incremental_parts(paths, volume, &manifest)?.map(|(snapshot, _state)| snapshot);
    let dirty =
        super::content_dirty::load_for_metadata(paths, volume, manifest.metadata_generation)?;
    let dirty_files = dirty.as_ref().map_or(0, |state| state.ids.len());
    Ok(IncrementalMetadataStatus {
        change_count: snapshot.as_ref().map_or(0, |snapshot| {
            snapshot
                .upserts
                .len()
                .saturating_add(snapshot.tombstones.len())
        }),
        content_dirty: dirty_files > 0,
        content_dirty_files: dirty_files,
    })
}

pub(super) fn materialized_volume_metadata(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
) -> Result<Option<MetadataIndex>> {
    let store = paths.volume_store(&volume.key);
    let Some(manifest) = load_volume_manifest(&store)? else {
        return Ok(None);
    };
    let base = load_manifest_metadata(paths, volume, &manifest)?;
    let Some(loaded) = load_volume_incremental_with_base(paths, volume, &manifest, &base)? else {
        return Ok(Some(base));
    };
    if loaded.delta.change_count() == 0 {
        Ok(Some(base))
    } else {
        Ok(Some(MetadataIndex::build(
            loaded.delta.materialize_records(&base),
        )?))
    }
}

#[cfg(windows)]
pub(super) fn mark_content_catch_up_if_needed(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
) -> Result<()> {
    let status = incremental_metadata_status(paths, volume)?;
    if !status.content_dirty {
        return Ok(());
    }
    let store = paths.volume_store(&volume.key);
    let Some(current) = load_volume_manifest(&store)? else {
        return Ok(());
    };
    if current.phase != VolumePhase::Ready {
        return Ok(());
    }
    let next = VolumeManifest {
        generation: current.generation.saturating_add(1).max(1),
        key: current.key.clone(),
        mount: current.mount.clone(),
        phase: VolumePhase::ContentCatchUp,
        metadata_generation: current.metadata_generation,
        metadata_file: current.metadata_file.clone(),
        metadata_records: current.metadata_records,
        inaccessible_directories: current.inaccessible_directories,
    };
    write_volume_manifest(&store, &next)?;
    Ok(())
}

pub(super) fn initialize_volume_incremental(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
    checkpoint: UsnCheckpoint,
) -> Result<u64> {
    let volume_store = paths.volume_store(&volume.key);
    let manifest = load_volume_manifest(&volume_store)?.ok_or_else(|| {
        super::AppError::InvalidState("incremental initialization requires volume manifest".into())
    })?;
    let metadata = load_manifest_metadata(paths, volume, &manifest)?;
    let overlay = DeltaOverlay::new(&metadata, 0, 0);
    let state = IncrementalState {
        generation: 0,
        checkpoint,
        pending_renames: Vec::new(),
    };
    if super::content_dirty::load_for_metadata(paths, volume, manifest.metadata_generation)?
        .is_none()
    {
        super::content_dirty::replace(
            paths,
            volume,
            manifest.metadata_generation,
            std::iter::empty(),
        )?;
    }
    persist_volume_incremental(paths, volume, &manifest, &overlay, &state)
}

pub fn incremental_checkpoint_status(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
) -> Result<IncrementalCheckpointStatus> {
    let store = paths.volume_store(&volume.key);
    let Some(manifest) = load_volume_manifest(&store)? else {
        return Ok(IncrementalCheckpointStatus::Missing);
    };
    let Some(loaded) = load_volume_incremental(paths, volume, &manifest)? else {
        return Ok(IncrementalCheckpointStatus::Missing);
    };
    platform_checkpoint_status(volume, loaded.state.checkpoint)
}

#[cfg(windows)]
fn platform_checkpoint_status(
    volume: &DiscoveredVolume,
    checkpoint: UsnCheckpoint,
) -> Result<IncrementalCheckpointStatus> {
    let device = crate::product::windows_volume_device(&volume.mount)?;
    let handle = match crate::windows_usn::live::VolumeHandle::open(&device) {
        Ok(value) => value,
        Err(_) => return Ok(IncrementalCheckpointStatus::Unavailable),
    };
    let bounds = match handle.query_journal() {
        Ok(value) => value,
        Err(_) => return Ok(IncrementalCheckpointStatus::Unavailable),
    };
    Ok(match crate::usn::validate_checkpoint(checkpoint, bounds) {
        CheckpointStatus::Valid => IncrementalCheckpointStatus::Valid,
        CheckpointStatus::ReconcileRequired => IncrementalCheckpointStatus::ReconcileRequired,
    })
}

#[cfg(not(windows))]
fn platform_checkpoint_status(
    _volume: &DiscoveredVolume,
    _checkpoint: UsnCheckpoint,
) -> Result<IncrementalCheckpointStatus> {
    Ok(IncrementalCheckpointStatus::Unavailable)
}

#[cfg(windows)]
pub(super) fn capture_volume_checkpoint(volume: &DiscoveredVolume) -> Option<UsnCheckpoint> {
    let device = crate::product::windows_volume_device(&volume.mount).ok()?;
    let handle = crate::windows_usn::live::VolumeHandle::open(&device).ok()?;
    let bounds = handle.query_journal().ok()?;
    Some(UsnCheckpoint {
        journal_id: bounds.journal_id,
        next_usn: bounds.next_usn,
    })
}

#[cfg(not(windows))]
pub(super) fn capture_volume_checkpoint(_volume: &DiscoveredVolume) -> Option<UsnCheckpoint> {
    None
}

#[cfg(windows)]
pub(super) fn catch_up_volume(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
    cursor: &mut CatchUpCursor,
) -> Result<CatchUpResult> {
    let volume_store = paths.volume_store(&volume.key);
    let Some(manifest) = load_volume_manifest(&volume_store)? else {
        return Ok(CatchUpResult::NeedsReconcile {
            reason: "missing volume manifest".into(),
        });
    };
    let metadata = load_manifest_metadata(paths, volume, &manifest)?;
    let Some(mut loaded) = load_volume_incremental(paths, volume, &manifest)? else {
        return Ok(CatchUpResult::NeedsReconcile {
            reason: "missing valid incremental checkpoint".into(),
        });
    };

    let device = crate::product::windows_volume_device(&volume.mount)?;
    let handle = match crate::windows_usn::live::VolumeHandle::open(&device) {
        Ok(value) => value,
        Err(error) => {
            return Ok(CatchUpResult::NeedsReconcile {
                reason: format!("USN volume open failed: {error}"),
            });
        }
    };
    let bounds = match handle.query_journal() {
        Ok(value) => value,
        Err(error) => {
            return Ok(CatchUpResult::NeedsReconcile {
                reason: format!("USN journal query failed: {error}"),
            });
        }
    };
    let durable_checkpoint = loaded.state.checkpoint;
    let cursor_usable =
        cursor.usable_for(durable_checkpoint, loaded.state.generation, bounds.next_usn);
    let checkpoint_for_validation = if cursor_usable && loaded.state.pending_renames.is_empty() {
        UsnCheckpoint {
            journal_id: cursor.journal_id,
            next_usn: cursor.next_usn,
        }
    } else {
        durable_checkpoint
    };
    if crate::usn::validate_checkpoint(checkpoint_for_validation, bounds)
        == CheckpointStatus::ReconcileRequired
    {
        return Ok(CatchUpResult::NeedsReconcile {
            reason: "USN checkpoint outside current journal bounds".into(),
        });
    }
    if !cursor_usable
        && crate::usn::validate_checkpoint(durable_checkpoint, bounds)
            == CheckpointStatus::ReconcileRequired
    {
        return Ok(CatchUpResult::NeedsReconcile {
            reason: "durable USN checkpoint outside current journal bounds".into(),
        });
    }

    let normalizer_checkpoint = if cursor_usable && loaded.state.pending_renames.is_empty() {
        checkpoint_for_validation
    } else {
        durable_checkpoint
    };
    let mut normalizer = crate::usn::UsnNormalizer::from_persisted(
        normalizer_checkpoint,
        loaded.state.pending_renames.clone(),
    );
    let excluded_ids = collect_excluded_file_ids(paths, volume);
    let root_id = root_file_id(volume)?;
    let target_next_usn = bounds.next_usn;
    let mut read_cursor = if cursor_usable {
        cursor.next_usn
    } else {
        durable_checkpoint.next_usn
    };
    let mut applied_total = 0_usize;
    let mut iterations = 0_usize;

    while read_cursor < target_next_usn && iterations < 256 {
        iterations += 1;
        let (observed_next, records) = match handle.read_journal(
            read_cursor,
            loaded.state.checkpoint.journal_id,
            u32::MAX,
            8 * 1024 * 1024,
        ) {
            Ok(value) => value,
            Err(error) => {
                return Ok(CatchUpResult::NeedsReconcile {
                    reason: format!("USN journal read failed: {error}"),
                });
            }
        };
        if observed_next < read_cursor {
            return Ok(CatchUpResult::NeedsReconcile {
                reason: "USN read cursor moved backwards".into(),
            });
        }
        let previous_pending = loaded.state.pending_renames.clone();
        let changes = normalizer.process_batch(&records, observed_next);
        if changes
            .iter()
            .any(|change| matches!(change, NormalizedFsChange::ReconcileRequired))
        {
            return Ok(CatchUpResult::NeedsReconcile {
                reason: "USN normalizer requested reconcile".into(),
            });
        }
        let applied = match apply_normalized_changes(
            paths,
            volume,
            &metadata,
            &mut loaded.delta,
            root_id,
            &excluded_ids,
            &changes,
        ) {
            Ok(value) => value,
            Err(error) => {
                return Ok(CatchUpResult::NeedsReconcile {
                    reason: format!(
                        "incremental metadata event could not be resolved safely: {error}"
                    ),
                });
            }
        };
        if applied.requires_reconcile {
            return Ok(CatchUpResult::NeedsReconcile {
                reason: "incremental metadata path resolution requested reconcile".into(),
            });
        }
        applied_total = applied_total.saturating_add(applied.count);
        let pending = normalizer.pending_state();
        let must_persist = applied.count > 0 || pending != previous_pending;
        if must_persist {
            loaded.state.checkpoint = normalizer.checkpoint();
            loaded.state.pending_renames = pending;
            let previous_delta_generation = loaded.delta.generation();
            let generation =
                persist_volume_incremental(paths, volume, &manifest, &loaded.delta, &loaded.state)?;
            let mut persisted_snapshot = loaded.delta.snapshot();
            persisted_snapshot.parent_generation = previous_delta_generation;
            persisted_snapshot.generation = generation;
            loaded.delta = DeltaOverlay::from_snapshot(&metadata, persisted_snapshot);
            loaded.state.generation = generation;
            let _ = super::content_dirty::merge(
                paths,
                volume,
                manifest.metadata_generation,
                applied.dirty_add.iter().copied(),
                applied.dirty_remove.iter().copied(),
            )?;
        }
        if observed_next == read_cursor {
            break;
        }
        read_cursor = observed_next;
    }

    if read_cursor < target_next_usn && iterations >= 256 {
        return Ok(CatchUpResult::NeedsReconcile {
            reason: "USN catch-up exceeded bounded batch count".into(),
        });
    }
    cursor.journal_id = durable_checkpoint.journal_id;
    cursor.next_usn = read_cursor;
    cursor.incremental_generation = loaded.state.generation;
    cursor.initialized = true;
    let content_dirty =
        super::content_dirty::load_for_metadata(paths, volume, manifest.metadata_generation)?
            .is_some_and(|state| !state.ids.is_empty());
    if content_dirty {
        mark_content_catch_up_if_needed(paths, volume)?;
    }
    if applied_total == 0 {
        Ok(CatchUpResult::NoChanges { content_dirty })
    } else {
        Ok(CatchUpResult::Applied {
            metadata_changes: applied_total,
            content_dirty,
        })
    }
}

#[cfg(not(windows))]
pub(super) fn catch_up_volume(
    _paths: &AppPaths,
    _volume: &DiscoveredVolume,
    _cursor: &mut CatchUpCursor,
) -> Result<CatchUpResult> {
    Ok(CatchUpResult::NeedsReconcile {
        reason: "USN catch-up is Windows-only".into(),
    })
}

pub(super) fn compact_volume_metadata_if_needed(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
    extractor: &crate::extraction::ExtractorConfig,
) -> Result<Option<(UsnCheckpoint, u64)>> {
    let store = paths.volume_store(&volume.key);
    let Some(manifest) = load_volume_manifest(&store)? else {
        return Ok(None);
    };
    let base = load_manifest_metadata(paths, volume, &manifest)?;
    let Some(loaded) = load_volume_incremental_with_base(paths, volume, &manifest, &base)? else {
        return Ok(None);
    };
    if !loaded.delta.should_compact(base.records().len()) {
        return Ok(None);
    }
    if super::content_dirty::load_for_metadata(paths, volume, manifest.metadata_generation)?
        .is_some_and(|state| !state.ids.is_empty())
    {
        return Ok(None);
    }

    let materialized = MetadataIndex::build(loaded.delta.materialize_records(&base))?;
    let metadata_dir = store.join("metadata");
    fs::create_dir_all(&metadata_dir)?;
    let mut metadata_generation = manifest.metadata_generation.saturating_add(1).max(1);
    let metadata_file = loop {
        let name = format!("metadata-{metadata_generation:020}.prmet");
        let path = metadata_dir.join(&name);
        if !path.exists() {
            materialized.write_snapshot(&path)?;
            break name;
        }
        metadata_generation = metadata_generation.saturating_add(1);
    };

    let reused_content = super::content::reuse_complete_content_set_for_metadata_generation(
        paths,
        volume,
        extractor,
        &materialized,
        metadata_generation,
    )?;
    super::content_dirty::replace(paths, volume, metadata_generation, std::iter::empty())?;
    let next_manifest = VolumeManifest {
        generation: manifest.generation.saturating_add(1).max(1),
        key: manifest.key.clone(),
        mount: manifest.mount.clone(),
        phase: if reused_content {
            VolumePhase::Ready
        } else {
            VolumePhase::MetadataReady
        },
        metadata_generation,
        metadata_file: Some(metadata_file),
        metadata_records: materialized.records().len(),
        inaccessible_directories: manifest.inaccessible_directories,
    };
    write_volume_manifest(&store, &next_manifest)?;
    let generation = initialize_volume_incremental(paths, volume, loaded.state.checkpoint)?;
    gc_incremental_storage(paths, volume, 2)?;
    gc_metadata_snapshots(paths, volume, 2)?;
    Ok(Some((loaded.state.checkpoint, generation)))
}

pub(super) fn gc_incremental_storage(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
    keep_valid: usize,
) -> Result<usize> {
    let keep_valid = keep_valid.max(2);
    let store = incremental_dir(paths, volume);
    let mut pairs = numbered_files(&store, PAIR_PREFIX, PAIR_SUFFIX)?;
    pairs.sort_unstable_by_key(|(generation, _)| std::cmp::Reverse(*generation));
    let manifest = load_volume_manifest(&paths.volume_store(&volume.key))?;
    let current_metadata = manifest
        .as_ref()
        .map(|m| m.metadata_generation)
        .unwrap_or(0);
    let mut keep_pairs = std::collections::BTreeSet::new();
    let mut keep_delta = std::collections::BTreeSet::new();
    let mut keep_state = std::collections::BTreeSet::new();
    for (generation, path) in &pairs {
        if keep_pairs.len() >= keep_valid {
            break;
        }
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        let values = parse_key_values(&text);
        if parse_u64(&values, "version") != Some(PAIR_VERSION) {
            continue;
        }
        if parse_u64(&values, "metadata_generation") != Some(current_metadata) {
            continue;
        }
        let Some(delta) = parse_u64(&values, "delta_generation") else {
            continue;
        };
        let Some(state) = parse_u64(&values, "state_generation") else {
            continue;
        };
        if load_delta_generation(&store, delta).is_err()
            || load_state_generation(&store, state).is_err()
        {
            continue;
        }
        keep_pairs.insert(*generation);
        keep_delta.insert(delta);
        keep_state.insert(state);
    }
    let mut removed = 0;
    for (generation, path) in pairs {
        if !keep_pairs.contains(&generation) {
            let _ = fs::remove_file(path);
            removed += 1;
        }
    }
    for (generation, path) in numbered_files(&store, "delta-", ".prdelta")? {
        if !keep_delta.contains(&generation) {
            let _ = fs::remove_file(path);
            removed += 1;
        }
    }
    for (generation, path) in numbered_files(&store, "state-", ".princ")? {
        if !keep_state.contains(&generation) {
            let _ = fs::remove_file(path);
            removed += 1;
        }
    }
    Ok(removed)
}

fn gc_metadata_snapshots(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
    keep: usize,
) -> Result<usize> {
    let dir = paths.volume_store(&volume.key).join("metadata");
    let mut files = numbered_files(&dir, "metadata-", ".prmet")?;
    files.sort_unstable_by_key(|(generation, _)| std::cmp::Reverse(*generation));
    let mut removed = 0;
    for (_, path) in files.into_iter().skip(keep.max(2)) {
        fs::remove_file(path)?;
        removed += 1;
    }
    Ok(removed)
}

fn persist_volume_incremental(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
    manifest: &VolumeManifest,
    overlay: &DeltaOverlay,
    state: &IncrementalState,
) -> Result<u64> {
    let store = incremental_dir(paths, volume);
    fs::create_dir_all(&store)?;
    let generation = next_number(&store, PAIR_PREFIX, PAIR_SUFFIX)?;
    let mut snapshot: DeltaSnapshot = overlay.snapshot();
    snapshot.parent_generation = overlay.generation();
    snapshot.generation = generation;
    write_delta_generation(&store, &snapshot)?;
    let durable_state = IncrementalState {
        generation,
        checkpoint: state.checkpoint,
        pending_renames: state.pending_renames.clone(),
    };
    write_state_generation(&store, &durable_state)?;
    let pair = IncrementalPair {
        generation,
        metadata_generation: manifest.metadata_generation,
        delta_generation: generation,
        state_generation: generation,
    };
    write_pair(&store, &pair)?;
    Ok(generation)
}

fn write_pair(store: &Path, pair: &IncrementalPair) -> Result<()> {
    let content = format!(
        "version={PAIR_VERSION}\nmetadata_generation={}\ndelta_generation={}\nstate_generation={}\n",
        pair.metadata_generation, pair.delta_generation, pair.state_generation
    );
    atomic_write_new(
        &store.join(format!("{PAIR_PREFIX}{:020}{PAIR_SUFFIX}", pair.generation)),
        content.as_bytes(),
    )
}

fn load_manifest_metadata(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
    manifest: &VolumeManifest,
) -> Result<MetadataIndex> {
    let file = manifest.metadata_file.as_deref().ok_or_else(|| {
        super::AppError::InvalidState("incremental metadata requires published snapshot".into())
    })?;
    Ok(MetadataIndex::load_snapshot(
        paths.volume_store(&volume.key).join("metadata").join(file),
    )?)
}

#[cfg(any(windows, test))]
fn root_file_id(volume: &DiscoveredVolume) -> Result<u64> {
    let metadata = fs::symlink_metadata(&volume.mount)?;
    Ok(crate::product::platform_file_id(&volume.mount, &metadata)?)
}

#[cfg(windows)]
fn collect_excluded_file_ids(paths: &AppPaths, volume: &DiscoveredVolume) -> HashSet<u64> {
    if !paths.root.starts_with(&volume.mount) || !paths.root.exists() {
        return HashSet::new();
    }
    let mut out = HashSet::new();
    let mut stack = vec![paths.root.clone()];
    while let Some(path) = stack.pop() {
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if let Ok(file_id) = crate::product::platform_file_id(&path, &metadata) {
            out.insert(file_id);
        }
        if !metadata.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            stack.push(entry.path());
        }
    }
    out
}

#[cfg(any(windows, test))]
fn apply_normalized_changes(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
    base: &MetadataIndex,
    overlay: &mut DeltaOverlay,
    root_id: u64,
    excluded_ids: &HashSet<u64>,
    changes: &[NormalizedFsChange],
) -> Result<AppliedChanges> {
    let mut result = AppliedChanges::default();
    for change in changes {
        match change {
            NormalizedFsChange::ReconcileRequired => {
                result.requires_reconcile = true;
                break;
            }
            NormalizedFsChange::Delete { file_id } => {
                if excluded_ids.contains(file_id) {
                    continue;
                }
                if let Some(record) = overlay.current_record(base, *file_id).cloned() {
                    result.count = result.count.saturating_add(delete_record_tree(
                        base,
                        overlay,
                        &record,
                        &mut result.dirty_remove,
                    ));
                }
            }
            NormalizedFsChange::Modify { file_id } => {
                if excluded_ids.contains(file_id) {
                    continue;
                }
                let Some(previous) = overlay.current_record(base, *file_id).cloned() else {
                    result.requires_reconcile = true;
                    break;
                };
                let absolute = volume.mount.join(&previous.path);
                if absolute.starts_with(&paths.root) {
                    continue;
                }
                match metadata_record_for_existing_path(volume, &absolute)? {
                    Some(record) if record.file_id == *file_id => {
                        let content_changed = previous.content_searchable
                            != record.content_searchable
                            || previous.extractable != record.extractable
                            || previous.size != record.size
                            || previous.modified_ns != record.modified_ns;
                        let file_id = record.file_id;
                        let searchable = record.content_searchable;
                        overlay.upsert(base, record, content_changed);
                        if content_changed && searchable {
                            result.dirty_add.insert(file_id);
                        } else if content_changed {
                            result.dirty_remove.insert(file_id);
                        }
                        result.count = result.count.saturating_add(1);
                    }
                    Some(_) => {
                        result.requires_reconcile = true;
                        break;
                    }
                    None => {
                        overlay.delete(base, *file_id);
                        result.dirty_remove.insert(*file_id);
                        result.count = result.count.saturating_add(1);
                    }
                }
            }
            NormalizedFsChange::Create {
                file_id,
                parent_id,
                name,
                ..
            } => {
                if excluded_ids.contains(file_id) || excluded_ids.contains(parent_id) {
                    continue;
                }
                let Some(parent) = resolve_parent_path(base, overlay, *parent_id, root_id) else {
                    result.requires_reconcile = true;
                    break;
                };
                let absolute_parent = volume.mount.join(&parent);
                if absolute_parent.starts_with(&paths.root) {
                    continue;
                }
                if !parent.as_os_str().is_empty()
                    && directory_is_reparse_point(&absolute_parent).unwrap_or(false)
                {
                    continue;
                }
                let absolute = absolute_parent.join(name);
                if absolute.starts_with(&paths.root) {
                    continue;
                }
                match metadata_record_for_existing_path(volume, &absolute)? {
                    Some(record) if record.file_id == *file_id => {
                        let content_changed = record.content_searchable;
                        let file_id = record.file_id;
                        let searchable = record.content_searchable;
                        overlay.upsert(base, record, content_changed);
                        if content_changed && searchable {
                            result.dirty_add.insert(file_id);
                        } else if content_changed {
                            result.dirty_remove.insert(file_id);
                        }
                        result.count = result.count.saturating_add(1);
                    }
                    Some(_) => {
                        result.requires_reconcile = true;
                        break;
                    }
                    None => {}
                }
            }
            NormalizedFsChange::Rename {
                file_id,
                new_parent_id,
                new_name,
                ..
            } => {
                let previous = overlay.current_record(base, *file_id).cloned();
                if excluded_ids.contains(file_id) || excluded_ids.contains(new_parent_id) {
                    if let Some(previous) = previous.as_ref() {
                        result.count = result.count.saturating_add(delete_record_tree(
                            base,
                            overlay,
                            previous,
                            &mut result.dirty_remove,
                        ));
                    }
                    continue;
                }
                let Some(previous) = previous else {
                    result.requires_reconcile = true;
                    break;
                };
                let Some(parent) = resolve_parent_path(base, overlay, *new_parent_id, root_id)
                else {
                    result.requires_reconcile = true;
                    break;
                };
                let absolute_parent = volume.mount.join(&parent);
                if absolute_parent.starts_with(&paths.root) {
                    result.count = result.count.saturating_add(delete_record_tree(
                        base,
                        overlay,
                        &previous,
                        &mut result.dirty_remove,
                    ));
                    continue;
                }
                if !parent.as_os_str().is_empty()
                    && directory_is_reparse_point(&absolute_parent).unwrap_or(false)
                {
                    result.count = result.count.saturating_add(delete_record_tree(
                        base,
                        overlay,
                        &previous,
                        &mut result.dirty_remove,
                    ));
                    continue;
                }
                let new_absolute = absolute_parent.join(new_name);
                if new_absolute.starts_with(&paths.root) {
                    result.count = result.count.saturating_add(delete_record_tree(
                        base,
                        overlay,
                        &previous,
                        &mut result.dirty_remove,
                    ));
                    continue;
                }
                let Some(updated) = metadata_record_for_existing_path(volume, &new_absolute)?
                else {
                    result.count = result.count.saturating_add(delete_record_tree(
                        base,
                        overlay,
                        &previous,
                        &mut result.dirty_remove,
                    ));
                    continue;
                };
                if updated.file_id != *file_id {
                    result.requires_reconcile = true;
                    break;
                }

                let descendants = if previous.kind == MetadataFileKind::Directory {
                    overlay
                        .materialize_records(base)
                        .into_iter()
                        .filter(|candidate| {
                            candidate.file_id != *file_id
                                && candidate.path.starts_with(&previous.path)
                        })
                        .collect::<Vec<_>>()
                } else {
                    Vec::new()
                };
                let content_changed = previous.content_searchable != updated.content_searchable
                    || previous.extractable != updated.extractable
                    || previous.size != updated.size
                    || previous.modified_ns != updated.modified_ns;
                let new_root = updated.path.clone();
                let updated_id = updated.file_id;
                let searchable = updated.content_searchable;
                overlay.upsert(base, updated, content_changed);
                if content_changed && searchable {
                    result.dirty_add.insert(updated_id);
                } else if content_changed {
                    result.dirty_remove.insert(updated_id);
                }
                result.count = result.count.saturating_add(1);
                for mut descendant in descendants {
                    let Ok(suffix) = descendant.path.strip_prefix(&previous.path) else {
                        result.requires_reconcile = true;
                        break;
                    };
                    descendant.path = new_root.join(suffix);
                    overlay.upsert(base, descendant, false);
                    result.count = result.count.saturating_add(1);
                }
                if result.requires_reconcile {
                    break;
                }
            }
        }
    }
    Ok(result)
}

#[cfg(any(windows, test))]
fn delete_record_tree(
    base: &MetadataIndex,
    overlay: &mut DeltaOverlay,
    record: &crate::metadata::MetadataRecord,
    dirty_remove: &mut std::collections::BTreeSet<u64>,
) -> usize {
    let mut deleted = 0_usize;
    if record.kind == MetadataFileKind::Directory {
        let descendants = overlay
            .materialize_records(base)
            .into_iter()
            .filter(|candidate| {
                candidate.file_id != record.file_id && candidate.path.starts_with(&record.path)
            })
            .map(|candidate| candidate.file_id)
            .collect::<Vec<_>>();
        for descendant in descendants {
            overlay.delete(base, descendant);
            dirty_remove.insert(descendant);
            deleted = deleted.saturating_add(1);
        }
    }
    overlay.delete(base, record.file_id);
    dirty_remove.insert(record.file_id);
    deleted.saturating_add(1)
}

#[cfg(any(windows, test))]
fn resolve_parent_path(
    base: &MetadataIndex,
    overlay: &DeltaOverlay,
    parent_id: u64,
    root_id: u64,
) -> Option<PathBuf> {
    if parent_id == root_id {
        return Some(PathBuf::new());
    }
    let record = overlay.current_record(base, parent_id)?;
    (record.kind == MetadataFileKind::Directory).then(|| record.path.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{AppPaths, VolumeKey, build_or_resume_metadata};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "personalrag-incremental-runtime-{name}-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn volume(root: &Path) -> DiscoveredVolume {
        DiscoveredVolume {
            key: VolumeKey("incremental-runtime-test".into()),
            mount: root.to_path_buf(),
            serial: 1,
        }
    }

    fn build_base(paths: &AppPaths, volume: &DiscoveredVolume) -> (VolumeManifest, MetadataIndex) {
        let mut never_stop = || false;
        build_or_resume_metadata(
            paths,
            volume,
            std::slice::from_ref(&paths.root),
            2,
            &mut never_stop,
        )
        .unwrap();
        let manifest = load_volume_manifest(&paths.volume_store(&volume.key))
            .unwrap()
            .unwrap();
        let metadata = load_manifest_metadata(paths, volume, &manifest).unwrap();
        (manifest, metadata)
    }

    fn file_id(path: &Path) -> u64 {
        let metadata = fs::symlink_metadata(path).unwrap();
        crate::product::platform_file_id(path, &metadata).unwrap()
    }

    #[test]
    fn catch_up_cursor_reuses_only_monotonic_position_in_same_journal() {
        let durable = UsnCheckpoint {
            journal_id: 7,
            next_usn: 100,
        };
        let mut cursor = CatchUpCursor::default();
        assert!(!cursor.usable_for(durable, 3, 200));

        cursor.reset(durable, 3);
        assert!(cursor.usable_for(durable, 3, 200));
        cursor.next_usn = 150;
        assert!(cursor.usable_for(durable, 3, 200));
        assert!(!cursor.usable_for(durable, 3, 140));
        assert!(!cursor.usable_for(durable, 2, 200));
        assert!(!cursor.usable_for(
            UsnCheckpoint {
                journal_id: 8,
                next_usn: 100,
            },
            3,
            200,
        ));
        assert!(!cursor.usable_for(
            UsnCheckpoint {
                journal_id: 7,
                next_usn: 160,
            },
            3,
            200,
        ));
    }

    #[test]
    fn incremental_pair_falls_back_to_previous_valid_generation() {
        let base = temp_dir("fallback");
        let root = base.join("root");
        let app_root = base.join("app");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "a").unwrap();
        let paths = AppPaths::for_root(&app_root);
        paths.ensure().unwrap();
        let volume = volume(&root);
        let (manifest, metadata) = build_base(&paths, &volume);
        let checkpoint = UsnCheckpoint {
            journal_id: 7,
            next_usn: 11,
        };
        initialize_volume_incremental(&paths, &volume, checkpoint).unwrap();
        let mut loaded = load_volume_incremental(&paths, &volume, &manifest)
            .unwrap()
            .unwrap();
        let record = metadata.records()[0].clone();
        loaded.delta.upsert(&metadata, record, false);
        persist_volume_incremental(&paths, &volume, &manifest, &loaded.delta, &loaded.state)
            .unwrap();
        let inc = incremental_dir(&paths, &volume);
        let latest = numbered_files(&inc, PAIR_PREFIX, PAIR_SUFFIX)
            .unwrap()
            .into_iter()
            .max_by_key(|value| value.0)
            .unwrap();
        fs::write(latest.1, "broken").unwrap();
        let fallback = load_volume_incremental(&paths, &volume, &manifest)
            .unwrap()
            .unwrap();
        assert_eq!(fallback.state.checkpoint, checkpoint);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn delta_generation_lineage_preserves_step4_parent_semantics() {
        let base = temp_dir("lineage");
        let root = base.join("root");
        let app_root = base.join("app");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "a").unwrap();
        let paths = AppPaths::for_root(&app_root);
        paths.ensure().unwrap();
        let volume = volume(&root);
        let (manifest, metadata) = build_base(&paths, &volume);
        initialize_volume_incremental(
            &paths,
            &volume,
            UsnCheckpoint {
                journal_id: 1,
                next_usn: 10,
            },
        )
        .unwrap();
        let first = load_volume_incremental(&paths, &volume, &manifest)
            .unwrap()
            .unwrap();
        assert_eq!(first.delta.generation(), 1);
        assert_eq!(first.delta.parent_generation(), 0);

        let mut overlay = first.delta;
        overlay.upsert(&metadata, metadata.records()[0].clone(), false);
        let second_generation =
            persist_volume_incremental(&paths, &volume, &manifest, &overlay, &first.state).unwrap();
        let second_snapshot =
            load_delta_generation(incremental_dir(&paths, &volume), second_generation).unwrap();
        assert_eq!(second_snapshot.parent_generation, 1);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn normalized_changes_update_overlay_without_full_rescan() {
        let base_dir = temp_dir("changes");
        let root = base_dir.join("root");
        let app_root = base_dir.join("app");
        fs::create_dir_all(root.join("dir")).unwrap();
        fs::write(root.join("dir/a.txt"), "old").unwrap();
        let paths = AppPaths::for_root(&app_root);
        paths.ensure().unwrap();
        let volume = volume(&root);
        let (_manifest, metadata) = build_base(&paths, &volume);
        let root_id = root_file_id(&volume).unwrap();
        let dir_id = file_id(&root.join("dir"));
        let file_id_value = file_id(&root.join("dir/a.txt"));
        let mut overlay = DeltaOverlay::new(&metadata, 1, 1);

        fs::write(root.join("dir/a.txt"), "new-content").unwrap();
        let modified = apply_normalized_changes(
            &paths,
            &volume,
            &metadata,
            &mut overlay,
            root_id,
            &HashSet::new(),
            &[NormalizedFsChange::Modify {
                file_id: file_id_value,
            }],
        )
        .unwrap();
        assert_eq!(modified.count, 1);
        assert!(
            overlay
                .snapshot()
                .upserts
                .iter()
                .any(|value| value.record.file_id == file_id_value && value.content_changed)
        );

        fs::rename(root.join("dir"), root.join("moved")).unwrap();
        let renamed = apply_normalized_changes(
            &paths,
            &volume,
            &metadata,
            &mut overlay,
            root_id,
            &HashSet::new(),
            &[NormalizedFsChange::Rename {
                file_id: dir_id,
                old_parent_id: root_id,
                old_name: "dir".into(),
                new_parent_id: root_id,
                new_name: "moved".into(),
            }],
        )
        .unwrap();
        assert!(!renamed.requires_reconcile);
        let records = overlay.materialize_records(&metadata);
        assert!(
            records
                .iter()
                .any(|value| value.path == Path::new("moved/a.txt"))
        );
        assert!(
            !records
                .iter()
                .any(|value| value.path == Path::new("dir/a.txt"))
        );
        fs::remove_dir_all(base_dir).unwrap();
    }
    #[test]
    fn dirty_queue_excludes_old_shard_even_when_fingerprint_matches() {
        use crate::SearchLimits;
        use crate::app::{ContentBuildOptions, FederatedContentIndex, build_or_resume_content};
        use crate::extraction::ExtractorConfig;
        use crate::incremental::ContentQueryKind;

        let base_dir = temp_dir("dirty-fingerprint");
        let root = base_dir.join("root");
        let app_root = base_dir.join("app");
        fs::create_dir_all(&root).unwrap();
        let file = root.join("a.txt");
        fs::write(&file, "old-needle\n").unwrap();
        let paths = AppPaths::for_root(&app_root);
        paths.ensure().unwrap();
        let volume = volume(&root);
        let (manifest, metadata) = build_base(&paths, &volume);
        let extractor = ExtractorConfig::discover();
        let mut never_stop = || false;
        build_or_resume_content(
            &paths,
            &volume,
            &extractor,
            ContentBuildOptions::default(),
            &mut never_stop,
        )
        .unwrap();

        initialize_volume_incremental(
            &paths,
            &volume,
            UsnCheckpoint {
                journal_id: 5,
                next_usn: 50,
            },
        )
        .unwrap();
        let mut loaded = load_volume_incremental(&paths, &volume, &manifest)
            .unwrap()
            .unwrap();
        let unchanged_fingerprint = metadata.records()[0].clone();
        loaded.delta.upsert(&metadata, unchanged_fingerprint, true);
        persist_volume_incremental(&paths, &volume, &manifest, &loaded.delta, &loaded.state)
            .unwrap();
        super::super::content_dirty::merge(
            &paths,
            &volume,
            manifest.metadata_generation,
            [metadata.records()[0].file_id],
            std::iter::empty(),
        )
        .unwrap();

        let content =
            FederatedContentIndex::load(&paths, std::slice::from_ref(&volume), &extractor).unwrap();
        assert!(
            content
                .search(
                    ContentQueryKind::Literal("old-needle"),
                    false,
                    SearchLimits::default(),
                )
                .unwrap()
                .is_empty()
        );
        fs::remove_dir_all(base_dir).unwrap();
    }

    #[test]
    fn federated_search_consumes_metadata_delta_and_excludes_stale_content() {
        use crate::SearchLimits;
        use crate::app::{
            ContentBuildOptions, FederatedContentIndex, FederatedMetadataIndex,
            build_or_resume_content,
        };
        use crate::extraction::ExtractorConfig;
        use crate::incremental::ContentQueryKind;

        let base_dir = temp_dir("federated-delta");
        let root = base_dir.join("root");
        let app_root = base_dir.join("app");
        fs::create_dir_all(root.join("dir")).unwrap();
        let file = root.join("dir/a.txt");
        fs::write(&file, "old-needle\n").unwrap();
        let paths = AppPaths::for_root(&app_root);
        paths.ensure().unwrap();
        let volume = volume(&root);
        let (manifest, metadata) = build_base(&paths, &volume);
        let extractor = ExtractorConfig::discover();
        let mut never_stop = || false;
        build_or_resume_content(
            &paths,
            &volume,
            &extractor,
            ContentBuildOptions::default(),
            &mut never_stop,
        )
        .unwrap();

        initialize_volume_incremental(
            &paths,
            &volume,
            UsnCheckpoint {
                journal_id: 9,
                next_usn: 100,
            },
        )
        .unwrap();
        let file_id_value = file_id(&file);
        let root_id = root_file_id(&volume).unwrap();
        let mut loaded = load_volume_incremental(&paths, &volume, &manifest)
            .unwrap()
            .unwrap();

        fs::write(&file, "new-content-is-longer\n").unwrap();
        let applied = apply_normalized_changes(
            &paths,
            &volume,
            &metadata,
            &mut loaded.delta,
            root_id,
            &HashSet::new(),
            &[NormalizedFsChange::Modify {
                file_id: file_id_value,
            }],
        )
        .unwrap();
        assert_eq!(applied.count, 1);
        persist_volume_incremental(&paths, &volume, &manifest, &loaded.delta, &loaded.state)
            .unwrap();

        let federated =
            FederatedMetadataIndex::load(&paths, std::slice::from_ref(&volume)).unwrap();
        let hits = federated.search(Some("a.txt"), None, false, 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].record.size, "new-content-is-longer\n".len() as u64);

        let content =
            FederatedContentIndex::load(&paths, std::slice::from_ref(&volume), &extractor).unwrap();
        assert!(
            content
                .search(
                    ContentQueryKind::Literal("old-needle"),
                    false,
                    SearchLimits::default(),
                )
                .unwrap()
                .is_empty()
        );
        assert!(
            content
                .search(
                    ContentQueryKind::Literal("new-content-is-longer"),
                    false,
                    SearchLimits::default(),
                )
                .unwrap()
                .is_empty()
        );

        fs::remove_dir_all(base_dir).unwrap();
    }

    #[test]
    fn metadata_compaction_materializes_delta_and_resets_incremental_state() {
        let base = temp_dir("metadata-compaction");
        let root = base.join("root");
        let app_root = base.join("app");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "alpha").unwrap();
        let paths = AppPaths::for_root(&app_root);
        paths.ensure().unwrap();
        let volume = volume(&root);
        let (manifest, metadata) = build_base(&paths, &volume);
        let checkpoint = UsnCheckpoint {
            journal_id: 9,
            next_usn: 100,
        };
        initialize_volume_incremental(&paths, &volume, checkpoint).unwrap();
        let mut loaded = load_volume_incremental(&paths, &volume, &manifest)
            .unwrap()
            .unwrap();
        let mut record = metadata.records()[0].clone();
        record.path = PathBuf::from("renamed.txt");
        loaded.delta.upsert(&metadata, record, false);
        persist_volume_incremental(&paths, &volume, &manifest, &loaded.delta, &loaded.state)
            .unwrap();

        let compacted = compact_volume_metadata_if_needed(
            &paths,
            &volume,
            &crate::extraction::ExtractorConfig::discover(),
        )
        .unwrap();
        assert!(compacted.is_some());
        let next_manifest = load_volume_manifest(&paths.volume_store(&volume.key))
            .unwrap()
            .unwrap();
        assert!(next_manifest.metadata_generation > manifest.metadata_generation);
        let next_metadata = load_manifest_metadata(&paths, &volume, &next_manifest).unwrap();
        assert_eq!(next_metadata.records()[0].path, Path::new("renamed.txt"));
        let status = incremental_metadata_status(&paths, &volume).unwrap();
        assert_eq!(status.change_count, 0);
        assert_eq!(status.content_dirty_files, 0);
        fs::remove_dir_all(base).unwrap();
    }
}

#[cfg(test)]
mod step8_dirty_status_tests {
    use super::*;
    use crate::app::{AppPaths, DiscoveredVolume, VolumeKey, build_or_resume_metadata};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn dirty_status_is_visible_without_usn_incremental_pair() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("personalrag-dirty-no-pair-{stamp}"));
        let root = base.join("root");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "alpha").unwrap();
        let paths = AppPaths::for_root(base.join("app"));
        paths.ensure().unwrap();
        let volume = DiscoveredVolume {
            key: VolumeKey("dirty-no-pair".into()),
            mount: root,
            serial: 1,
        };
        let mut never_stop = || false;
        build_or_resume_metadata(
            &paths,
            &volume,
            std::slice::from_ref(&paths.root),
            2,
            &mut never_stop,
        )
        .unwrap();
        let manifest = load_volume_manifest(&paths.volume_store(&volume.key))
            .unwrap()
            .unwrap();
        super::super::content_dirty::replace(&paths, &volume, manifest.metadata_generation, [42])
            .unwrap();
        let status = incremental_metadata_status(&paths, &volume).unwrap();
        assert_eq!(status.change_count, 0);
        assert_eq!(status.content_dirty_files, 1);
        assert!(status.content_dirty);
        fs::remove_dir_all(base).unwrap();
    }
}
