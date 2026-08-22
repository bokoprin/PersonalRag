from pathlib import Path


def replace_once(text: str, old: str, new: str, label: str) -> str:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{label}: expected exactly one match, got {count}")
    return text.replace(old, new, 1)


def replace_span(text: str, start_marker: str, end_marker: str, replacement: str, label: str) -> str:
    start = text.find(start_marker)
    if start < 0:
        raise SystemExit(f"{label}: start marker not found")
    end = text.find(end_marker, start)
    if end < 0:
        raise SystemExit(f"{label}: end marker not found")
    return text[:start] + replacement + text[end:]


generation_path = Path("search-core/src/generation.rs")
generation = generation_path.read_text(encoding="utf-8")

insert_marker = "#[derive(Clone, Debug)]\npub struct GenerationReport"
verified_type = '''/// Proof that a portable base index completed a full read-back checksum verification.\n///\n/// The token has no public constructor and is consumed by generation adoption, preventing the\n/// high-throughput adoption path from accidentally skipping its one required full index verify.\n#[derive(Debug)]\npub struct VerifiedBuiltIndex {\n    path: PathBuf,\n}\n\npub fn verify_built_index_for_generation_adoption(\n    built_index: impl AsRef<Path>,\n) -> Result<VerifiedBuiltIndex> {\n    let path = built_index.as_ref().to_path_buf();\n    verify_index(&path)?;\n    Ok(VerifiedBuiltIndex { path })\n}\n\n'''
if "pub struct VerifiedBuiltIndex" in generation:
    raise SystemExit("generation: VerifiedBuiltIndex already exists")
generation = replace_once(
    generation,
    insert_marker,
    verified_type + insert_marker,
    "insert VerifiedBuiltIndex",
)

start_marker = "pub fn initialize_generation_from_built_index(\n"
end_marker = "pub fn publish_incremental_update(\n"
new_adoption = '''pub fn initialize_generation_from_built_index(\n    root: impl AsRef<Path>,\n    built_index: impl AsRef<Path>,\n    documents: &[LogicalDocumentIdentity],\n) -> Result<GenerationReport> {\n    let verified = verify_built_index_for_generation_adoption(built_index)?;\n    initialize_generation_from_verified_built_index(root, verified, documents)\n}\n\n/// Adopt an index that has already passed a full read-back checksum verification.\n///\n/// `verified` is consumed so the fast path cannot be called with an arbitrary path. The adoption\n/// step only adds the logical map and generation metadata; it never rewrites the verified segment\n/// payloads. Call `verify_generation_structure` after publication to validate CURRENT, the\n/// generation manifest, the logical map checksum, and logical/physical document consistency.\npub fn initialize_generation_from_verified_built_index(\n    root: impl AsRef<Path>,\n    verified: VerifiedBuiltIndex,\n    documents: &[LogicalDocumentIdentity],\n) -> Result<GenerationReport> {\n    let root = root.as_ref();\n    let built_index = verified.path.as_path();\n    if root == built_index {\n        return Err(SearchError::InvalidArgument(\n            "generation root and built index must be different paths".into(),\n        ));\n    }\n    if root.join("CURRENT").exists() {\n        return Err(SearchError::InvalidArgument(\n            "generation store is already initialized".into(),\n        ));\n    }\n\n    // The checksum-heavy segment verification has already been performed to create `verified`.\n    // This cheap open revalidates the portable manifest shape and obtains the physical doc count.\n    let physical = LazyPersistentIndex::open(built_index)?;\n    let physical_docs = usize::try_from(physical.docs())\n        .map_err(|_| SearchError::Format("physical document count too large".into()))?;\n    if physical_docs != documents.len() {\n        return Err(SearchError::InvalidArgument(format!(\n            "logical identity count {} does not match physical document count {physical_docs}",\n            documents.len()\n        )));\n    }\n    validate_logical_identities(documents)?;\n\n    fs::create_dir_all(root.join("components"))?;\n    fs::create_dir_all(root.join("generations"))?;\n    let base_relative = "components/base-g0000000000000000".to_owned();\n    let base_path = root.join(&base_relative);\n    if base_path.exists() {\n        return Err(SearchError::InvalidArgument(\n            "base component already exists".into(),\n        ));\n    }\n\n    write_doc_map_identities(&built_index.join("logical-map.bin"), documents)?;\n    sync_directory(built_index)?;\n    fs::rename(built_index, &base_path)?;\n    sync_directory(&root.join("components"))?;\n\n    let manifest = GenerationManifest {\n        generation: 0,\n        sources: vec![SourceDescriptor {\n            kind: SourceKind::Base,\n            generation: 0,\n            index_dir: base_relative,\n            map_file: "logical-map.bin".into(),\n            tombstone_file: None,\n        }],\n    };\n    let manifest_relative = generation_manifest_relative(0, "base");\n    publish_generation_manifest(root, &manifest_relative, &manifest)?;\n    publish_current(root, 0, &manifest_relative)?;\n    Ok(GenerationReport {\n        generation: 0,\n        live_docs: documents.len(),\n        delta_count: 0,\n        build: None,\n        compacted: false,\n    })\n}\n\n'''
generation = replace_span(
    generation,
    start_marker,
    end_marker,
    new_adoption,
    "replace adoption functions",
)

