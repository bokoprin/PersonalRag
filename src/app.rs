use crate::metadata::{MetadataFileKind, MetadataIndex, MetadataRecord, MetadataSearchRequest};
use crate::persistent::crc64_ecma;
use crate::product;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub const METADATA_CHECKPOINT_RECORDS: usize = 4_096;
const METADATA_CHECKPOINT_DIRS: usize = 128;
const QUEUE_MAGIC: &[u8; 8] = b"PRV2MQ01";

#[derive(Debug)]
pub enum AppError {
    Io(io::Error),
    Metadata(crate::metadata::MetadataError),
    Product(crate::product::ProductError),
    InvalidState(String),
    Unsupported(String),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::Metadata(error) => write!(f, "metadata error: {error}"),
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

impl From<crate::product::ProductError> for AppError {
    fn from(value: crate::product::ProductError) -> Self {
        Self::Product(value)
    }
}

pub type Result<T> = std::result::Result<T, AppError>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct VolumeKey(pub String);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredVolume {
    pub key: VolumeKey,
    pub mount: PathBuf,
    pub serial: u32,
}

#[derive(Clone, Debug)]
pub struct AppPaths {
    pub root: PathBuf,
    pub volumes: PathBuf,
    pub catalog: PathBuf,
}

impl AppPaths {
    pub fn for_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            volumes: root.join("volumes"),
            catalog: root.join("catalog"),
            root,
        }
    }

    pub fn default_for_current_user() -> Result<Self> {
        #[cfg(windows)]
        {
            let base = std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            return Ok(Self::for_root(base.join("PersonalRag")));
        }
        #[cfg(not(windows))]
        {
            if let Some(base) = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
                return Ok(Self::for_root(base.join("PersonalRag")));
            }
            Ok(Self::for_root(std::env::temp_dir().join("PersonalRag")))
        }
    }

    pub fn ensure(&self) -> Result<()> {
        fs::create_dir_all(&self.volumes)?;
        fs::create_dir_all(&self.catalog)?;
        Ok(())
    }

    pub fn volume_store(&self, key: &VolumeKey) -> PathBuf {
        self.volumes
            .join(format!("{:016x}", crc64_ecma(key.0.as_bytes())))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumePhase {
    Discovered,
    MetadataBuilding,
    MetadataReady,
    ContentBuilding,
    ContentCatchUp,
    Ready,
    Degraded,
}

impl VolumePhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Discovered => "discovered",
            Self::MetadataBuilding => "metadata-building",
            Self::MetadataReady => "metadata-ready",
            Self::ContentBuilding => "content-building",
            Self::ContentCatchUp => "content-catch-up",
            Self::Ready => "ready",
            Self::Degraded => "degraded",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "discovered" => Ok(Self::Discovered),
            "metadata-building" => Ok(Self::MetadataBuilding),
            "metadata-ready" => Ok(Self::MetadataReady),
            "content-building" => Ok(Self::ContentBuilding),
            "content-catch-up" => Ok(Self::ContentCatchUp),
            "ready" => Ok(Self::Ready),
            "degraded" => Ok(Self::Degraded),
            _ => Err(AppError::InvalidState(format!(
                "unknown volume phase: {value}"
            ))),
        }
    }
}

#[derive(Clone, Debug)]
pub struct VolumeManifest {
    pub generation: u64,
    pub key: VolumeKey,
    pub mount: PathBuf,
    pub phase: VolumePhase,
    pub metadata_generation: u64,
    pub metadata_file: Option<String>,
    pub metadata_records: usize,
    pub inaccessible_directories: usize,
}

impl VolumeManifest {
    fn initial(volume: &DiscoveredVolume) -> Self {
        Self {
            generation: 0,
            key: volume.key.clone(),
            mount: volume.mount.clone(),
            phase: VolumePhase::Discovered,
            metadata_generation: 0,
            metadata_file: None,
            metadata_records: 0,
            inaccessible_directories: 0,
        }
    }
}

#[derive(Clone, Debug)]
struct MetadataBuildState {
    generation: u64,
    segment_count: u64,
    queue_generation: u64,
    records: usize,
    inaccessible_directories: usize,
}

