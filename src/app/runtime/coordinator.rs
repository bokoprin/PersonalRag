use super::super::{
    AppPaths, ContentBuildOptions, DiscoveredVolume, METADATA_CHECKPOINT_RECORDS, StartupAction,
    VolumeKey, begin_metadata_refresh, build_content_step, build_content_step_trusted,
    build_or_resume_metadata, content_progress, determine_startup_action, load_volume_manifest,
    validated_content_progress,
};
use super::snapshot::{SharedSnapshot, publish_snapshot};
use super::watcher::RuntimeWatchers;
use crate::extraction::ExtractorConfig;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

const WATCH_POLL_INTERVAL: Duration = Duration::from_millis(250);

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

    for volume in &volumes {
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
        let result = match action {
            StartupAction::FreshMetadataBuild | StartupAction::ResumeMetadataBuild => {
                run_metadata_build(&paths, volume, &stop)
            }
            StartupAction::Reconcile => begin_metadata_refresh(&paths, volume)
                .and_then(|_| run_metadata_build(&paths, volume, &stop)),
            StartupAction::ResumeContentBuild | StartupAction::Ready => Ok(()),
        };
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
            refresh_volume_metadata(&paths, volume, &stop, &mut errors, &snapshot, &volumes);
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
                refresh_volume_metadata(&paths, volume, &stop, &mut errors, &snapshot, &volumes);
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
                Ok(report) => {
                    validated_content.insert(volume.key.clone());
                    errors.remove(&volume.key);
                    made_progress = true;
                    if report.complete {
                        // The next pass can use the already-validated state without reloading all shards.
                    }
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

    while !stop.load(Ordering::Acquire) {
        let mut refreshed = false;
        for volume in &volumes {
            if stop.load(Ordering::Acquire) {
                break;
            }
            if !watchers.should_refresh(volume) {
                continue;
            }
            refresh_volume_metadata(&paths, volume, &stop, &mut errors, &snapshot, &volumes);
            watchers.reopen(volume);
            refreshed = true;

            if stop.load(Ordering::Acquire) {
                break;
            }
            let mut validate_state = true;
            loop {
                let result = if validate_state {
                    build_content_step(&paths, volume, &extractor, options)
                } else {
                    build_content_step_trusted(&paths, volume, &extractor, options)
                };
                validate_state = false;
                match result {
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

fn run_metadata_build(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
    stop: &AtomicBool,
) -> super::super::Result<()> {
    let mut should_stop = || stop.load(Ordering::Acquire);
    build_or_resume_metadata(
        paths,
        volume,
        std::slice::from_ref(&paths.root),
        METADATA_CHECKPOINT_RECORDS,
        &mut should_stop,
    )?;
    Ok(())
}

fn refresh_volume_metadata(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
    stop: &AtomicBool,
    errors: &mut HashMap<VolumeKey, String>,
    snapshot: &SharedSnapshot,
    all_volumes: &[DiscoveredVolume],
) {
    if let Err(error) = begin_metadata_refresh(paths, volume) {
        errors.insert(volume.key.clone(), error.to_string());
        publish_snapshot(paths, all_volumes, snapshot, errors, true);
        return;
    }
    publish_snapshot(paths, all_volumes, snapshot, errors, true);
    match run_metadata_build(paths, volume, stop) {
        Ok(()) => {
            errors.remove(&volume.key);
        }
        Err(error) => {
            errors.insert(volume.key.clone(), error.to_string());
        }
    }
    publish_snapshot(paths, all_volumes, snapshot, errors, true);
}
