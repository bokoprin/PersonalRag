use super::super::content::{
    catch_up_dirty_content_step, compact_content_if_needed,
    reuse_complete_content_set_for_metadata_generation_with_dirty,
};
use super::super::incremental_runtime::{
    CatchUpCursor, CatchUpResult, capture_volume_checkpoint, catch_up_volume,
    compact_volume_metadata_if_needed, incremental_metadata_status, initialize_volume_incremental,
    materialized_volume_metadata,
};
use super::super::volume::recover_volume_manifest;
use super::super::{
    AppError, AppPaths, ContentBuildOptions, DiscoveredVolume, METADATA_CHECKPOINT_RECORDS,
    StartupAction, VolumeKey, VolumeManifest, VolumePhase, begin_metadata_refresh,
    build_content_step, build_content_step_trusted, build_or_resume_metadata, content_progress,
    determine_startup_action, load_volume_manifest, validated_content_progress,
    write_volume_manifest,
};
use super::snapshot::{SharedSnapshot, publish_snapshot};
use super::watcher::RuntimeWatchers;
use crate::extraction::ExtractorConfig;
use crate::metadata::MetadataIndex;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const WATCH_POLL_INTERVAL: Duration = Duration::from_millis(250);
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(30);
const CONTENT_COMPACTION_SHARDS: usize = 32;