#[derive(Clone, Debug)]
pub struct MetadataBuildReport {
    pub complete: bool,
    pub committed_segments: u64,
    pub metadata_records: usize,
    pub inaccessible_directories: usize,
    pub manifest: Option<VolumeManifest>,
}

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
            let path_query = full_path.and_then(|query| path_query_for_volume(query, &volume.mount));
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
                &[self.paths.root.clone()],
                METADATA_CHECKPOINT_RECORDS,
                &mut should_stop,
            )?;
            reports.push(report);
        }
        Ok(reports)
    }
}

pub fn build_or_resume_metadata<F>(
    app_paths: &AppPaths,
    volume: &DiscoveredVolume,
    exclusions: &[PathBuf],
    checkpoint_records: usize,
    should_stop: &mut F,
) -> Result<MetadataBuildReport>
where
    F: FnMut() -> bool,
{
    app_paths.ensure()?;
    let volume_store = app_paths.volume_store(&volume.key);
    let build_dir = volume_store.join("metadata-build");
    let metadata_dir = volume_store.join("metadata");
    fs::create_dir_all(&build_dir)?;
    fs::create_dir_all(&metadata_dir)?;

    let current_manifest =
        load_volume_manifest(&volume_store)?.unwrap_or_else(|| VolumeManifest::initial(volume));
    if matches!(
        current_manifest.phase,
        VolumePhase::MetadataReady
            | VolumePhase::ContentBuilding
            | VolumePhase::ContentCatchUp
            | VolumePhase::Ready
    ) && current_manifest.metadata_file.is_some()
    {
        return Ok(MetadataBuildReport {
            complete: true,
            committed_segments: 0,
            metadata_records: current_manifest.metadata_records,
            inaccessible_directories: current_manifest.inaccessible_directories,
            manifest: Some(current_manifest),
        });
    }

    let (mut state, mut queue) = match load_metadata_build_state(&build_dir)? {
        Some((state, queue)) => (state, queue),
        None => {
            let queue = VecDeque::from([PathBuf::new()]);
            let state = MetadataBuildState {
                generation: 1,
                segment_count: 0,
                queue_generation: 1,
                records: 0,
                inaccessible_directories: 0,
            };
            persist_build_checkpoint(&build_dir, &state, &queue)?;
            (state, queue)
        }
    };

    let mut used_ids = HashSet::new();
    for segment in 0..state.segment_count {
        let index = MetadataIndex::load_snapshot(segment_path(&build_dir, segment))?;
        for record in index.records() {
            used_ids.insert(record.file_id);
        }
    }

    let threshold = checkpoint_records.max(1);
    while !queue.is_empty() {
        if should_stop() {
            return Ok(MetadataBuildReport {
                complete: false,
                committed_segments: state.segment_count,
                metadata_records: state.records,
                inaccessible_directories: state.inaccessible_directories,
                manifest: None,
            });
        }

        let mut batch_records = Vec::new();
        let mut directories = 0_usize;
        while !queue.is_empty()
            && batch_records.len() < threshold
            && directories < METADATA_CHECKPOINT_DIRS
        {
            let relative_dir = queue.pop_front().expect("queue checked non-empty");
            directories += 1;
            let absolute_dir = volume.mount.join(&relative_dir);
            if is_excluded(&absolute_dir, exclusions) {
                continue;
            }
            let read_dir = match fs::read_dir(&absolute_dir) {
                Ok(value) => value,
                Err(_) => {
                    state.inaccessible_directories =
                        state.inaccessible_directories.saturating_add(1);
                    continue;
                }
            };
            let mut entries = read_dir.filter_map(std::result::Result::ok).collect::<Vec<_>>();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                if is_excluded(&path, exclusions) {
                    continue;
                }
                let file_type = match entry.file_type() {
                    Ok(value) => value,
                    Err(_) => continue,
                };
                let metadata = match fs::symlink_metadata(&path) {
                    Ok(value) => value,
                    Err(_) => continue,
                };
                let relative = path
                    .strip_prefix(&volume.mount)
                    .unwrap_or(&path)
                    .to_path_buf();
                let kind = if file_type.is_file() {
                    MetadataFileKind::File
                } else if file_type.is_dir() {
                    MetadataFileKind::Directory
                } else if file_type.is_symlink() {
                    MetadataFileKind::Symlink
                } else {
                    MetadataFileKind::Other
                };

                if let Ok(mut file_id) = product::platform_file_id(&path, &metadata) {
                    if !used_ids.insert(file_id) {
                        file_id = product::disambiguated_file_id(file_id, &relative, &used_ids);
                        used_ids.insert(file_id);
                    }
                    let searchable = file_type.is_file()
                        && (crate::is_searchable_path(&relative)
                            || crate::extraction::is_extractable_document(&relative));
                    batch_records.push(MetadataRecord {
                        file_id,
                        path: relative.clone(),
                        source_root: 0,
                        size: if file_type.is_file() {
                            metadata.len()
                        } else {
                            0
                        },
                        modified_ns: metadata_modified_ns(&metadata),
                        kind,
                        content_searchable: searchable,
                        extractable: file_type.is_file()
                            && crate::extraction::is_extractable_document(&relative),
                    });
                }

                if file_type.is_dir() && !is_directory_reparse_point(&metadata) {
                    queue.push_back(relative);
                }
            }
        }

        if !batch_records.is_empty() {
            let segment = MetadataIndex::build(batch_records)?;
            let path = segment_path(&build_dir, state.segment_count);
            if path.exists() {
                fs::remove_file(&path)?;
            }
            segment.write_snapshot(&path)?;
            state.segment_count = state.segment_count.saturating_add(1);
            state.records = state.records.saturating_add(segment.records().len());
        }

        state.generation = state.generation.saturating_add(1);
        state.queue_generation = state.generation;
        persist_build_checkpoint(&build_dir, &state, &queue)?;
    }

    let mut all_records = Vec::with_capacity(state.records);
    for segment in 0..state.segment_count {
        let index = MetadataIndex::load_snapshot(segment_path(&build_dir, segment))?;
        all_records.extend_from_slice(index.records());
    }
    all_records.sort_by(|left, right| left.path.cmp(&right.path));
    let metadata = MetadataIndex::build(all_records)?;

    let mut metadata_generation = current_manifest.metadata_generation.saturating_add(1).max(1);
    let metadata_file = loop {
        let name = format!("metadata-{metadata_generation:020}.prmet");
        let path = metadata_dir.join(&name);
        if !path.exists() {
            metadata.write_snapshot(&path)?;
            break name;
        }
        metadata_generation = metadata_generation.saturating_add(1);
    };

    let manifest = VolumeManifest {
        generation: current_manifest.generation.saturating_add(1).max(1),
        key: volume.key.clone(),
        mount: volume.mount.clone(),
        phase: VolumePhase::MetadataReady,
        metadata_generation,
        metadata_file: Some(metadata_file),
        metadata_records: metadata.records().len(),
        inaccessible_directories: state.inaccessible_directories,
    };
    write_volume_manifest(&volume_store, &manifest)?;
    let _ = fs::remove_dir_all(&build_dir);

    Ok(MetadataBuildReport {
        complete: true,
        committed_segments: state.segment_count,
        metadata_records: manifest.metadata_records,
        inaccessible_directories: manifest.inaccessible_directories,
        manifest: Some(manifest),
    })
}

