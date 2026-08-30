use super::state_io::{atomic_write_new, numbered_files, read_u32, read_u64};
use super::{AppError, AppPaths, DiscoveredVolume, Result};
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

const MAGIC: &[u8; 8] = b"PRV2CDQ1";
const VERSION: u32 = 1;
const HEADER_BYTES: usize = 40;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DirtyContentStatus {
    pub generation: u64,
    pub metadata_generation: u64,
    pub pending_files: usize,
}

#[derive(Clone, Debug, Default)]
pub(super) struct DirtyContentState {
    pub generation: u64,
    pub metadata_generation: u64,
    pub ids: BTreeSet<u64>,
}

fn dir(paths: &AppPaths, volume: &DiscoveredVolume) -> std::path::PathBuf {
    paths.volume_store(&volume.key).join("content-dirty")
}

pub fn dirty_content_status(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
) -> Result<DirtyContentStatus> {
    let metadata_generation = super::load_volume_manifest(&paths.volume_store(&volume.key))?
        .map(|value| value.metadata_generation)
        .unwrap_or(0);
    Ok(load_for_metadata(paths, volume, metadata_generation)?
        .map(|state| DirtyContentStatus {
            generation: state.generation,
            metadata_generation: state.metadata_generation,
            pending_files: state.ids.len(),
        })
        .unwrap_or_default())
}

pub(super) fn load_for_metadata(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
    metadata_generation: u64,
) -> Result<Option<DirtyContentState>> {
    if metadata_generation == 0 {
        return Ok(None);
    }
    let store = dir(paths, volume);
    let mut states = numbered_files(&store, "dirty-", ".prcdq")?;
    states.sort_unstable_by_key(|(generation, _)| std::cmp::Reverse(*generation));
    for (generation, path) in states {
        let Ok(bytes) = fs::read(path) else {
            continue;
        };
        let Ok(state) = deserialize(generation, &bytes) else {
            continue;
        };
        if state.metadata_generation == metadata_generation {
            return Ok(Some(state));
        }
    }
    Ok(None)
}

