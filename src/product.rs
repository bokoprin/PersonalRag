use crate::extraction::{ExtractorConfig, is_extractable_document};
use crate::incremental::{
    BundleManifest, DeltaSnapshot, IncrementalError, IncrementalState, LoadedBundle,
    compact_and_commit_with_extraction, gc_bundles_with_verification,
    load_bundle_with_verification, next_generation_number, write_bundle, write_delta_generation,
    write_metadata_generation, write_state_generation,
};
use crate::metadata::{MetadataError, MetadataFileKind, MetadataIndex, MetadataRecord};
use crate::persistent::{PersistentError, publish_generation_with_extraction};
use crate::usn::UsnCheckpoint;
use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[derive(Debug)]
pub enum ProductError {
    Io(io::Error),
    Metadata(MetadataError),
    Persistent(PersistentError),
    Incremental(IncrementalError),
    InvalidArgument(String),
    Unsupported(String),
}

impl fmt::Display for ProductError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::Metadata(error) => write!(f, "metadata error: {error}"),
            Self::Persistent(error) => write!(f, "persistent error: {error}"),
            Self::Incremental(error) => write!(f, "incremental error: {error}"),
            Self::InvalidArgument(message) => f.write_str(message),
            Self::Unsupported(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for ProductError {}

impl From<io::Error> for ProductError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<MetadataError> for ProductError {
    fn from(value: MetadataError) -> Self {
        Self::Metadata(value)
    }
}
impl From<PersistentError> for ProductError {
    fn from(value: PersistentError) -> Self {
        Self::Persistent(value)
    }
}
impl From<IncrementalError> for ProductError {
    fn from(value: IncrementalError) -> Self {
        Self::Incremental(value)
    }
}

pub type Result<T> = std::result::Result<T, ProductError>;

#[derive(Clone, Debug)]
pub struct InitReport {
    pub manifest: BundleManifest,
    pub metadata_records: usize,
    pub searchable_records: usize,
    pub store_bytes: u64,
    pub checkpoint: UsnCheckpoint,
    pub usn_available: bool,
}

#[derive(Clone, Debug)]
pub struct ReconcileReport {
    pub committed: bool,
    pub compacted: bool,
    pub manifest: BundleManifest,
    pub delta_changes: usize,
    pub metadata_records: usize,
}

pub fn initialize_store(
    root: impl AsRef<Path>,
    store: impl AsRef<Path>,
    extractor: &ExtractorConfig,
) -> Result<InitReport> {
    let root = absolute_existing_dir(root.as_ref())?;
    let store = absolute_path(store.as_ref())?;
    validate_root_store(&root, &store)?;
    if store.join("BUNDLE_CURRENT").exists() {
        return Err(ProductError::InvalidArgument(format!(
            "store already contains a PersonalRag bundle: {}",
            store.display()
        )));
    }
    fs::create_dir_all(&store)?;

    // Capture the journal position before the potentially long initial scan so
    // changes that race with indexing remain visible to the live producer.
    let (checkpoint, usn_available) = initial_checkpoint(&root);
    let records = scan_metadata_records(&root)?;
    let searchable_records = records
        .iter()
        .filter(|record| record.content_searchable)
        .count();
    let metadata = MetadataIndex::build(records)?;
    let generation = 1_u64;
    let content = publish_generation_with_extraction(&root, &store, generation, 0, extractor)?;
    write_metadata_generation(&store, generation, &metadata)?;
    write_delta_generation(
        &store,
        &DeltaSnapshot {
            generation,
            parent_generation: 0,
            upserts: Vec::new(),
            tombstones: Vec::new(),
        },
    )?;
    write_state_generation(
        &store,
        &IncrementalState {
            generation,
            checkpoint,
            pending_renames: Vec::new(),
        },
    )?;
    let manifest = BundleManifest {
        generation,
        parent_generation: 0,
        content_generation: content.generation,
        metadata_generation: generation,
        delta_generation: generation,
        state_generation: generation,
    };
    write_bundle(&store, manifest)?;
    let loaded = load_bundle_with_verification(&root, &store, extractor)?;
    if loaded.manifest != manifest {
        return Err(ProductError::InvalidArgument(
            "newly initialized bundle did not reload as the current valid bundle".to_string(),
        ));
    }
    Ok(InitReport {
        manifest,
        metadata_records: metadata.records().len(),
        searchable_records,
        store_bytes: directory_file_bytes(&store)?,
        checkpoint,
        usn_available,
    })
}

pub fn reconcile_store(
    root: impl AsRef<Path>,
    store: impl AsRef<Path>,
    extractor: &ExtractorConfig,
    checkpoint: Option<UsnCheckpoint>,
) -> Result<ReconcileReport> {
    let root = absolute_existing_dir(root.as_ref())?;
    let store = absolute_existing_dir(store.as_ref())?;
    validate_root_store(&root, &store)?;
    let mut loaded = load_bundle_with_verification(&root, &store, extractor)?;
    let before = loaded.delta.snapshot();
    let observed = scan_metadata_records(&root)?;
    loaded.delta.reconcile(&loaded.metadata, observed);
    let after = loaded.delta.snapshot();
    let changed = before.upserts != after.upserts || before.tombstones != after.tombstones;
    let mut checkpoint_changed = false;
    if let Some(checkpoint) = checkpoint {
        checkpoint_changed =
            loaded.state.checkpoint != checkpoint || !loaded.state.pending_renames.is_empty();
        loaded.state.checkpoint = checkpoint;
        loaded.state.pending_renames.clear();
    }
    if !changed && !checkpoint_changed {
        return Ok(ReconcileReport {
            committed: false,
            compacted: false,
            manifest: loaded.manifest,
            delta_changes: loaded.delta.change_count(),
            metadata_records: loaded.delta.materialize_records(&loaded.metadata).len(),
        });
    }

    let compacted = changed && loaded.delta.should_compact(loaded.metadata.records().len());
    let manifest = if compacted {
        compact_and_commit_with_extraction(&root, &store, &loaded, extractor)?
    } else if changed {
        commit_overlay_bundle(&store, &loaded)?
    } else {
        commit_state_bundle(&store, &loaded)?
    };
    let _ = gc_bundles_with_verification(&root, &store, 2, extractor);
    let reloaded = load_bundle_with_verification(&root, &store, extractor)?;
    if reloaded.manifest != manifest {
        return Err(ProductError::InvalidArgument(
            "committed bundle did not reload as current".to_string(),
        ));
    }
    Ok(ReconcileReport {
        committed: true,
        compacted,
        manifest,
        delta_changes: reloaded.delta.change_count(),
        metadata_records: reloaded.delta.materialize_records(&reloaded.metadata).len(),
    })
}

pub fn load_product_bundle(
    root: impl AsRef<Path>,
    store: impl AsRef<Path>,
    extractor: &ExtractorConfig,
) -> Result<LoadedBundle> {
    Ok(load_bundle_with_verification(root, store, extractor)?)
}

fn commit_overlay_bundle(store: &Path, loaded: &LoadedBundle) -> Result<BundleManifest> {
    let next = next_generation_number(store)?;
    let mut snapshot = loaded.delta.snapshot();
    snapshot.generation = next;
    snapshot.parent_generation = loaded.manifest.delta_generation;
    write_delta_generation(store, &snapshot)?;
    write_state_generation(
        store,
        &IncrementalState {
            generation: next,
            checkpoint: loaded.state.checkpoint,
            pending_renames: loaded.state.pending_renames.clone(),
        },
    )?;
    let manifest = BundleManifest {
        generation: next,
        parent_generation: loaded.manifest.generation,
        content_generation: loaded.manifest.content_generation,
        metadata_generation: loaded.manifest.metadata_generation,
        delta_generation: next,
        state_generation: next,
    };
    write_bundle(store, manifest)?;
    Ok(manifest)
}

fn commit_state_bundle(store: &Path, loaded: &LoadedBundle) -> Result<BundleManifest> {
    let next = next_generation_number(store)?;
    write_state_generation(
        store,
        &IncrementalState {
            generation: next,
            checkpoint: loaded.state.checkpoint,
            pending_renames: loaded.state.pending_renames.clone(),
        },
    )?;
    let manifest = BundleManifest {
        generation: next,
        parent_generation: loaded.manifest.generation,
        content_generation: loaded.manifest.content_generation,
        metadata_generation: loaded.manifest.metadata_generation,
        delta_generation: loaded.manifest.delta_generation,
        state_generation: next,
    };
    write_bundle(store, manifest)?;
    Ok(manifest)
}

pub fn scan_metadata_records(root: &Path) -> Result<Vec<MetadataRecord>> {
    let root = absolute_existing_dir(root)?;
    let mut records = Vec::new();
    let mut used_ids = HashSet::new();
    scan_dir(&root, &root, &mut records, &mut used_ids)?;
    records.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(records)
}

fn scan_dir(
    root: &Path,
    dir: &Path,
    records: &mut Vec<MetadataRecord>,
    used_ids: &mut HashSet<u64>,
) -> Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() && should_skip_dir(root, &path) {
            continue;
        }
        let metadata = fs::symlink_metadata(&path)?;
        let relative = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        let kind = if file_type.is_file() {
            MetadataFileKind::File
        } else if file_type.is_dir() {
            MetadataFileKind::Directory
        } else if file_type.is_symlink() {
            MetadataFileKind::Symlink
        } else {
            MetadataFileKind::Other
        };
        let mut file_id = platform_file_id(&path, &metadata)?;
        if !used_ids.insert(file_id) {
            file_id = disambiguated_file_id(file_id, &relative, used_ids);
            used_ids.insert(file_id);
        }
        let searchable = file_type.is_file()
            && (crate::is_searchable_path(&relative) || is_extractable_document(&relative));
        records.push(MetadataRecord {
            file_id,
            path: relative,
            source_root: 0,
            size: if file_type.is_file() {
                metadata.len()
            } else {
                0
            },
            modified_ns: modified_ns(&metadata),
            kind,
            content_searchable: searchable,
            extractable: file_type.is_file() && is_extractable_document(&path),
        });
        if file_type.is_dir() {
            scan_dir(root, &path, records, used_ids)?;
        }
    }
    Ok(())
}