fn metadata_modified_ns(metadata: &fs::Metadata) -> u128 {
    metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_nanos())
        .unwrap_or(0)
}

fn is_excluded(path: &Path, exclusions: &[PathBuf]) -> bool {
    exclusions.iter().any(|excluded| path.starts_with(excluded))
}

#[cfg(windows)]
fn is_directory_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_directory_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

fn segment_path(build_dir: &Path, segment: u64) -> PathBuf {
    build_dir.join(format!("segment-{segment:020}.prmet"))
}

fn persist_build_checkpoint(
    build_dir: &Path,
    state: &MetadataBuildState,
    queue: &VecDeque<PathBuf>,
) -> Result<()> {
    let queue_name = format!("queue-{:020}.bin", state.queue_generation);
    write_path_queue(&build_dir.join(&queue_name), queue)?;
    let state_path = build_dir.join(format!("build-{:020}.state", state.generation));
    let content = format!(
        "version=1\nsegment_count={}\nqueue_generation={}\nrecords={}\ninaccessible={}\n",
        state.segment_count, state.queue_generation, state.records, state.inaccessible_directories
    );
    atomic_write_new(&state_path, content.as_bytes())?;
    Ok(())
}

fn load_metadata_build_state(
    build_dir: &Path,
) -> Result<Option<(MetadataBuildState, VecDeque<PathBuf>)>> {
    let mut states = numbered_files(build_dir, "build-", ".state")?;
    states.sort_unstable_by_key(|(generation, _)| std::cmp::Reverse(*generation));
    for (generation, path) in states {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let values = parse_key_values(&text);
        let Some(segment_count) = parse_u64(&values, "segment_count") else {
            continue;
        };
        let Some(queue_generation) = parse_u64(&values, "queue_generation") else {
            continue;
        };
        let Some(records) = parse_u64(&values, "records") else {
            continue;
        };
        let Some(inaccessible) = parse_u64(&values, "inaccessible") else {
            continue;
        };
        let queue_path = build_dir.join(format!("queue-{queue_generation:020}.bin"));
        let Ok(queue) = read_path_queue(&queue_path) else {
            continue;
        };
        if (0..segment_count).any(|segment| !segment_path(build_dir, segment).exists()) {
            continue;
        }
        return Ok(Some((
            MetadataBuildState {
                generation,
                segment_count,
                queue_generation,
                records: records as usize,
                inaccessible_directories: inaccessible as usize,
            },
            queue,
        )));
    }
    Ok(None)
}

