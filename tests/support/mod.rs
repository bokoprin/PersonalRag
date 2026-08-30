#![allow(dead_code)]

use personalrag_v2::app::{AppPaths, AppRuntimeHandle, DiscoveredVolume, VolumeKey};
use personalrag_v2::extraction::ExtractorConfig;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub struct Step8TestApp {
    pub base: PathBuf,
    pub root: PathBuf,
    pub paths: AppPaths,
    pub volume: DiscoveredVolume,
    pub extractor: ExtractorConfig,
    runtime: Option<AppRuntimeHandle>,
}

impl Step8TestApp {
    pub fn new(name: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "personalrag-step8-harness-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let root = base.join("root");
        let app_root = base.join("app");
        fs::create_dir_all(&root).unwrap();
        let paths = AppPaths::for_root(app_root);
        let volume = DiscoveredVolume {
            key: VolumeKey(format!("step8-harness-{name}")),
            mount: root.clone(),
            serial: 1,
        };
        Self {
            base,
            root,
            paths,
            volume,
            extractor: ExtractorConfig::discover(),
            runtime: None,
        }
    }

    pub fn write(&self, relative: impl AsRef<Path>, contents: impl AsRef<[u8]>) {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    pub fn start(&mut self, watch_changes: bool) {
        assert!(self.runtime.is_none(), "runtime already started");
        self.runtime = Some(
            AppRuntimeHandle::start_with(
                self.paths.clone(),
                vec![self.volume.clone()],
                self.extractor.clone(),
                watch_changes,
            )
            .unwrap(),
        );
    }

    pub fn wait_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let snapshot = self.runtime().snapshot();
            if snapshot.content_ready_volumes == 1 {
                assert!(snapshot.filename_search_available);
                assert!(snapshot.content_search_available);
                return;
            }
            assert!(Instant::now() < deadline, "runtime did not become ready");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn stop(&mut self) {
        if let Some(mut runtime) = self.runtime.take() {
            runtime.join();
            assert!(!runtime.snapshot().running);
        }
    }

    pub fn runtime(&self) -> &AppRuntimeHandle {
        self.runtime.as_ref().expect("runtime not started")
    }
}

impl Drop for Step8TestApp {
    fn drop(&mut self) {
        self.stop();
        let _ = fs::remove_dir_all(&self.base);
    }
}
