use personalrag_v2::{
    PersistentError, gc_valid_generations, load_latest, publish_generation, publish_next_generation,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_dir(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "personalrag-v2-persistent-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn write_fixture(root: &Path) {
    fs::write(
        root.join("a.txt"),
        b"alpha beta gamma\nabc|bcd|cde\nrare-q4=wxyz\nJapanese=\xe6\x97\xa5\xe6\x9c\xac\xe8\xaa\x9e\n",
    )
    .unwrap();
    fs::write(
        root.join("b.txt"),
        b"delta epsilon\nwxy|xyz klmn|lmno\nrare-q5=klmno\naaaaa\n",
    )
    .unwrap();
}

#[test]
fn publish_reload_preserves_q45_literal_results_and_line_boundaries() {
    let base = temp_dir("reload");
    let root = base.join("root");
    let store = base.join("store");
    fs::create_dir_all(&root).unwrap();
    write_fixture(&root);

    let published = publish_generation(&root, &store, 1, 0).unwrap();
    assert_eq!(published.generation, 1);
    assert!(published.capacity.q45_bytes > 0);

    let index = load_latest(&root, &store).unwrap();
    for (query, expected) in [
        ("wxyz", 1_usize),
        ("klmno", 1),
        ("日本語", 1),
        ("aaa", 3),
        ("BCDE", 0),
        ("abcde", 0),
    ] {
        let outcome = index.search_all(query, false).unwrap();
        assert_eq!(outcome.hits.len(), expected, "query={query}");
    }
    let adversarial = index.search_all("abcde", false).unwrap();
    assert!(adversarial.hits.is_empty());
    let _ = fs::remove_dir_all(base);
}

#[test]
fn corrupt_current_and_latest_generation_fall_back_to_previous_valid_generation() {
    let base = temp_dir("fallback");
    let root = base.join("root");
    let store = base.join("store");
    fs::create_dir_all(&root).unwrap();
    write_fixture(&root);
    publish_generation(&root, &store, 1, 0).unwrap();
    publish_generation(&root, &store, 2, 1).unwrap();

    let latest = store.join("gen-00000000000000000002.prv2");
    let mut bytes = fs::read(&latest).unwrap();
    let at = bytes.len() / 2;
    bytes[at] ^= 0x5a;
    fs::write(&latest, bytes).unwrap();
    fs::write(store.join("CURRENT"), b"not-a-generation\n").unwrap();

    let index = load_latest(&root, &store).unwrap();
    assert_eq!(index.generation(), 1);
    assert_eq!(index.search_all("wxyz", false).unwrap().hits.len(), 1);
    let _ = fs::remove_dir_all(base);
}

#[test]
fn source_drift_is_explicit_and_checksum_validation_is_available() {
    let base = temp_dir("drift");
    let root = base.join("root");
    let store = base.join("store");
    fs::create_dir_all(&root).unwrap();
    write_fixture(&root);
    publish_generation(&root, &store, 1, 0).unwrap();
    let index = load_latest(&root, &store).unwrap();
    index.validate_all_sources().unwrap();

    let mut changed = fs::read(root.join("a.txt")).unwrap();
    changed.extend_from_slice(b"changed\n");
    fs::write(root.join("a.txt"), changed).unwrap();
    let error = index.search_all("wxyz", false).unwrap_err();
    assert!(matches!(error, PersistentError::SourceDrift(_)));
    let _ = fs::remove_dir_all(base);
}

#[test]
fn gc_keeps_at_least_two_valid_generations_and_never_counts_corrupt_as_fallback() {
    let base = temp_dir("gc");
    let root = base.join("root");
    let store = base.join("store");
    fs::create_dir_all(&root).unwrap();
    write_fixture(&root);
    for generation in 1..=4 {
        publish_generation(&root, &store, generation, generation.saturating_sub(1)).unwrap();
    }
    let corrupt = store.join("gen-00000000000000000003.prv2");
    fs::write(&corrupt, b"corrupt").unwrap();
    let removed = gc_valid_generations(&store, 1).unwrap();
    assert!(
        removed
            .iter()
            .any(|path| path.ends_with("gen-00000000000000000001.prv2"))
    );
    assert!(store.join("gen-00000000000000000002.prv2").exists());
    assert!(store.join("gen-00000000000000000004.prv2").exists());
    assert!(corrupt.exists());
    let _ = fs::remove_dir_all(base);
}

#[test]
fn publish_next_generation_tracks_parent_and_current() {
    let base = temp_dir("next");
    let root = base.join("root");
    let store = base.join("store");
    fs::create_dir_all(&root).unwrap();
    write_fixture(&root);
    let first = publish_next_generation(&root, &store).unwrap();
    let second = publish_next_generation(&root, &store).unwrap();
    assert_eq!((first.generation, first.parent_generation), (1, 0));
    assert_eq!((second.generation, second.parent_generation), (2, 1));
    let loaded = load_latest(&root, &store).unwrap();
    assert_eq!((loaded.generation(), loaded.parent_generation()), (2, 1));
    let _ = fs::remove_dir_all(base);
}

#[cfg(unix)]
#[test]
fn unix_non_utf8_relative_path_round_trips_without_loss() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let base = temp_dir("nonutf8");
    let root = base.join("root");
    let store = base.join("store");
    fs::create_dir_all(&root).unwrap();
    let name = OsString::from_vec(vec![b'n', b'o', b'n', 0xff, b'.', b't', b'x', b't']);
    fs::write(root.join(name), b"exact-path-sentinel\n").unwrap();
    publish_generation(&root, &store, 1, 0).unwrap();
    let loaded = load_latest(&root, &store).unwrap();
    assert_eq!(
        loaded
            .search_all("exact-path-sentinel", true)
            .unwrap()
            .hits
            .len(),
        1
    );
    loaded.validate_all_sources().unwrap();
    let _ = fs::remove_dir_all(base);
}