fn write_path_queue(path: &Path, queue: &VecDeque<PathBuf>) -> Result<()> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(QUEUE_MAGIC);
    put_u32(&mut bytes, queue.len() as u32);
    for path in queue {
        let (encoding, payload) = encode_path(path);
        bytes.push(encoding);
        put_u32(&mut bytes, payload.len() as u32);
        bytes.extend_from_slice(&payload);
    }
    atomic_write_new(path, &bytes)?;
    Ok(())
}

fn read_path_queue(path: &Path) -> Result<VecDeque<PathBuf>> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    if bytes.len() < 12 || &bytes[..8] != QUEUE_MAGIC {
        return Err(AppError::InvalidState("invalid metadata queue".to_string()));
    }
    let count = read_u32(&bytes, 8)? as usize;
    let mut offset = 12_usize;
    let mut out = VecDeque::with_capacity(count);
    for _ in 0..count {
        let encoding = *bytes
            .get(offset)
            .ok_or_else(|| AppError::InvalidState("truncated queue encoding".to_string()))?;
        offset += 1;
        let len = read_u32(&bytes, offset)? as usize;
        offset += 4;
        let end = offset
            .checked_add(len)
            .ok_or_else(|| AppError::InvalidState("queue length overflow".to_string()))?;
        let payload = bytes
            .get(offset..end)
            .ok_or_else(|| AppError::InvalidState("truncated queue path".to_string()))?;
        out.push_back(decode_path(encoding, payload)?);
        offset = end;
    }
    if offset != bytes.len() {
        return Err(AppError::InvalidState(
            "metadata queue trailing bytes".to_string(),
        ));
    }
    Ok(out)
}

#[cfg(windows)]
fn encode_path(path: &Path) -> (u8, Vec<u8>) {
    use std::os::windows::ffi::OsStrExt;
    let mut bytes = Vec::new();
    for value in path.as_os_str().encode_wide() {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    (1, bytes)
}

#[cfg(unix)]
fn encode_path(path: &Path) -> (u8, Vec<u8>) {
    use std::os::unix::ffi::OsStrExt;
    (2, path.as_os_str().as_bytes().to_vec())
}

#[cfg(not(any(windows, unix)))]
fn encode_path(path: &Path) -> (u8, Vec<u8>) {
    (3, path.to_string_lossy().as_bytes().to_vec())
}

fn decode_path(encoding: u8, payload: &[u8]) -> Result<PathBuf> {
    match encoding {
        1 => decode_windows_path(payload),
        2 => decode_unix_path(payload),
        3 => Ok(PathBuf::from(
            std::str::from_utf8(payload)
                .map_err(|_| AppError::InvalidState("invalid UTF-8 queue path".to_string()))?,
        )),
        _ => Err(AppError::InvalidState(
            "unknown queue path encoding".to_string(),
        )),
    }
}

#[cfg(windows)]
fn decode_windows_path(payload: &[u8]) -> Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    if !payload.len().is_multiple_of(2) {
        return Err(AppError::InvalidState(
            "invalid UTF-16 queue path".to_string(),
        ));
    }
    let wide = payload
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    Ok(PathBuf::from(OsString::from_wide(&wide)))
}

