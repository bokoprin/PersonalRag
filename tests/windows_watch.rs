#![cfg(windows)]

use personalrag_v2::extraction::ExtractorConfig;
use personalrag_v2::gui::{GuiSearchRequest, GuiSearchSession};
use personalrag_v2::product::{WindowsWatchProducer, initialize_store};
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn temp_dir(tag: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "personalrag-windows-watch-{tag}-{}-{stamp}",
        std::process::id()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn non_elevated_watch_path_publishes_a_real_content_change() {
    let base = temp_dir("live");
    let root = base.join("root");
    let store = base.join("store");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&store).unwrap();

    let mut filler = Vec::with_capacity(4 * 1024 * 1024);
    while filler.len() < 4 * 1024 * 1024 {
        filler.extend_from_slice(b"windows watch deterministic capacity filler 0123456789\n");
    }
    fs::write(root.join("filler.txt"), filler).unwrap();
    fs::write(root.join("item.txt"), b"PR_WATCH_OLD_TOKEN\n").unwrap();

    let extractor = ExtractorConfig::default();
    initialize_store(&root, &store, &extractor).unwrap();
    let mut producer = WindowsWatchProducer::open(&root, &store, extractor.clone()).unwrap();

    fs::write(root.join("item.txt"), b"PR_WATCH_NEW_TOKEN\n").unwrap();

    let mut published = false;
    for _ in 0..200 {
        if producer.poll_once().unwrap().is_some() {
            published = true;
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        published,
        "watch mode {} did not publish the observed file change",
        producer.mode().as_str()
    );

    let gui = GuiSearchSession::load(&root, &store, extractor).unwrap();
    let request = |token: &str| GuiSearchRequest {
        content_query: token.to_string(),
        max_files: 20,
        ..GuiSearchRequest::default()
    };
    assert!(gui.search(&request("PR_WATCH_OLD_TOKEN")).unwrap().rows.is_empty());
    assert_eq!(
        gui.search(&request("PR_WATCH_NEW_TOKEN")).unwrap().rows.len(),
        1
    );

    fs::remove_dir_all(base).unwrap();
}