old_verify = '''pub fn verify_generation(root: impl AsRef<Path>) -> Result<()> {\n    let _ = MergedIndex::open(root, true)?;\n    Ok(())\n}\n'''
new_verify = '''pub fn verify_generation(root: impl AsRef<Path>) -> Result<()> {\n    let _ = MergedIndex::open(root, true)?;\n    Ok(())\n}\n\n/// Verify generation metadata and logical mapping without re-hashing already verified base\n/// segment payloads. This is intended for the immediate post-publication check after adopting a\n/// `VerifiedBuiltIndex`; `verify_generation` remains the full checksum verification API.\npub fn verify_generation_structure(root: impl AsRef<Path>) -> Result<()> {\n    let _ = MergedIndex::open(root, false)?;\n    Ok(())\n}\n'''
generation = replace_once(generation, old_verify, new_verify, "add structure verifier")
generation_path.write_text(generation, encoding="utf-8")

lib_path = Path("search-core/src/lib.rs")
lib = lib_path.read_text(encoding="utf-8")
old_exports = '''    GenerationReport, LogicalDocument, LogicalDocumentIdentity, MergedIndex, MergedSearchSession,\n    compact_generation, compact_generation_unified, initialize_generation,\n    initialize_generation_from_built_index, publish_incremental_update,\n    publish_incremental_update_unified, verify_generation,\n'''
new_exports = '''    GenerationReport, LogicalDocument, LogicalDocumentIdentity, MergedIndex, MergedSearchSession,\n    VerifiedBuiltIndex, compact_generation, compact_generation_unified, initialize_generation,\n    initialize_generation_from_built_index, initialize_generation_from_verified_built_index,\n    publish_incremental_update, publish_incremental_update_unified,\n    verify_built_index_for_generation_adoption, verify_generation, verify_generation_structure,\n'''
lib = replace_once(lib, old_exports, new_exports, "generation exports")
lib_path.write_text(lib, encoding="utf-8")

engine_path = Path("bridge-core/src/engine.rs")
engine = engine_path.read_text(encoding="utf-8")
if engine.count("verify_index(&base_index_path)") != 1:
    raise SystemExit("engine: expected exactly one base verify_index call")
engine = replace_once(
    engine,
    "initialize_generation_from_built_index",
    "initialize_generation_from_verified_built_index",
    "engine adoption import",
)
engine = replace_once(
    engine,
    "publish_vnext_incremental_generation, recommend_system_build_tuning, verify_generation,\n    verify_index, verify_positional2_sidecars, verify_positional3_sidecars,",
    "publish_vnext_incremental_generation, recommend_system_build_tuning,\n    verify_built_index_for_generation_adoption, verify_generation_structure,\n    verify_positional2_sidecars, verify_positional3_sidecars,",
    "engine verify imports",
)
engine = replace_once(
    engine,
    "        verify_index(&base_index_path).map_err(|error| error.to_string())?;",
    "        let verified_built_index = verify_built_index_for_generation_adoption(&base_index_path)\n            .map_err(|error| error.to_string())?;",
    "engine base verify token",
)
engine = replace_once(
    engine,
    "        initialize_generation_from_verified_built_index(build_dir, &base_index_path, &identities)\n            .map_err(|error| error.to_string())?;",
    "        initialize_generation_from_verified_built_index(build_dir, verified_built_index, &identities)\n            .map_err(|error| error.to_string())?;",
    "engine consume verified token",
)
engine = replace_once(
    engine,
    "        verify_generation(build_dir).map_err(|error| error.to_string())?;",
    "        verify_generation_structure(build_dir).map_err(|error| error.to_string())?;",
    "engine structure verify",
)
engine_path.write_text(engine, encoding="utf-8")

test_path = Path("search-core/tests/verified_generation_adoption.rs")
if test_path.exists():
    raise SystemExit("verified_generation_adoption test already exists")
test_path.write_text(r'''use std::fs::{self, OpenOptions};
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
    let mut file = OpenOptions::new().read(true).write(true).open(path).unwrap();
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
    assert_eq!(MergedIndex::open(&store, false).unwrap().live_docs(), identities.len());
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
''', encoding="utf-8")

print("verified generation fast path source transformation applied")