#[cfg(not(windows))]
fn decode_windows_path(_payload: &[u8]) -> Result<PathBuf> {
    Err(AppError::InvalidState(
        "Windows queue path on non-Windows host".to_string(),
    ))
}

#[cfg(unix)]
fn decode_unix_path(payload: &[u8]) -> Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(payload.to_vec())))
}

#[cfg(not(unix))]
fn decode_unix_path(_payload: &[u8]) -> Result<PathBuf> {
    Err(AppError::InvalidState(
        "Unix queue path on non-Unix host".to_string(),
    ))
}

fn atomic_write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if path.exists() {
        return Ok(());
    }
    let temp = path.with_extension("tmp");
    if temp.exists() {
        fs::remove_file(&temp)?;
    }
    {
        let mut file = File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&temp, path)?;
    sync_directory(path.parent().unwrap_or_else(|| Path::new(".")))?;
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

fn write_volume_manifest(store: &Path, manifest: &VolumeManifest) -> Result<PathBuf> {
    fs::create_dir_all(store)?;
    let path = store.join(format!("volume-{:020}.state", manifest.generation));
    let metadata_file = manifest.metadata_file.as_deref().unwrap_or("");
    let content = format!(
        "version=1\nkey={}\nmount={}\nphase={}\nmetadata_generation={}\nmetadata_file={}\nmetadata_records={}\ninaccessible={}\n",
        hex_encode(manifest.key.0.as_bytes()),
        hex_encode(manifest.mount.to_string_lossy().as_bytes()),
        manifest.phase.as_str(),
        manifest.metadata_generation,
        metadata_file,
        manifest.metadata_records,
        manifest.inaccessible_directories
    );
    atomic_write_new(&path, content.as_bytes())?;
    Ok(path)
}

pub fn load_volume_manifest(store: &Path) -> Result<Option<VolumeManifest>> {
    let mut states = numbered_files(store, "volume-", ".state")?;
    states.sort_unstable_by_key(|(generation, _)| std::cmp::Reverse(*generation));
    for (generation, path) in states {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let values = parse_key_values(&text);
        let Some(key_hex) = values.get("key") else {
            continue;
        };
        let Some(mount_hex) = values.get("mount") else {
            continue;
        };
        let Ok(key_bytes) = hex_decode(key_hex) else {
            continue;
        };
        let Ok(mount_bytes) = hex_decode(mount_hex) else {
            continue;
        };
        let Ok(key) = String::from_utf8(key_bytes) else {
            continue;
        };
        let Ok(mount) = String::from_utf8(mount_bytes) else {
            continue;
        };
        let Some(phase_text) = values.get("phase") else {
            continue;
        };
        let Ok(phase) = VolumePhase::parse(phase_text) else {
            continue;
        };
        let Some(metadata_generation) = parse_u64(&values, "metadata_generation") else {
            continue;
        };
        let Some(metadata_records) = parse_u64(&values, "metadata_records") else {
            continue;
        };
        let Some(inaccessible) = parse_u64(&values, "inaccessible") else {
            continue;
        };
        let metadata_file = values
            .get("metadata_file")
            .filter(|value| !value.is_empty())
            .cloned();
        if let Some(file_name) = metadata_file.as_deref()
            && !store.join("metadata").join(file_name).exists()
        {
            continue;
        }
        return Ok(Some(VolumeManifest {
            generation,
            key: VolumeKey(key),
            mount: PathBuf::from(mount),
            phase,
            metadata_generation,
            metadata_file,
            metadata_records: metadata_records as usize,
            inaccessible_directories: inaccessible as usize,
        }));
    }
    Ok(None)
}

