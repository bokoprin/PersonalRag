use super::{
    AppPaths, DiscoveredVolume, Result, VolumePhase, load_volume_manifest,
    validated_content_progress,
};
use crate::extraction::ExtractorConfig;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupAction {
    FreshMetadataBuild,
    ResumeMetadataBuild,
    ResumeContentBuild,
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
        return Ok(StartupAction::Reconcile);
    }

    match validated_content_progress(paths, volume, extractor)? {
        Some(progress) if progress.complete => Ok(StartupAction::Ready),
        _ => Ok(StartupAction::ResumeContentBuild),
    }
}
