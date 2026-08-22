use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use personalrag_portable_search::{
    BuildMode, BuildOptions, DocumentInput, LogicalDocumentIdentity, MergedIndex, build_index,
    initialize_generation_from_verified_built_index, verify_built_index_for_generation_adoption,
    verify_generation, verify_generation_structure,
};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "personalrag-verified-adoption-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = make_writable_tree(&self.0);
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn make_writable_tree(root: &Path) -> std::io::Result<()> {
    if !root.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            make_writable_tree(&entry.path())?;
        } else {
            make_writable(&entry.path())?;
        }
    }
    Ok(())
}

fn make_writable(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o644))
    }
    #[cfg(not(unix))]
    {
        let mut permissions = fs::metadata(path)?.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions)
    }
}

fn fixture() -> (Vec<DocumentInput>, Vec<LogicalDocumentIdentity>) {
    let mut docs = Vec::new();
    let mut identities = Vec::new();
    for index in 0..96usize {
        let path = format!("src/module_{index:04}.rs");
        let content = format!("fn module_{index}() {{ let marker = \"verified_{index}\"; }}\n");
        docs.push(DocumentInput::new(
            path.clone(),
            path.clone(),
            path.to_ascii_lowercase().into_bytes(),
            content.to_ascii_lowercase().into_bytes(),
        ));
        identities.push(LogicalDocumentIdentity::new(
            index as u64 + 1,
            path.clone(),
            path,
        ));
    }
    (docs, identities)
}

fn build_fixture(parent: &Path) -> (PathBuf, Vec<LogicalDocumentIdentity>) {
    let (docs, identities) = fixture();
    let built = parent.join("built");
    build_index(
        &docs,
        &built,
        &BuildOptions {
            mode: BuildMode::Adaptive,
            segment_docs: 23,
            workers: 3,
        },
    )
    .unwrap();
    (built, identities)
}

fn first_segment(root: &Path) -> PathBuf {
    fn visit(path: &Path) -> Option<PathBuf> {
        for entry in fs::read_dir(path).ok()? {
            let entry = entry.ok()?;
            let candidate = entry.path();
            if entry.file_type().ok()?.is_dir() {
                if let Some(found) = visit(&candidate) {
                    return Some(found);
                }
            } else if candidate.extension().and_then(|value| value.to_str()) == Some("prseg") {
                return Some(candidate);
            }
        }
        None
    }
    visit(root).expect("fixture must contain a .prseg segment")
}

fn corrupt_middle(path: &Path) {
    make_writable(path).unwrap();
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let len = file.metadata().unwrap().len();
    assert!(len > 64);
    let offset = (len / 2).min(len - 17);
    file.seek(SeekFrom::Start(offset)).unwrap();
    let mut byte = [0u8; 1];
    file.read_exact(&mut byte).unwrap();
    byte[0] ^= 0x5a;
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(&byte).unwrap();
    file.sync_all().unwrap();
}

#[test]
fn verified_adoption_publishes_and_structure_verification_passes() {
    let temp = TempDir::new("success");
    let (built, identities) = build_fixture(temp.path());
    let store = temp.path().join("store");

    let verified = verify_built_index_for_generation_adoption(&built).unwrap();
    let report =
        initialize_generation_from_verified_built_index(&store, verified, &identities).unwrap();

    assert_eq!(report.live_docs, identities.len());
    assert!(!built.exists());
    verify_generation_structure(&store).unwrap();
    verify_generation(&store).unwrap();
    assert_eq!(
        MergedIndex::open(&store, false).unwrap().live_docs(),
        identities.len()
    );
}

#[test]
fn checksum_gate_rejects_corrupt_segment_before_adoption() {
    let temp = TempDir::new("corrupt-before");
    let (built, _) = build_fixture(temp.path());
    corrupt_middle(&first_segment(&built));

    assert!(verify_built_index_for_generation_adoption(&built).is_err());
}

#[test]
fn structure_verify_rejects_corrupt_logical_map() {
    let temp = TempDir::new("logical-map");
    let (built, identities) = build_fixture(temp.path());
    let store = temp.path().join("store");
    let verified = verify_built_index_for_generation_adoption(&built).unwrap();
    initialize_generation_from_verified_built_index(&store, verified, &identities).unwrap();

    let map = store
        .join("components")
        .join("base-g0000000000000000")
        .join("logical-map.bin");
    corrupt_middle(&map);
    assert!(verify_generation_structure(&store).is_err());
}

#[test]
fn full_generation_verify_still_rejects_segment_corruption_after_publish() {
    let temp = TempDir::new("corrupt-after");
    let (built, identities) = build_fixture(temp.path());
    let store = temp.path().join("store");
    let verified = verify_built_index_for_generation_adoption(&built).unwrap();
    initialize_generation_from_verified_built_index(&store, verified, &identities).unwrap();

    let base = store.join("components").join("base-g0000000000000000");
    corrupt_middle(&first_segment(&base));
    assert!(verify_generation(&store).is_err());
}