pub(super) fn replace(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
    metadata_generation: u64,
    ids: impl IntoIterator<Item = u64>,
) -> Result<DirtyContentState> {
    let store = dir(paths, volume);
    fs::create_dir_all(&store)?;
    let generation = numbered_files(&store, "dirty-", ".prcdq")?
        .into_iter()
        .map(|(generation, _)| generation)
        .max()
        .unwrap_or(0)
        .saturating_add(1)
        .max(1);
    let state = DirtyContentState {
        generation,
        metadata_generation,
        ids: ids.into_iter().filter(|id| *id != 0).collect(),
    };
    atomic_write_new(
        &store.join(format!("dirty-{generation:020}.prcdq")),
        &serialize(&state),
    )?;
    Ok(state)
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn merge(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
    metadata_generation: u64,
    add: impl IntoIterator<Item = u64>,
    remove: impl IntoIterator<Item = u64>,
) -> Result<DirtyContentState> {
    let mut ids = load_for_metadata(paths, volume, metadata_generation)?
        .map(|state| state.ids)
        .unwrap_or_default();
    ids.extend(add.into_iter().filter(|id| *id != 0));
    for id in remove {
        ids.remove(&id);
    }
    replace(paths, volume, metadata_generation, ids)
}

pub(super) fn gc(paths: &AppPaths, volume: &DiscoveredVolume, keep: usize) -> Result<usize> {
    let keep = keep.max(2);
    let store = dir(paths, volume);
    let mut states = numbered_files(&store, "dirty-", ".prcdq")?;
    states.sort_unstable_by_key(|(generation, _)| std::cmp::Reverse(*generation));
    let mut removed = 0;
    for (_, path) in states.into_iter().skip(keep) {
        fs::remove_file(path)?;
        removed += 1;
    }
    Ok(removed)
}

fn serialize(state: &DirtyContentState) -> Vec<u8> {
    let mut payload = Vec::with_capacity(state.ids.len() * 8);
    for id in &state.ids {
        payload.extend_from_slice(&id.to_le_bytes());
    }
    let mut bytes = vec![0_u8; HEADER_BYTES];
    bytes[0..8].copy_from_slice(MAGIC);
    bytes[8..12].copy_from_slice(&VERSION.to_le_bytes());
    bytes[16..24].copy_from_slice(&state.metadata_generation.to_le_bytes());
    bytes[24..32].copy_from_slice(&(state.ids.len() as u64).to_le_bytes());
    bytes[32..40].copy_from_slice(&crate::persistent::crc64_ecma(&payload).to_le_bytes());
    bytes.extend_from_slice(&payload);
    bytes
}

fn deserialize(generation: u64, bytes: &[u8]) -> Result<DirtyContentState> {
    if bytes.len() < HEADER_BYTES || &bytes[0..8] != MAGIC {
        return Err(AppError::InvalidState("dirty content queue magic".into()));
    }
    if read_u32(bytes, 8, "dirty content version")? != VERSION {
        return Err(AppError::InvalidState("dirty content queue version".into()));
    }
    let metadata_generation = read_u64(bytes, 16, "dirty content metadata generation")?;
    let count = usize::try_from(read_u64(bytes, 24, "dirty content count")?)
        .map_err(|_| AppError::InvalidState("dirty content count overflow".into()))?;
    let expected = HEADER_BYTES
        .checked_add(
            count
                .checked_mul(8)
                .ok_or_else(|| AppError::InvalidState("dirty content length overflow".into()))?,
        )
        .ok_or_else(|| AppError::InvalidState("dirty content length overflow".into()))?;
    if bytes.len() != expected {
        return Err(AppError::InvalidState("dirty content queue length".into()));
    }
    let payload = &bytes[HEADER_BYTES..];
    if crate::persistent::crc64_ecma(payload) != read_u64(bytes, 32, "dirty content checksum")? {
        return Err(AppError::InvalidState(
            "dirty content queue checksum".into(),
        ));
    }
    let mut ids = BTreeSet::new();
    for chunk in payload.chunks_exact(8) {
        let id = u64::from_le_bytes(chunk.try_into().expect("8 bytes"));
        if id == 0 || !ids.insert(id) {
            return Err(AppError::InvalidState(
                "dirty content queue duplicate/zero id".into(),
            ));
        }
    }
    Ok(DirtyContentState {
        generation,
        metadata_generation,
        ids,
    })
}

pub(super) fn remove_generation_files_except(
    content_dir: &Path,
    keep: &BTreeSet<u64>,
) -> Result<usize> {
    let mut removed = 0;
    if !content_dir.exists() {
        return Ok(0);
    }
    for entry in fs::read_dir(content_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let generation = if let Some(v) = name
            .strip_prefix("gen-")
            .and_then(|v| v.strip_suffix(".prv2"))
            .and_then(|v| v.parse::<u64>().ok())
        {
            Some(v)
        } else if let Some(v) = name
            .strip_prefix("verify-")
            .and_then(|v| v.strip_suffix(".prv2ver"))
            .and_then(|v| v.parse::<u64>().ok())
        {
            Some(v)
        } else {
            name.strip_prefix("content-map-")
                .and_then(|v| v.strip_suffix(".bin"))
                .and_then(|v| v.parse::<u64>().ok())
        };
        if generation.is_some_and(|g| !keep.contains(&g)) {
            fs::remove_file(entry.path())?;
            removed += 1;
        }
    }
    Ok(removed)
}

pub(super) fn reconcile_after_metadata_refresh(
    paths: &AppPaths,
    volume: &DiscoveredVolume,
    old_metadata: Option<&crate::metadata::MetadataIndex>,
    carried_ids: &BTreeSet<u64>,
    new_metadata: &crate::metadata::MetadataIndex,
    new_metadata_generation: u64,
) -> Result<DirtyContentState> {
    let old_by_id = old_metadata
        .map(|metadata| {
            metadata
                .records()
                .iter()
                .map(|record| (record.file_id, record))
                .collect::<std::collections::HashMap<_, _>>()
        })
        .unwrap_or_default();
    let new_by_id = new_metadata
        .records()
        .iter()
        .map(|record| (record.file_id, record))
        .collect::<std::collections::HashMap<_, _>>();
    let mut dirty = carried_ids
        .iter()
        .copied()
        .filter(|id| {
            new_by_id
                .get(id)
                .is_some_and(|record| record.content_searchable)
        })
        .collect::<BTreeSet<_>>();
    if old_metadata.is_some() {
        for record in new_metadata.records() {
            if !record.content_searchable {
                dirty.remove(&record.file_id);
                continue;
            }
            let changed = match old_by_id.get(&record.file_id).copied() {
                None => true,
                Some(previous) => {
                    previous.size != record.size
                        || previous.modified_ns != record.modified_ns
                        || previous.content_searchable != record.content_searchable
                        || previous.extractable != record.extractable
                }
            };
            if changed {
                dirty.insert(record.file_id);
            }
        }
    }
    replace(paths, volume, new_metadata_generation, dirty)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{DiscoveredVolume, VolumeKey};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn dirty_queue_falls_back_from_corrupt_latest_state() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base = std::env::temp_dir().join(format!("personalrag-dirty-{stamp}"));
        let root = base.join("root");
        fs::create_dir_all(&root).unwrap();
        let paths = AppPaths::for_root(base.join("app"));
        paths.ensure().unwrap();
        let volume = DiscoveredVolume {
            key: VolumeKey("dirty-test".into()),
            mount: root,
            serial: 1,
        };
        replace(&paths, &volume, 7, [10, 20]).unwrap();
        let latest = replace(&paths, &volume, 7, [10, 20, 30]).unwrap();
        fs::write(
            dir(&paths, &volume).join(format!("dirty-{:020}.prcdq", latest.generation)),
            b"broken",
        )
        .unwrap();
        let loaded = load_for_metadata(&paths, &volume, 7).unwrap().unwrap();
        assert_eq!(loaded.ids.into_iter().collect::<Vec<_>>(), vec![10, 20]);
        fs::remove_dir_all(base).unwrap();
    }
}
