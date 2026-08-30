#![cfg(windows)]

use personalrag_v2::app::{
    AppPaths, AppRuntimeHandle, DiscoveredVolume, FederatedContentIndex, FederatedMetadataIndex,
    IncrementalCheckpointStatus, VolumeKey, VolumePhase, incremental_checkpoint_status,
};
use personalrag_v2::extraction::ExtractorConfig;
use personalrag_v2::{SearchLimits, incremental::ContentQueryKind};
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn e2e_volume() -> Option<PathBuf> {
    std::env::var_os("PERSONALRAG_STEP8_E2E_VOLUME").map(PathBuf::from)
}

fn wait_until(deadline: Instant, mut predicate: impl FnMut() -> bool, label: &str) {
    while !predicate() {
        assert!(Instant::now() < deadline, "timed out waiting for {label}");
        thread::sleep(Duration::from_millis(200));
    }
}

fn metadata_has(paths: &AppPaths, volume: &DiscoveredVolume, name: &str) -> bool {
    FederatedMetadataIndex::load(paths, std::slice::from_ref(volume))
        .map(|index| !index.search(Some(name), None, false, 50).is_empty())
        .unwrap_or(false)
}

fn content_has(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
    extractor: &ExtractorConfig,
    needle: &str,
) -> bool {
    FederatedContentIndex::load(paths, std::slice::from_ref(volume), extractor)
        .and_then(|index| {
            index.search(
                ContentQueryKind::Literal(needle),
                false,
                SearchLimits::default(),
            )
        })
        .map(|hits| !hits.is_empty())
        .unwrap_or(false)
}

#[test]
#[ignore = "requires PERSONALRAG_STEP8_E2E_VOLUME pointing at disposable NTFS volume"]
fn native_ntfs_usn_continuous_indexing_survives_restart() {
    let Some(root) = e2e_volume() else {
        eprintln!("PERSONALRAG_STEP8_E2E_VOLUME is not set; skipping native E2E");
        return;
    };
    assert!(
        root.exists(),
        "E2E volume does not exist: {}",
        root.display()
    );

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let app_root = std::env::temp_dir().join(format!("personalrag-step8-e2e-{stamp}"));
    let paths = AppPaths::for_root(&app_root);
    let volume = DiscoveredVolume {
        key: VolumeKey(format!("step8-e2e-{stamp}")),
        mount: root.clone(),
        serial: 0,
    };
    let extractor = ExtractorConfig::discover();

    let initial = root.join("initial.txt");
    let renamed = root.join("renamed.txt");
    let deleted = root.join("delete-me.txt");
    let offline = root.join("offline.txt");
    fs::write(&initial, "initial-native-needle").unwrap();
    fs::write(&deleted, "delete-native-needle").unwrap();

    let mut runtime =
        AppRuntimeHandle::start_with(paths.clone(), vec![volume.clone()], extractor.clone(), true)
            .unwrap();
    let ready_deadline = Instant::now() + Duration::from_secs(90);
    wait_until(
        ready_deadline,
        || {
            let snapshot = runtime.snapshot();
            snapshot.content_ready_volumes == 1
                && snapshot
                    .volumes
                    .first()
                    .is_some_and(|status| status.phase == VolumePhase::Ready)
        },
        "initial Ready",
    );
    assert_eq!(
        incremental_checkpoint_status(&paths, &volume).unwrap(),
        IncrementalCheckpointStatus::Valid,
        "native E2E must exercise a real NTFS USN checkpoint, not only reconcile fallback"
    );
    assert!(metadata_has(&paths, &volume, "initial.txt"));
    assert!(content_has(
        &paths,
        &volume,
        &extractor,
        "initial-native-needle"
    ));

    fs::write(&initial, "modified-native-needle-with-different-size").unwrap();
    fs::write(root.join("created.txt"), "created-native-needle").unwrap();
    fs::rename(root.join("created.txt"), &renamed).unwrap();
    fs::remove_file(&deleted).unwrap();

    let update_deadline = Instant::now() + Duration::from_secs(90);
    wait_until(
        update_deadline,
        || {
            metadata_has(&paths, &volume, "renamed.txt")
                && !metadata_has(&paths, &volume, "created.txt")
                && !metadata_has(&paths, &volume, "delete-me.txt")
                && content_has(&paths, &volume, &extractor, "modified-native-needle")
                && content_has(&paths, &volume, &extractor, "created-native-needle")
                && !content_has(&paths, &volume, &extractor, "initial-native-needle")
        },
        "live USN metadata/content catch-up",
    );
    wait_until(
        update_deadline,
        || {
            runtime.snapshot().volumes.first().is_some_and(|status| {
                status.phase == VolumePhase::Ready && status.content_dirty_files == 0
            })
        },
        "live catch-up Ready",
    );
    runtime.join();

    fs::write(&offline, "offline-native-needle").unwrap();
    fs::write(&initial, "offline-modified-native-needle").unwrap();

    let mut restarted =
        AppRuntimeHandle::start_with(paths.clone(), vec![volume.clone()], extractor.clone(), true)
            .unwrap();
    let restart_deadline = Instant::now() + Duration::from_secs(90);
    wait_until(
        restart_deadline,
        || {
            metadata_has(&paths, &volume, "offline.txt")
                && content_has(&paths, &volume, &extractor, "offline-native-needle")
                && content_has(
                    &paths,
                    &volume,
                    &extractor,
                    "offline-modified-native-needle",
                )
                && restarted.snapshot().volumes.first().is_some_and(|status| {
                    status.phase == VolumePhase::Ready && status.content_dirty_files == 0
                })
        },
        "restart catch-up",
    );
    assert_eq!(
        incremental_checkpoint_status(&paths, &volume).unwrap(),
        IncrementalCheckpointStatus::Valid,
    );
    restarted.join();

    for path in [&initial, &renamed, &offline] {
        let _ = fs::remove_file(path);
    }
    let _ = fs::remove_dir_all(&app_root);
}
