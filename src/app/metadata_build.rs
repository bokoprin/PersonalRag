use super::state_io::{atomic_write_new, numbered_files, parse_key_values, parse_u64, read_u32};
use super::{
    AppError, AppPaths, DiscoveredVolume, Result, VolumeManifest, VolumePhase,
    load_volume_manifest, write_volume_manifest,
};
use crate::metadata::{MetadataFileKind, MetadataIndex, MetadataRecord};
use crate::product;
use std::collections::{HashSet, VecDeque};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub const METADATA_CHECKPOINT_RECORDS: usize = 4_096;
const METADATA_CHECKPOINT_DIRS: usize = 128;
const QUEUE_MAGIC: &[u8; 8] = b"PRV2MQ01";

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

pub fn begin_metadata_refresh(
    app_paths: &AppPaths,
    volume: &DiscoveredVolume,
) -> Result<VolumeManifest> {
    app_paths.ensure()?;
    let volume_store = app_paths.volume_store(&volume.key);
    let current =
        load_volume_manifest(&volume_store)?.unwrap_or_else(|| VolumeManifest::initial(volume));
    if current.phase == VolumePhase::MetadataBuilding {
        return Ok(current);
    }
    let manifest = VolumeManifest {
        generation: current.generation.saturating_add(1).max(1),
        key: volume.key.clone(),
        mount: volume.mount.clone(),
        phase: VolumePhase::MetadataBuilding,
        metadata_generation: current.metadata_generation,
        metadata_file: current.metadata_file.clone(),
        metadata_records: current.metadata_records,
        inaccessible_directories: current.inaccessible_directories,
    };
    write_volume_manifest(&volume_store, &manifest)?;
    Ok(manifest)
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
            let mut entries = read_dir
                .filter_map(std::result::Result::ok)
                .collect::<Vec<_>>();
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

    let mut metadata_generation = current_manifest
        .metadata_generation
        .saturating_add(1)
        .max(1);
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
    bytes.extend_from_slice(&(queue.len() as u32).to_le_bytes());
    for path in queue {
        let (encoding, payload) = encode_path(path);
        bytes.push(encoding);
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&payload);
    }
    atomic_write_new(path, &bytes)
}

fn read_path_queue(path: &Path) -> Result<VecDeque<PathBuf>> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    if bytes.len() < 12 || &bytes[..8] != QUEUE_MAGIC {
        return Err(AppError::InvalidState("invalid metadata queue".to_string()));
    }
    let count = read_u32(&bytes, 8, "metadata queue count")? as usize;
    let mut offset = 12_usize;
    let mut out = VecDeque::with_capacity(count);
    for _ in 0..count {
        let encoding = *bytes
            .get(offset)
            .ok_or_else(|| AppError::InvalidState("truncated queue encoding".to_string()))?;
        offset += 1;
        let len = read_u32(&bytes, offset, "metadata queue path length")? as usize;
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
        3 => Ok(PathBuf::from(std::str::from_utf8(payload).map_err(
            |_| AppError::InvalidState("invalid UTF-8 queue path".to_string()),
        )?)),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{FederatedMetadataIndex, VolumeKey};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "personalrag-metadata-build-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
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
        let first = build_or_resume_metadata(
            &paths,
            &volume,
            std::slice::from_ref(&app_root),
            2,
            &mut stop,
        )
        .unwrap();
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
        let report = build_or_resume_metadata(
            &paths,
            &volume,
            std::slice::from_ref(&app_root),
            2,
            &mut never_stop,
        )
        .unwrap();
        assert!(report.complete);

        let federated = FederatedMetadataIndex::load(&paths, &[volume]).unwrap();
        assert_eq!(
            federated
                .search(Some("must-not-index"), None, false, 100)
                .len(),
            0
        );
        assert_eq!(federated.search(Some("visible"), None, false, 100).len(), 1);
        fs::remove_dir_all(base).unwrap();
    }
}
