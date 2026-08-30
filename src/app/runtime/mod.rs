mod coordinator;
mod snapshot;
mod watcher;

pub use snapshot::{RuntimeReader, RuntimeSnapshot, RuntimeVolumeStatus};

use super::{AppCoordinator, AppPaths, DiscoveredVolume, Result};
use crate::extraction::ExtractorConfig;
use snapshot::{SharedSnapshot, starting_snapshot};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};

pub struct AppRuntimeHandle {
    paths: AppPaths,
    volumes: Vec<DiscoveredVolume>,
    snapshot: SharedSnapshot,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl AppRuntimeHandle {
    pub fn start_default(extractor: ExtractorConfig) -> Result<Self> {
        let coordinator = AppCoordinator::new_default()?;
        Self::start_with(coordinator.paths, coordinator.volumes, extractor, true)
    }

    pub fn start_with(
        paths: AppPaths,
        volumes: Vec<DiscoveredVolume>,
        extractor: ExtractorConfig,
        watch_changes: bool,
    ) -> Result<Self> {
        paths.ensure()?;
        let snapshot = starting_snapshot(&volumes);
        snapshot::publish_snapshot(
            &paths,
            &volumes,
            &snapshot,
            &std::collections::HashMap::new(),
            true,
        );
        let stop = Arc::new(AtomicBool::new(false));
        let thread_paths = paths.clone();
        let thread_volumes = volumes.clone();
        let thread_snapshot = Arc::clone(&snapshot);
        let thread_stop = Arc::clone(&stop);
        let join = thread::Builder::new()
            .name("personalrag-index-coordinator".to_string())
            .spawn(move || {
                coordinator::run_coordinator(
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
        RuntimeReader::new(Arc::clone(&self.snapshot))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{VolumeKey, load_volume_manifest};
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
        let mut runtime =
            AppRuntimeHandle::start_with(paths, vec![volume], ExtractorConfig::discover(), false)
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

    #[test]
    fn ready_runtime_without_watch_reuses_existing_metadata_generation() {
        let base = temp_dir("reuse-ready");
        let root = base.join("root");
        let app_root = base.join("app");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "runtime-needle").unwrap();

        let paths = AppPaths::for_root(&app_root);
        let volume = volume(&root);
        let extractor = ExtractorConfig::discover();
        let mut first = AppRuntimeHandle::start_with(
            paths.clone(),
            vec![volume.clone()],
            extractor.clone(),
            false,
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while first.snapshot().content_ready_volumes != 1 {
            assert!(
                Instant::now() < deadline,
                "first runtime did not become ready"
            );
            thread::sleep(Duration::from_millis(20));
        }
        first.join();
        let before = load_volume_manifest(&paths.volume_store(&volume.key))
            .unwrap()
            .unwrap()
            .metadata_generation;

        let mut second =
            AppRuntimeHandle::start_with(paths.clone(), vec![volume.clone()], extractor, false)
                .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while second.snapshot().content_ready_volumes != 1 {
            assert!(
                Instant::now() < deadline,
                "second runtime did not become ready"
            );
            thread::sleep(Duration::from_millis(20));
        }
        second.join();
        let after = load_volume_manifest(&paths.volume_store(&volume.key))
            .unwrap()
            .unwrap()
            .metadata_generation;
        assert_eq!(before, after);
        fs::remove_dir_all(base).unwrap();
    }
}
