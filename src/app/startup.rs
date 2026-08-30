use super::incremental_runtime::incremental_checkpoint_status;
use super::{
    AppPaths, DiscoveredVolume, IncrementalCheckpointStatus, Result, VolumePhase,
    load_volume_manifest, validated_content_progress,
};
use crate::extraction::ExtractorConfig;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupAction {
    FreshMetadataBuild,
    ResumeMetadataBuild,
    ResumeContentBuild,
    CatchUpChanges,
    Reconcile,
    Ready,
}

pub fn determine_startup_action(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
    extractor: &ExtractorConfig,
    watch_changes: bool,
) -> Result<StartupAction> {
    let store = paths.volume_store(&volume.key);
    let Some(manifest) = load_volume_manifest(&store)? else {
        return Ok(StartupAction::FreshMetadataBuild);
    };

    if manifest.phase == VolumePhase::MetadataBuilding {
        return Ok(StartupAction::ResumeMetadataBuild);
    }
    if manifest.metadata_file.is_none() {
        return Ok(StartupAction::FreshMetadataBuild);
    }

    if watch_changes {
        return Ok(startup_action_for_checkpoint_status(
            incremental_checkpoint_status(paths, volume)?,
        ));
    }

    match validated_content_progress(paths, volume, extractor)? {
        Some(progress) if progress.complete => Ok(StartupAction::Ready),
        _ => Ok(StartupAction::ResumeContentBuild),
    }
}

fn startup_action_for_checkpoint_status(status: IncrementalCheckpointStatus) -> StartupAction {
    match status {
        IncrementalCheckpointStatus::Valid => StartupAction::CatchUpChanges,
        IncrementalCheckpointStatus::Missing
        | IncrementalCheckpointStatus::ReconcileRequired
        | IncrementalCheckpointStatus::Unavailable => StartupAction::Reconcile,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_incremental_checkpoint_selects_catch_up() {
        assert_eq!(
            startup_action_for_checkpoint_status(IncrementalCheckpointStatus::Valid),
            StartupAction::CatchUpChanges
        );
        for status in [
            IncrementalCheckpointStatus::Missing,
            IncrementalCheckpointStatus::ReconcileRequired,
            IncrementalCheckpointStatus::Unavailable,
        ] {
            assert_eq!(
                startup_action_for_checkpoint_status(status),
                StartupAction::Reconcile
            );
        }
    }
}