fn should_skip_dir(root: &Path, path: &Path) -> bool {
    path != root
        && matches!(
            path.file_name().and_then(|value| value.to_str()),
            Some(".git" | "target" | "node_modules" | ".venv" | "venv" | "dist" | "build")
        )
}

fn modified_ns(metadata: &fs::Metadata) -> u128 {
    metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_nanos())
        .unwrap_or(0)
}

fn disambiguated_file_id(base: u64, path: &Path, used: &HashSet<u64>) -> u64 {
    let mut salt = 0_u64;
    loop {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64 ^ base ^ salt;
        for byte in path.to_string_lossy().as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        if hash != 0 && !used.contains(&hash) {
            return hash;
        }
        salt = salt.wrapping_add(1);
    }
}

fn absolute_existing_dir(path: &Path) -> Result<PathBuf> {
    if !path.is_dir() {
        return Err(ProductError::InvalidArgument(format!(
            "directory does not exist: {}",
            path.display()
        )));
    }
    Ok(fs::canonicalize(path)?)
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn validate_root_store(root: &Path, store: &Path) -> Result<()> {
    if store.starts_with(root) {
        return Err(ProductError::InvalidArgument(format!(
            "index store must be outside the indexed root: root={} store={}",
            root.display(),
            store.display()
        )));
    }
    Ok(())
}

fn directory_file_bytes(root: &Path) -> Result<u64> {
    let mut total = 0_u64;
    if !root.exists() {
        return Ok(0);
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            if metadata.is_dir() {
                stack.push(entry.path());
            } else if metadata.is_file() {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    Ok(total)
}

#[cfg(unix)]
fn platform_file_id(_path: &Path, metadata: &fs::Metadata) -> Result<u64> {
    use std::os::unix::fs::MetadataExt;
    let ino = metadata.ino();
    let dev = metadata.dev();
    Ok(ino ^ dev.rotate_left(17))
}

#[cfg(windows)]
fn platform_file_id(path: &Path, _metadata: &fs::Metadata) -> Result<u64> {
    windows_file_identity(path).map_err(ProductError::Io)
}

#[cfg(not(any(unix, windows)))]
fn platform_file_id(path: &Path, _metadata: &fs::Metadata) -> Result<u64> {
    Ok(disambiguated_file_id(0x5052_5632, path, &HashSet::new()))
}

#[cfg(windows)]
fn windows_file_identity(path: &Path) -> io::Result<u64> {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::null_mut;

    type Handle = *mut c_void;
    const INVALID_HANDLE_VALUE: Handle = -1_isize as Handle;
    const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const OPEN_EXISTING: u32 = 3;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;

    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    #[repr(C)]
    struct ByHandleFileInformation {
        file_attributes: u32,
        creation_time: FileTime,
        last_access_time: FileTime,
        last_write_time: FileTime,
        volume_serial_number: u32,
        file_size_high: u32,
        file_size_low: u32,
        number_of_links: u32,
        file_index_high: u32,
        file_index_low: u32,
    }
    unsafe extern "system" {
        fn CreateFileW(
            name: *const u16,
            access: u32,
            share: u32,
            security: *mut c_void,
            creation: u32,
            flags: u32,
            template: Handle,
        ) -> Handle;
        fn GetFileInformationByHandle(
            handle: Handle,
            information: *mut ByHandleFileInformation,
        ) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
    }

    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    wide.push(0);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let mut info = ByHandleFileInformation {
        file_attributes: 0,
        creation_time: FileTime { low: 0, high: 0 },
        last_access_time: FileTime { low: 0, high: 0 },
        last_write_time: FileTime { low: 0, high: 0 },
        volume_serial_number: 0,
        file_size_high: 0,
        file_size_low: 0,
        number_of_links: 0,
        file_index_high: 0,
        file_index_low: 0,
    };
    let ok = unsafe { GetFileInformationByHandle(handle, &mut info) };
    let error = (ok == 0).then(io::Error::last_os_error);
    unsafe { CloseHandle(handle) };
    if let Some(error) = error {
        return Err(error);
    }
    Ok((u64::from(info.file_index_high) << 32) | u64::from(info.file_index_low))
}

#[cfg(windows)]
fn initial_checkpoint(root: &Path) -> (UsnCheckpoint, bool) {
    let Ok(volume) = windows_volume_device(root) else {
        return (
            UsnCheckpoint {
                journal_id: 0,
                next_usn: 0,
            },
            false,
        );
    };
    let Ok(handle) = crate::windows_usn::live::VolumeHandle::open(&volume) else {
        return (
            UsnCheckpoint {
                journal_id: 0,
                next_usn: 0,
            },
            false,
        );
    };
    match handle.query_journal() {
        Ok(bounds) => (
            UsnCheckpoint {
                journal_id: bounds.journal_id,
                next_usn: bounds.next_usn,
            },
            true,
        ),
        Err(_) => (
            UsnCheckpoint {
                journal_id: 0,
                next_usn: 0,
            },
            false,
        ),
    }
}

#[cfg(not(windows))]
fn initial_checkpoint(_root: &Path) -> (UsnCheckpoint, bool) {
    (
        UsnCheckpoint {
            journal_id: 0,
            next_usn: 0,
        },
        false,
    )
}

#[cfg(windows)]
pub fn windows_volume_device(root: &Path) -> Result<String> {
    use std::path::{Component, Prefix};
    let root = fs::canonicalize(root)?;
    let prefix = root.components().next().ok_or_else(|| {
        ProductError::Unsupported(format!(
            "cannot determine Windows volume for {}",
            root.display()
        ))
    })?;
    let Component::Prefix(prefix) = prefix else {
        return Err(ProductError::Unsupported(format!(
            "indexed root is not on a drive-letter volume: {}",
            root.display()
        )));
    };
    let letter = match prefix.kind() {
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => char::from(letter),
        _ => {
            return Err(ProductError::Unsupported(format!(
                "only drive-letter NTFS roots are supported by the USN producer: {}",
                root.display()
            )));
        }
    };
    Ok(format!(r"\\.\{}:", letter.to_ascii_uppercase()))
}

#[cfg(windows)]
pub struct WindowsUsnProducer {
    root: PathBuf,
    store: PathBuf,
    extractor: ExtractorConfig,
    volume: crate::windows_usn::live::VolumeHandle,
    checkpoint: UsnCheckpoint,
    root_file_id: u64,
}

#[cfg(windows)]
impl WindowsUsnProducer {
    pub fn open(
        root: impl AsRef<Path>,
        store: impl AsRef<Path>,
        extractor: ExtractorConfig,
    ) -> Result<Self> {
        let root = absolute_existing_dir(root.as_ref())?;
        let store = absolute_existing_dir(store.as_ref())?;
        validate_root_store(&root, &store)?;
        let loaded = load_bundle_with_verification(&root, &store, &extractor)?;
        let device = windows_volume_device(&root)?;
        let volume = crate::windows_usn::live::VolumeHandle::open(&device)?;
        let bounds = volume.query_journal()?;
        let mut checkpoint = loaded.state.checkpoint;
        if crate::usn::validate_checkpoint(checkpoint, bounds)
            == crate::usn::CheckpointStatus::ReconcileRequired
        {
            checkpoint = UsnCheckpoint {
                journal_id: bounds.journal_id,
                next_usn: bounds.next_usn,
            };
            let _ = reconcile_store(&root, &store, &extractor, Some(checkpoint))?;
        }
        let root_file_id = windows_file_identity(&root)?;
        Ok(Self {
            root,
            store,
            extractor,
            volume,
            checkpoint,
            root_file_id,
        })
    }

    pub fn checkpoint(&self) -> UsnCheckpoint {
        self.checkpoint
    }

    pub fn poll_once(&mut self) -> Result<Option<ReconcileReport>> {
        let bounds = self.volume.query_journal()?;
        if crate::usn::validate_checkpoint(self.checkpoint, bounds)
            == crate::usn::CheckpointStatus::ReconcileRequired
        {
            self.checkpoint = UsnCheckpoint {
                journal_id: bounds.journal_id,
                next_usn: bounds.next_usn,
            };
            return Ok(Some(reconcile_store(
                &self.root,
                &self.store,
                &self.extractor,
                Some(self.checkpoint),
            )?));
        }

        let (next_usn, records) = self.volume.read_journal(
            self.checkpoint.next_usn,
            self.checkpoint.journal_id,
            u32::MAX,
            1024 * 1024,
        )?;
        self.checkpoint.next_usn = next_usn;
        if records.is_empty() {
            return Ok(None);
        }
        let loaded = load_bundle_with_verification(&self.root, &self.store, &self.extractor)?;
        let materialized = loaded.delta.materialize_records(&loaded.metadata);
        let known = materialized
            .iter()
            .map(|record| record.file_id)
            .collect::<HashSet<_>>();
        let relevant = records.iter().any(|record| {
            known.contains(&record.file_reference)
                || record.parent_reference == self.root_file_id
                || known.contains(&record.parent_reference)
        });
        if !relevant {
            return Ok(None);
        }
        Ok(Some(reconcile_store(
            &self.root,
            &self.store,
            &self.extractor,
            Some(self.checkpoint),
        )?))
    }
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowsWatchMode {
    Usn,
    DirectoryNotification,
}

#[cfg(windows)]
impl WindowsWatchMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Usn => "usn",
            Self::DirectoryNotification => "directory-notify",
        }
    }
}

#[cfg(windows)]
enum WindowsWatchInner {
    Usn(WindowsUsnProducer),
    DirectoryNotification {
        root: PathBuf,
        store: PathBuf,
        extractor: ExtractorConfig,
        notification: crate::windows_watch::live::ChangeNotification,
        checkpoint: UsnCheckpoint,
        fallback_reason: String,
    },
}

#[cfg(windows)]
pub struct WindowsWatchProducer {
    inner: WindowsWatchInner,
}

#[cfg(windows)]
impl WindowsWatchProducer {
    pub fn open(
        root: impl AsRef<Path>,
        store: impl AsRef<Path>,
        extractor: ExtractorConfig,
    ) -> Result<Self> {
        let root = absolute_existing_dir(root.as_ref())?;
        let store = absolute_existing_dir(store.as_ref())?;
        validate_root_store(&root, &store)?;
        let loaded = load_bundle_with_verification(&root, &store, &extractor)?;

        match WindowsUsnProducer::open(&root, &store, extractor.clone()) {
            Ok(producer) => Ok(Self {
                inner: WindowsWatchInner::Usn(producer),
            }),
            Err(usn_error) => {
                let notification =
                    crate::windows_watch::live::ChangeNotification::open(&root).map_err(
                        |fallback_error| {
                            ProductError::Unsupported(format!(
                                "USN watch unavailable ({usn_error}); directory notification fallback unavailable: {fallback_error}"
                            ))
                        },
                    )?;
                Ok(Self {
                    inner: WindowsWatchInner::DirectoryNotification {
                        root,
                        store,
                        extractor,
                        notification,
                        checkpoint: loaded.state.checkpoint,
                        fallback_reason: usn_error.to_string(),
                    },
                })
            }
        }
    }

    pub fn mode(&self) -> WindowsWatchMode {
        match self.inner {
            WindowsWatchInner::Usn(_) => WindowsWatchMode::Usn,
            WindowsWatchInner::DirectoryNotification { .. } => {
                WindowsWatchMode::DirectoryNotification
            }
        }
    }

    pub fn fallback_reason(&self) -> Option<&str> {
        match &self.inner {
            WindowsWatchInner::Usn(_) => None,
            WindowsWatchInner::DirectoryNotification {
                fallback_reason, ..
            } => Some(fallback_reason),
        }
    }

    pub fn checkpoint(&self) -> UsnCheckpoint {
        match &self.inner {
            WindowsWatchInner::Usn(producer) => producer.checkpoint(),
            WindowsWatchInner::DirectoryNotification { checkpoint, .. } => *checkpoint,
        }
    }

    pub fn poll_once(&mut self) -> Result<Option<ReconcileReport>> {
        match &mut self.inner {
            WindowsWatchInner::Usn(producer) => producer.poll_once(),
            WindowsWatchInner::DirectoryNotification {
                root,
                store,
                extractor,
                notification,
                ..
            } => {
                if !notification.poll_changed()? {
                    return Ok(None);
                }
                let report = reconcile_store(root, store, extractor, None)?;
                Ok(report.committed.then_some(report))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "personalrag-product-{tag}-{}-{stamp}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn metadata_scan_keeps_all_files_but_marks_only_supported_content() {
        let root = temp_dir("scan");
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("docs/a.txt"), b"hello").unwrap();
        fs::write(root.join("docs/blob.bin"), b"\x00\x01").unwrap();
        let records = scan_metadata_records(&root).unwrap();
        assert!(records.iter().any(|r| r.path.ends_with("docs")));
        let text = records.iter().find(|r| r.path.ends_with("a.txt")).unwrap();
        let blob = records
            .iter()
            .find(|r| r.path.ends_with("blob.bin"))
            .unwrap();
        assert!(text.content_searchable);
        assert!(!blob.content_searchable);
        fs::remove_dir_all(root).unwrap();
    }
}
