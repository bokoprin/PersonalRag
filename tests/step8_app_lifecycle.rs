mod support;

use personalrag_v2::SearchLimits;
use personalrag_v2::app::{
    FederatedContentIndex, FederatedMetadataIndex, StartupAction, begin_metadata_refresh,
    build_or_resume_metadata, determine_startup_action, load_volume_manifest,
};
use personalrag_v2::incremental::ContentQueryKind;
use support::Step8TestApp;

#[test]
fn step8_harness_builds_searchable_metadata_and_content() {
    let mut app = Step8TestApp::new("searchable");
    app.write("folder/alpha-target.txt", "content-target\n");
    app.start(false);
    app.wait_ready();

    let metadata =
        FederatedMetadataIndex::load(&app.paths, std::slice::from_ref(&app.volume)).unwrap();
    let file_hits = metadata.search(Some("alpha-target"), None, false, 100);
    assert_eq!(file_hits.len(), 1);

    let content = FederatedContentIndex::load(
        &app.paths,
        std::slice::from_ref(&app.volume),
        &app.extractor,
    )
    .unwrap();
    let content_hits = content
        .search(
            ContentQueryKind::Literal("content-target"),
            false,
            SearchLimits::default(),
        )
        .unwrap();
    assert_eq!(content_hits.len(), 1);
}

#[test]
fn startup_action_distinguishes_ready_reuse_from_watched_reconcile() {
    let mut app = Step8TestApp::new("startup-action");
    app.write("ready.txt", "ready-content\n");
    app.start(false);
    app.wait_ready();
    app.stop();

    let manifest = load_volume_manifest(&app.paths.volume_store(&app.volume.key))
        .unwrap()
        .unwrap();
    assert!(manifest.metadata_file.is_some());

    assert_eq!(
        determine_startup_action(&app.paths, &app.volume, &app.extractor, false).unwrap(),
        StartupAction::Ready
    );
    assert_eq!(
        determine_startup_action(&app.paths, &app.volume, &app.extractor, true).unwrap(),
        StartupAction::Reconcile
    );
}

#[test]
fn startup_action_covers_fresh_metadata_resume_and_content_resume() {
    let app = Step8TestApp::new("startup-phases");
    app.write("phase.txt", "phase-content\n");

    assert_eq!(
        determine_startup_action(&app.paths, &app.volume, &app.extractor, false).unwrap(),
        StartupAction::FreshMetadataBuild
    );

    begin_metadata_refresh(&app.paths, &app.volume).unwrap();
    assert_eq!(
        determine_startup_action(&app.paths, &app.volume, &app.extractor, false).unwrap(),
        StartupAction::ResumeMetadataBuild
    );

    let mut never_stop = || false;
    build_or_resume_metadata(
        &app.paths,
        &app.volume,
        std::slice::from_ref(&app.paths.root),
        2,
        &mut never_stop,
    )
    .unwrap();
    assert_eq!(
        determine_startup_action(&app.paths, &app.volume, &app.extractor, false).unwrap(),
        StartupAction::ResumeContentBuild
    );
}