fn write_app_catalog(paths: &AppPaths, volumes: &[DiscoveredVolume]) -> Result<()> {
    fs::create_dir_all(&paths.catalog)?;
    let generation = next_number(&paths.catalog, "catalog-", ".state")?;
    let mut content = String::from("version=1\n");
    for volume in volumes {
        content.push_str("volume=");
        content.push_str(&hex_encode(volume.key.0.as_bytes()));
        content.push(',');
        content.push_str(&hex_encode(volume.mount.to_string_lossy().as_bytes()));
        content.push(',');
        content.push_str(&volume.serial.to_string());
        content.push('\n');
    }
    atomic_write_new(
        &paths
            .catalog
            .join(format!("catalog-{generation:020}.state")),
        content.as_bytes(),
    )?;
    Ok(())
}

fn next_number(dir: &Path, prefix: &str, suffix: &str) -> Result<u64> {
    Ok(numbered_files(dir, prefix, suffix)?
        .into_iter()
        .map(|(generation, _)| generation)
        .max()
        .unwrap_or(0)
        .saturating_add(1)
        .max(1))
}

fn numbered_files(dir: &Path, prefix: &str, suffix: &str) -> Result<Vec<(u64, PathBuf)>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(middle) = name
            .strip_prefix(prefix)
            .and_then(|value| value.strip_suffix(suffix))
            && let Ok(generation) = middle.parse::<u64>()
        {
            out.push((generation, entry.path()));
        }
    }
    Ok(out)
}

