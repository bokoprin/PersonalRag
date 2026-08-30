mod content;
mod metadata_build;
mod runtime;
mod startup;
mod state_io;
mod volume;

pub use content::*;
pub use metadata_build::*;
pub use runtime::*;
pub use startup::*;
pub use volume::{
    AppPaths, DiscoveredVolume, VolumeKey, VolumeManifest, VolumePhase, discover_fixed_volumes,
    load_volume_manifest,
};
pub(super) use volume::{write_app_catalog, write_volume_manifest};

use crate::metadata::{MetadataIndex, MetadataRecord, MetadataSearchRequest};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum AppError {
    Io(io::Error),
    Metadata(crate::metadata::MetadataError),
    Persistent(crate::persistent::PersistentError),
    Product(crate::product::ProductError),
    InvalidState(String),
    Unsupported(String),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::Metadata(error) => write!(f, "metadata error: {error}"),
            Self::Persistent(error) => write!(f, "persistent error: {error}"),
            Self::Product(error) => write!(f, "product error: {error}"),
            Self::InvalidState(message) => f.write_str(message),
            Self::Unsupported(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for AppError {}

impl From<io::Error> for AppError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<crate::metadata::MetadataError> for AppError {
    fn from(value: crate::metadata::MetadataError) -> Self {
        Self::Metadata(value)
    }
}

impl From<crate::persistent::PersistentError> for AppError {
    fn from(value: crate::persistent::PersistentError) -> Self {
        Self::Persistent(value)
    }
}

impl From<crate::product::ProductError> for AppError {
    fn from(value: crate::product::ProductError) -> Self {
        Self::Product(value)
    }
}

pub type Result<T> = std::result::Result<T, AppError>;

#[derive(Clone, Debug)]
pub struct AppMetadataHit {
    pub volume: VolumeKey,
    pub absolute_path: PathBuf,
    pub record: MetadataRecord,
}

pub struct FederatedMetadataIndex {
    volumes: Vec<(DiscoveredVolume, VolumeManifest, MetadataIndex)>,
}

impl FederatedMetadataIndex {
    pub fn load(paths: &AppPaths, volumes: &[DiscoveredVolume]) -> Result<Self> {
        let mut loaded = Vec::new();
        for volume in volumes {
            let store = paths.volume_store(&volume.key);
            let Some(manifest) = load_volume_manifest(&store)? else {
                continue;
            };
            let Some(file_name) = manifest.metadata_file.as_deref() else {
                continue;
            };
            let metadata = MetadataIndex::load_snapshot(store.join("metadata").join(file_name))?;
            loaded.push((volume.clone(), manifest, metadata));
        }
        Ok(Self { volumes: loaded })
    }

    pub fn metadata_records(&self) -> usize {
        self.volumes
            .iter()
            .map(|(_, _, metadata)| metadata.records().len())
            .sum()
    }

    pub fn search(
        &self,
        filename: Option<&str>,
        full_path: Option<&str>,
        case_sensitive: bool,
        max_results: usize,
    ) -> Vec<AppMetadataHit> {
        let max_results = max_results.max(1);
        let mut out = Vec::new();
        for (volume, _, metadata) in &self.volumes {
            let path_query =
                full_path.and_then(|query| path_query_for_volume(query, &volume.mount));
            if full_path.is_some() && path_query.is_none() {
                continue;
            }
            let outcome = metadata.search(MetadataSearchRequest {
                filename,
                full_path: path_query,
                case_sensitive,
                max_results,
            });
            for hit in outcome.hits {
                let Some(record) = metadata.records().get(hit.record_index as usize) else {
                    continue;
                };
                out.push(AppMetadataHit {
                    volume: volume.key.clone(),
                    absolute_path: volume.mount.join(&record.path),
                    record: record.clone(),
                });
            }
        }
        out.sort_by(|left, right| {
            let left_name = left
                .record
                .path
                .file_name()
                .map(|value| value.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            let right_name = right
                .record
                .path
                .file_name()
                .map(|value| value.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            left_name
                .cmp(&right_name)
                .then_with(|| left.absolute_path.cmp(&right.absolute_path))
                .then_with(|| left.record.file_id.cmp(&right.record.file_id))
        });
        out.truncate(max_results);
        out
    }
}

fn path_query_for_volume<'a>(query: &'a str, mount: &Path) -> Option<&'a str> {
    let mount_text = mount.to_string_lossy();
    if query.len() >= 2 && query.as_bytes().get(1) == Some(&b':') {
        let mount_drive = mount_text.as_bytes().first().copied()?.to_ascii_lowercase();
        let query_drive = query.as_bytes().first().copied()?.to_ascii_lowercase();
        if mount_drive != query_drive {
            return None;
        }
        return Some(query[2..].trim_start_matches(['\\', '/']));
    }
    Some(query)
}

pub struct AppCoordinator {
    pub paths: AppPaths,
    pub volumes: Vec<DiscoveredVolume>,
}

impl AppCoordinator {
    pub fn new_default() -> Result<Self> {
        let paths = AppPaths::default_for_current_user()?;
        paths.ensure()?;
        let volumes = discover_fixed_volumes()?;
        write_app_catalog(&paths, &volumes)?;
        Ok(Self { paths, volumes })
    }

    pub fn with_volumes(paths: AppPaths, volumes: Vec<DiscoveredVolume>) -> Result<Self> {
        paths.ensure()?;
        write_app_catalog(&paths, &volumes)?;
        Ok(Self { paths, volumes })
    }

    pub fn run_metadata_phase<F>(&self, mut should_stop: F) -> Result<Vec<MetadataBuildReport>>
    where
        F: FnMut() -> bool,
    {
        let mut reports = Vec::new();
        for volume in &self.volumes {
            if should_stop() {
                break;
            }
            let report = build_or_resume_metadata(
                &self.paths,
                volume,
                std::slice::from_ref(&self.paths.root),
                METADATA_CHECKPOINT_RECORDS,
                &mut should_stop,
            )?;
            reports.push(report);
        }
        Ok(reports)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "personalrag-app-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn fake_volume(root: &Path, name: &str) -> DiscoveredVolume {
        DiscoveredVolume {
            key: VolumeKey(format!("test-{name}")),
            mount: root.to_path_buf(),
            serial: 1,
        }
    }

    #[test]
    fn federated_filename_search_merges_ready_volumes() {
        let base = temp_dir("federated");
        let app_root = base.join("app");
        let root_a = base.join("a");
        let root_b = base.join("b");
        fs::create_dir_all(&root_a).unwrap();
        fs::create_dir_all(&root_b).unwrap();
        fs::write(root_a.join("shared-alpha.txt"), "a").unwrap();
        fs::write(root_b.join("shared-beta.txt"), "b").unwrap();

        let paths = AppPaths::for_root(&app_root);
        paths.ensure().unwrap();
        let volume_a = fake_volume(&root_a, "a");
        let volume_b = fake_volume(&root_b, "b");
        let mut never_stop = || false;
        build_or_resume_metadata(
            &paths,
            &volume_a,
            std::slice::from_ref(&app_root),
            1,
            &mut never_stop,
        )
        .unwrap();
        build_or_resume_metadata(&paths, &volume_b, &[app_root], 1, &mut never_stop).unwrap();

        let federated =
            FederatedMetadataIndex::load(&paths, &[volume_a.clone(), volume_b.clone()]).unwrap();
        let hits = federated.search(Some("shared"), None, false, 100);
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().any(|hit| hit.volume == volume_a.key));
        assert!(hits.iter().any(|hit| hit.volume == volume_b.key));
        fs::remove_dir_all(base).unwrap();
    }
}