pub(super) fn run_coordinator(
    paths: AppPaths,
    volumes: Vec<DiscoveredVolume>,
    extractor: ExtractorConfig,
    snapshot: SharedSnapshot,
    stop: std::sync::Arc<AtomicBool>,
    watch_changes: bool,
) {
    let mut errors = HashMap::<VolumeKey, String>::new();
    let mut watchers = RuntimeWatchers::new(&volumes, watch_changes);
    let mut validated_content = HashSet::<VolumeKey>::new();
    let mut catch_up_cursors = HashMap::<VolumeKey, CatchUpCursor>::new();

    for volume in &volumes {
        if let Err(error) = recover_volume_manifest(&paths.volume_store(&volume.key)) {
            errors.insert(volume.key.clone(), error.to_string());
            publish_snapshot(&paths, &volumes, &snapshot, &errors, true);
            continue;
        }
        if stop.load(Ordering::Acquire) {
            publish_snapshot(&paths, &volumes, &snapshot, &errors, false);
            return;
        }
        let action = match determine_startup_action(&paths, volume, &extractor, watch_changes) {
            Ok(action) => action,
            Err(error) => {
                errors.insert(volume.key.clone(), error.to_string());
                publish_snapshot(&paths, &volumes, &snapshot, &errors, true);
                continue;
            }
        };
        let cursor = catch_up_cursors.entry(volume.key.clone()).or_default();
        let result = handle_startup_action(
            &paths,
            volume,
            &extractor,
            &stop,
            watch_changes,
            action,
            cursor,
        );
        match result {
            Ok(()) => {
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
            validated_content.remove(&volume.key);
            let cursor = catch_up_cursors.entry(volume.key.clone()).or_default();
            refresh_volume_changes(&paths, volume, &extractor, &stop, cursor, &mut errors);
            publish_snapshot(&paths, &volumes, &snapshot, &errors, true);
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
                validated_content.remove(&volume.key);
                let cursor = catch_up_cursors.entry(volume.key.clone()).or_default();
                refresh_volume_changes(&paths, volume, &extractor, &stop, cursor, &mut errors);
                publish_snapshot(&paths, &volumes, &snapshot, &errors, true);
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

            let lightweight = content_progress(&paths, volume);
            let appears_complete =
                matches!(lightweight, Ok(Some(ref progress)) if progress.complete);
            let dirty_files = incremental_metadata_status(&paths, volume)
                .map(|status| status.content_dirty_files)
                .unwrap_or(0);
            if appears_complete && dirty_files > 0 {
                all_terminal = false;
                match catch_up_dirty_content_step(&paths, volume, &extractor, options) {
                    Ok(_) => {
                        errors.remove(&volume.key);
                        made_progress = true;
                        thread::yield_now();
                    }
                    Err(error) => {
                        errors.insert(volume.key.clone(), error.to_string());
                    }
                }
                publish_snapshot(&paths, &volumes, &snapshot, &errors, true);
                continue;
            }

            if appears_complete && !validated_content.contains(&volume.key) {
                match validated_content_progress(&paths, volume, &extractor) {
                    Ok(Some(progress)) if progress.complete => {
                        validated_content.insert(volume.key.clone());
                        continue;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        all_terminal = false;
                        errors.insert(volume.key.clone(), error.to_string());
                        continue;
                    }
                }
            } else if appears_complete {
                continue;
            }

            if let Err(error) = lightweight {
                all_terminal = false;
                errors.insert(volume.key.clone(), error.to_string());
                continue;
            }

            all_terminal = false;
            let result = if validated_content.contains(&volume.key) {
                build_content_step_trusted(&paths, volume, &extractor, options)
            } else {
                build_content_step(&paths, volume, &extractor, options)
            };
            match result {
                Ok(_) => {
                    validated_content.insert(volume.key.clone());
                    errors.remove(&volume.key);
                    made_progress = true;
                    thread::yield_now();
                }
                Err(error) => {
                    errors.insert(volume.key.clone(), error.to_string());
                }
            }
            publish_snapshot(&paths, &volumes, &snapshot, &errors, true);
        }

        if all_terminal {
            break;
        }
        if !made_progress {
            thread::sleep(WATCH_POLL_INTERVAL);
        }
    }

    publish_snapshot(&paths, &volumes, &snapshot, &errors, true);

    let mut last_maintenance = Instant::now();
    while !stop.load(Ordering::Acquire) {
        let mut did_work = false;
        for volume in &volumes {
            if stop.load(Ordering::Acquire) {
                break;
            }
            if watchers.should_refresh(volume) {
                validated_content.remove(&volume.key);
                let cursor = catch_up_cursors.entry(volume.key.clone()).or_default();
                refresh_volume_changes(&paths, volume, &extractor, &stop, cursor, &mut errors);
                publish_snapshot(&paths, &volumes, &snapshot, &errors, true);
                watchers.reopen(volume);
                did_work = true;
            }

            match incremental_metadata_status(&paths, volume) {
                Ok(status) if status.content_dirty_files > 0 => {
                    match catch_up_dirty_content_step(&paths, volume, &extractor, options) {
                        Ok(_) => {
                            errors.remove(&volume.key);
                            did_work = true;
                            thread::yield_now();
                        }
                        Err(error) => {
                            errors.insert(volume.key.clone(), error.to_string());
                        }
                    }
                    publish_snapshot(&paths, &volumes, &snapshot, &errors, true);
                }
                Ok(_) => {}
                Err(error) => {
                    errors.insert(volume.key.clone(), error.to_string());
                }
            }
        }

        if last_maintenance.elapsed() >= MAINTENANCE_INTERVAL {
            for volume in &volumes {
                if stop.load(Ordering::Acquire) {
                    break;
                }
                if let Some(cursor) = catch_up_cursors.get_mut(&volume.key) {
                    match compact_volume_metadata_if_needed(&paths, volume, &extractor) {
                        Ok(Some((checkpoint, generation))) => {
                            cursor.reset(checkpoint, generation);
                            did_work = true;
                        }
                        Ok(None) => {}
                        Err(error) => {
                            errors.insert(volume.key.clone(), error.to_string());
                            continue;
                        }
                    }
                }
                match compact_content_if_needed(
                    &paths,
                    volume,
                    &extractor,
                    CONTENT_COMPACTION_SHARDS,
                ) {
                    Ok(true) => did_work = true,
                    Ok(false) => {}
                    Err(error) => {
                        errors.insert(volume.key.clone(), error.to_string());
                    }
                }
            }
            last_maintenance = Instant::now();
            publish_snapshot(&paths, &volumes, &snapshot, &errors, true);
        }

        if !did_work {
            thread::sleep(WATCH_POLL_INTERVAL);
        }
    }

    publish_snapshot(&paths, &volumes, &snapshot, &errors, false);
}

fn handle_startup_action(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
    extractor: &ExtractorConfig,
    stop: &AtomicBool,
    watch_changes: bool,
    action: StartupAction,
    cursor: &mut CatchUpCursor,
) -> super::super::Result<()> {
    match action {
        StartupAction::FreshMetadataBuild => {
            build_fresh_metadata(paths, volume, stop, watch_changes, cursor)
        }
        StartupAction::ResumeMetadataBuild => {
            let complete = run_metadata_build(paths, volume, stop)?;
            if complete && watch_changes && !stop.load(Ordering::Acquire) {
                reconcile_volume_metadata(paths, volume, extractor, stop, true, cursor)?;
            }
            Ok(())
        }
        StartupAction::CatchUpChanges => {
            catch_up_or_reconcile(paths, volume, extractor, stop, cursor)
        }
        StartupAction::Reconcile => {
            reconcile_volume_metadata(paths, volume, extractor, stop, watch_changes, cursor)
        }
        StartupAction::ResumeContentBuild | StartupAction::Ready => Ok(()),
    }
}

fn build_fresh_metadata(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
    stop: &AtomicBool,
    watch_changes: bool,
    cursor: &mut CatchUpCursor,
) -> super::super::Result<()> {
    let checkpoint = watch_changes
        .then(|| capture_volume_checkpoint(volume))
        .flatten();
    let complete = run_metadata_build(paths, volume, stop)?;
    if !complete || stop.load(Ordering::Acquire) {
        return Ok(());
    }
    if let Some(checkpoint) = checkpoint {
        let generation = initialize_volume_incremental(paths, volume, checkpoint)?;
        cursor.reset(checkpoint, generation);
        if let CatchUpResult::NeedsReconcile { .. } = catch_up_volume(paths, volume, cursor)? {
            return Err(AppError::InvalidState(
                "fresh metadata catch-up unexpectedly requires reconcile".into(),
            ));
        }
    }
    Ok(())
}

fn catch_up_or_reconcile(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
    extractor: &ExtractorConfig,
    stop: &AtomicBool,
    cursor: &mut CatchUpCursor,
) -> super::super::Result<()> {
    match catch_up_volume(paths, volume, cursor)? {
        CatchUpResult::NoChanges { .. } | CatchUpResult::Applied { .. } => Ok(()),
        CatchUpResult::NeedsReconcile { .. } => {
            reconcile_volume_metadata(paths, volume, extractor, stop, true, cursor)
        }
    }
}

fn reconcile_volume_metadata(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
    extractor: &ExtractorConfig,
    stop: &AtomicBool,
    watch_changes: bool,
    cursor: &mut CatchUpCursor,
) -> super::super::Result<()> {
    let attempts = if watch_changes { 2 } else { 1 };
    for attempt in 0..attempts {
        let old_manifest = load_volume_manifest(&paths.volume_store(&volume.key))?;
        let old_metadata = materialized_volume_metadata(paths, volume).ok().flatten();
        let carried_dirty = old_manifest
            .as_ref()
            .and_then(|manifest| {
                super::super::content_dirty::load_for_metadata(
                    paths,
                    volume,
                    manifest.metadata_generation,
                )
                .ok()
                .flatten()
            })
            .map(|state| state.ids)
            .unwrap_or_default();
        let checkpoint = watch_changes
            .then(|| capture_volume_checkpoint(volume))
            .flatten();
        begin_metadata_refresh(paths, volume)?;
        let complete = run_metadata_build(paths, volume, stop)?;
        if !complete || stop.load(Ordering::Acquire) {
            return Ok(());
        }
        prepare_content_after_metadata_refresh(
            paths,
            volume,
            extractor,
            old_metadata.as_ref(),
            &carried_dirty,
        )?;
        let Some(checkpoint) = checkpoint else {
            return Ok(());
        };
        let generation = initialize_volume_incremental(paths, volume, checkpoint)?;
        cursor.reset(checkpoint, generation);
        match catch_up_volume(paths, volume, cursor)? {
            CatchUpResult::NoChanges { .. } | CatchUpResult::Applied { .. } => return Ok(()),
            CatchUpResult::NeedsReconcile { reason } if attempt + 1 < attempts => {
                let _ = reason;
            }
            CatchUpResult::NeedsReconcile { reason } => {
                return Err(AppError::InvalidState(format!(
                    "USN catch-up still requires reconcile after metadata refresh: {reason}"
                )));
            }
        }
    }
    Ok(())
}

fn prepare_content_after_metadata_refresh(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
    extractor: &ExtractorConfig,
    old_metadata: Option<&MetadataIndex>,
    carried_dirty: &BTreeSet<u64>,
) -> super::super::Result<()> {
    let store = paths.volume_store(&volume.key);
    let manifest = load_volume_manifest(&store)?.ok_or_else(|| {
        AppError::InvalidState("metadata refresh completed without manifest".into())
    })?;
    let file = manifest.metadata_file.as_deref().ok_or_else(|| {
        AppError::InvalidState("metadata refresh completed without snapshot".into())
    })?;
    let metadata = MetadataIndex::load_snapshot(store.join("metadata").join(file))?;
    if old_metadata.is_none() {
        super::super::content_dirty::replace(
            paths,
            volume,
            manifest.metadata_generation,
            std::iter::empty(),
        )?;
        return Ok(());
    }
    let dirty = super::super::content_dirty::reconcile_after_metadata_refresh(
        paths,
        volume,
        old_metadata,
        carried_dirty,
        &metadata,
        manifest.metadata_generation,
    )?;
    let reused = reuse_complete_content_set_for_metadata_generation_with_dirty(
        paths,
        volume,
        extractor,
        &metadata,
        manifest.metadata_generation,
        &dirty.ids,
    )?;
    if !reused {
        super::super::content_dirty::replace(
            paths,
            volume,
            manifest.metadata_generation,
            std::iter::empty(),
        )?;
        return Ok(());
    }
    let next = VolumeManifest {
        generation: manifest.generation.saturating_add(1).max(1),
        key: manifest.key.clone(),
        mount: manifest.mount.clone(),
        phase: if dirty.ids.is_empty() {
            VolumePhase::Ready
        } else {
            VolumePhase::ContentCatchUp
        },
        metadata_generation: manifest.metadata_generation,
        metadata_file: manifest.metadata_file.clone(),
        metadata_records: manifest.metadata_records,
        inaccessible_directories: manifest.inaccessible_directories,
    };
    write_volume_manifest(&store, &next)?;
    Ok(())
}

fn run_metadata_build(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
    stop: &AtomicBool,
) -> super::super::Result<bool> {
    let mut should_stop = || stop.load(Ordering::Acquire);
    let report = build_or_resume_metadata(
        paths,
        volume,
        std::slice::from_ref(&paths.root),
        METADATA_CHECKPOINT_RECORDS,
        &mut should_stop,
    )?;
    Ok(report.complete)
}

fn refresh_volume_changes(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
    extractor: &ExtractorConfig,
    stop: &AtomicBool,
    cursor: &mut CatchUpCursor,
    errors: &mut HashMap<VolumeKey, String>,
) {
    match catch_up_or_reconcile(paths, volume, extractor, stop, cursor) {
        Ok(()) => {
            errors.remove(&volume.key);
        }
        Err(error) => {
            errors.insert(volume.key.clone(), error.to_string());
        }
    }
}