fn parse_key_values(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn parse_u64(values: &HashMap<String, String>, key: &str) -> Option<u64> {
    values.get(key)?.parse().ok()
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn hex_decode(value: &str) -> std::result::Result<Vec<u8>, ()> {
    if !value.len().is_multiple_of(2) {
        return Err(());
    }
    let mut out = Vec::with_capacity(value.len() / 2);
    for pair in value.as_bytes().chunks_exact(2) {
        let text = std::str::from_utf8(pair).map_err(|_| ())?;
        out.push(u8::from_str_radix(text, 16).map_err(|_| ())?);
    }
    Ok(out)
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let value = bytes
        .get(offset..offset.saturating_add(4))
        .ok_or_else(|| AppError::InvalidState("truncated u32".to_string()))?;
    Ok(u32::from_le_bytes(
        value
            .try_into()
            .map_err(|_| AppError::InvalidState("invalid u32".to_string()))?,
    ))
}

#[cfg(windows)]
pub fn discover_fixed_volumes() -> Result<Vec<DiscoveredVolume>> {
    use std::ffi::{OsString, c_void};
    use std::os::windows::ffi::OsStringExt;
    use std::ptr::null_mut;

    type Bool = i32;
    const DRIVE_FIXED: u32 = 3;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLogicalDriveStringsW(buffer_length: u32, buffer: *mut u16) -> u32;
        fn GetDriveTypeW(root_path_name: *const u16) -> u32;
        fn GetVolumeNameForVolumeMountPointW(
            volume_mount_point: *const u16,
            volume_name: *mut u16,
            buffer_length: u32,
        ) -> Bool;
        fn GetVolumeInformationW(
            root_path_name: *const u16,
            volume_name_buffer: *mut u16,
            volume_name_size: u32,
            volume_serial_number: *mut u32,
            maximum_component_length: *mut u32,
            file_system_flags: *mut u32,
            file_system_name_buffer: *mut u16,
            file_system_name_size: u32,
        ) -> Bool;
    }

    let needed = unsafe { GetLogicalDriveStringsW(0, null_mut()) };
    if needed == 0 {
        return Err(AppError::Io(io::Error::last_os_error()));
    }
    let mut buffer = vec![0_u16; needed as usize + 1];
    let written = unsafe { GetLogicalDriveStringsW(buffer.len() as u32, buffer.as_mut_ptr()) };
    if written == 0 {
        return Err(AppError::Io(io::Error::last_os_error()));
    }

    let mut volumes = Vec::new();
    let mut start = 0_usize;
    while start < written as usize {
        let Some(relative_end) = buffer[start..].iter().position(|value| *value == 0) else {
            break;
        };
        if relative_end == 0 {
            break;
        }
        let end = start + relative_end;
        let mut root_wide = buffer[start..=end].to_vec();
        if unsafe { GetDriveTypeW(root_wide.as_ptr()) } == DRIVE_FIXED {
            let mount = PathBuf::from(OsString::from_wide(&buffer[start..end]));
            let mut serial = 0_u32;
            let _ = unsafe {
                GetVolumeInformationW(
                    root_wide.as_ptr(),
                    null_mut(),
                    0,
                    &mut serial,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    0,
                )
            };
            let mut guid_buffer = vec![0_u16; 128];
            let has_guid = unsafe {
                GetVolumeNameForVolumeMountPointW(
                    root_wide.as_ptr(),
                    guid_buffer.as_mut_ptr(),
                    guid_buffer.len() as u32,
                )
            } != 0;
            let key = if has_guid {
                let len = guid_buffer
                    .iter()
                    .position(|value| *value == 0)
                    .unwrap_or(guid_buffer.len());
                String::from_utf16_lossy(&guid_buffer[..len])
            } else {
                format!("fixed-volume-{serial:08x}-{}", mount.display())
            };
            volumes.push(DiscoveredVolume {
                key: VolumeKey(key),
                mount,
                serial,
            });
        }
        start = end + 1;
    }
    volumes.sort_by(|left, right| left.mount.cmp(&right.mount));
    Ok(volumes)
}

#[cfg(not(windows))]
pub fn discover_fixed_volumes() -> Result<Vec<DiscoveredVolume>> {
    Err(AppError::Unsupported(
        "automatic fixed-volume discovery is Windows-only".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "personalrag-app-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
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
    fn metadata_build_resumes_from_durable_segments() {
        let base = temp_dir("resume");
        let root = base.join("root");
        let app_root = base.join("app");
        fs::create_dir_all(root.join("a")).unwrap();
        fs::create_dir_all(root.join("b")).unwrap();
        for index in 0..12 {
            fs::write(
                root.join(if index % 2 == 0 { "a" } else { "b" })
                    .join(format!("file-{index}.txt")),
                format!("value-{index}"),
            )
            .unwrap();
        }

        let paths = AppPaths::for_root(&app_root);
        paths.ensure().unwrap();
        let volume = fake_volume(&root, "resume");
        let calls = AtomicUsize::new(0);
        let mut stop = || calls.fetch_add(1, Ordering::SeqCst) >= 1;
        let first =
            build_or_resume_metadata(&paths, &volume, &[app_root.clone()], 2, &mut stop).unwrap();
        assert!(!first.complete);
        assert!(first.committed_segments > 0);

        let mut never_stop = || false;
        let second =
            build_or_resume_metadata(&paths, &volume, &[app_root], 2, &mut never_stop).unwrap();
        assert!(second.complete);
        assert!(second.metadata_records >= 14);

        let federated = FederatedMetadataIndex::load(&paths, &[volume]).unwrap();
        let hits = federated.search(Some("file-11"), None, false, 100);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].absolute_path.ends_with("file-11.txt"));
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn metadata_scan_hard_excludes_personalrag_store() {
        let base = temp_dir("exclude");
        let root = base.join("root");
        let app_root = root.join("user/AppData/Local/PersonalRag");
        fs::create_dir_all(&app_root).unwrap();
        fs::write(root.join("visible.txt"), "visible").unwrap();
        fs::write(app_root.join("must-not-index.txt"), "hidden").unwrap();

        let paths = AppPaths::for_root(&app_root);
        paths.ensure().unwrap();
        let volume = fake_volume(&root, "exclude");
        let mut never_stop = || false;
        let report =
            build_or_resume_metadata(&paths, &volume, &[app_root.clone()], 2, &mut never_stop)
                .unwrap();
        assert!(report.complete);

        let federated = FederatedMetadataIndex::load(&paths, &[volume]).unwrap();
        assert_eq!(
            federated.search(Some("must-not-index"), None, false, 100).len(),
            0
        );
        assert_eq!(federated.search(Some("visible"), None, false, 100).len(), 1);
        fs::remove_dir_all(base).unwrap();
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
        build_or_resume_metadata(&paths, &volume_a, &[app_root.clone()], 1, &mut never_stop)
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