#[test]
fn four_mib_controlled_corpus_stays_below_capacity_hard_gate() {
    let base = temp_dir("capacity");
    let root = base.join("root");
    let store = base.join("store");
    fs::create_dir_all(&root).unwrap();
    let mut body = Vec::new();
    while body.len() < 4 * 1024 * 1024 {
        body.extend_from_slice(b"abc|bcd|cde wxy|xyz klmn|lmno filler filler filler\n");
    }
    body.extend_from_slice(b"rare-q4=wxyz rare-q5=klmno\n");
    fs::write(root.join("large.txt"), body).unwrap();
    let published = publish_generation(&root, &store, 1, 0).unwrap();
    assert!(published.capacity.index_source_ratio() <= 0.10);
    assert!(published.capacity.q45_bytes > 0);
    let loaded = load_latest(&root, &store).unwrap();
    let adversarial = loaded.search_all("abcde", false).unwrap();
    assert!(adversarial.metrics.global_absent_shortcut);
    assert_eq!(adversarial.metrics.candidate_blocks, 0);
    let _ = fs::remove_dir_all(base);
}

#[test]
fn gc_preserves_valid_current_even_when_newer_orphan_generations_exist() {
    let base = temp_dir("gc-current");
    let root = base.join("root");
    let store = base.join("store");
    fs::create_dir_all(&root).unwrap();
    write_fixture(&root);
    for generation in 1..=3 {
        publish_generation(&root, &store, generation, generation.saturating_sub(1)).unwrap();
    }
    fs::write(store.join("CURRENT"), b"gen-00000000000000000001.prv2\n").unwrap();
    let removed = gc_valid_generations(&store, 2).unwrap();
    assert!(store.join("gen-00000000000000000001.prv2").exists());
    assert!(store.join("gen-00000000000000000003.prv2").exists());
    assert!(
        removed
            .iter()
            .any(|path| path.ends_with("gen-00000000000000000002.prv2"))
    );
    let loaded = load_latest(&root, &store).unwrap();
    assert_eq!(loaded.generation(), 1);
    let next = publish_next_generation(&root, &store).unwrap();
    assert_eq!(next.parent_generation, 1);
    assert_eq!(next.generation, 4);
    let _ = fs::remove_dir_all(base);
}

#[test]
fn persistent_v2_reload_preserves_unicode_regex_and_wildcard_semantics() {
    let base = temp_dir("unicode-patterns");
    let root = base.join("root");
    let store = base.join("store");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        root.join("unicode.txt"),
        "xxStraße yy Cafe\u{301} 日本語\nCreateDirectoryW\nERROR_1234\n".as_bytes(),
    )
    .unwrap();
    publish_generation(&root, &store, 1, 0).unwrap();
    let index = load_latest(&root, &store).unwrap();

    let strasse = index.search_all("STRASSE", false).unwrap();
    assert_eq!(strasse.hits.len(), 1);
    assert_eq!(strasse.hits[0].byte_offset_in_line, 2);
    assert_eq!(index.search_all("CAFÉ", false).unwrap().hits.len(), 1);
    assert_eq!(index.search_all("日本語", true).unwrap().hits.len(), 1);
    assert_eq!(
        index
            .search_regex_all(r"Create(File|Directory)W", false)
            .unwrap()
            .hits
            .len(),
        1
    );
    assert_eq!(
        index
            .search_regex_all(r"ERROR_[0-9]{4}", true)
            .unwrap()
            .hits
            .len(),
        1
    );
    assert_eq!(
        index
            .search_wildcard_all("Create*W", false)
            .unwrap()
            .hits
            .len(),
        1
    );
    let _ = fs::remove_dir_all(base);
}
