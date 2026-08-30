use super::{
    AppError, AppPaths, DiscoveredVolume, Result, VolumeManifest, VolumePhase, load_volume_manifest,
    numbered_files, parse_key_values, parse_u64, write_volume_manifest,
};
use crate::extraction::ExtractorConfig;
use crate::metadata::{MetadataIndex, MetadataRecord};
use crate::persistent::{
    PersistentIndex, PersistentSearchOverlay, load_generation_with_verification,
    publish_generation_from_paths_with_extraction,
};
use crate::{SearchLimits, incremental::ContentQueryKind};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

const CONTENT_SET_MAGIC_VERSION: u32 = 1;
const CONTENT_MAP_MAGIC: &[u8; 8] = b"PRV2CSM1";
const CONTENT_MAP_VERSION: u32 = 1;
const CONTENT_MAP_HEADER_BYTES: usize = 48;
const CONTENT_MAP_RECORD_BYTES: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentBuildOptions {
    pub max_files_per_shard: usize,
    pub max_source_bytes_per_shard: u64,
}

impl Default for ContentBuildOptions {
    fn default() -> Self {
        Self {
            max_files_per_shard: 512,
            max_source_bytes_per_shard: 64 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentBuildReport {
    pub complete: bool,
    pub content_set_generation: u64,
    pub indexed_cursor: usize,
    pub total_files: usize,
    pub skipped_files: usize,
    pub shard_count: usize,
}

#[derive(Clone, Debug)]
struct ContentSetState {
    generation: u64,
    metadata_generation: u64,
    cursor: usize,
    total_files: usize,
    skipped_files: usize,
    complete: bool,
    shards: Vec<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ContentShardEntry {
    stable_file_id: u64,
    source_size: u64,
    modified_ns: u128,
}

#[derive(Debug)]
struct LoadedShard {
    generation: u64,
    index: PersistentIndex,
    entries: Vec<ContentShardEntry>,
}

#[derive(Debug)]
struct LoadedContentVolume {
    volume: DiscoveredVolume,
    metadata: MetadataIndex,
    stable_to_record: HashMap<u64, usize>,
    shards: Vec<LoadedShard>,
}

#[derive(Clone, Debug)]
pub struct AppContentHit {
    pub volume: super::VolumeKey,
    pub absolute_path: PathBuf,
    pub record: MetadataRecord,
    pub line_number: u32,
    pub byte_offset_in_line: u32,
}

pub struct FederatedContentIndex {
    volumes: Vec<LoadedContentVolume>,
}

impl FederatedContentIndex {
    pub fn load(
        paths: &AppPaths,
        volumes: &[DiscoveredVolume],
        extractor: &ExtractorConfig,
    ) -> Result<Self> {
        let mut loaded_volumes = Vec::new();
        for volume in volumes {
            let store = paths.volume_store(&volume.key);
            let Some(manifest) = load_volume_manifest(&store)? else {
                continue;
            };
            let Some(metadata_file) = manifest.metadata_file.as_deref() else {
                continue;
            };
            let metadata =
                MetadataIndex::load_snapshot(store.join("metadata").join(metadata_file))?;
            let stable_to_record = metadata
                .records()
                .iter()
                .enumerate()
                .map(|(index, record)| (record.file_id, index))
                .collect::<HashMap<_, _>>();
            let Some(content_set) = load_content_set(&store)? else {
                continue;
            };
            let content_dir = store.join("content");
            let mut shards = Vec::new();
            for generation in &content_set.shards {
                let map = match load_shard_map(&content_dir, *generation) {
                    Ok(value) => value,
                    Err(_) => continue,
                };
                let index = match load_generation_with_verification(
                    &volume.mount,
                    &content_dir,
                    *generation,
                    extractor,
                ) {
                    Ok(value) => value,
                    Err(_) => continue,
                };
                if index.file_count() != map.len() {
                    continue;
                }
                shards.push(LoadedShard {
                    generation: *generation,
                    index,
                    entries: map,
                });
            }
            shards.sort_by_key(|shard| std::cmp::Reverse(shard.generation));
            loaded_volumes.push(LoadedContentVolume {
                volume: volume.clone(),
                metadata,
                stable_to_record,
                shards,
            });
        }
        Ok(Self {
            volumes: loaded_volumes,
        })
    }

    pub fn search(
        &self,
        query: ContentQueryKind<'_>,
        case_sensitive: bool,
        limits: SearchLimits,
    ) -> Result<Vec<AppContentHit>> {
        if limits.max_files == 0 || limits.max_matches_seen == 0 {
            return Ok(Vec::new());
        }

        let mut out = Vec::new();
        let mut seen_locations =
            HashSet::<(super::VolumeKey, u64, u32, u32)>::new();
        let mut seen_files = HashSet::<(super::VolumeKey, u64)>::new();
        let mut snippets_per_file = HashMap::<(super::VolumeKey, u64), usize>::new();

        for volume in &self.volumes {
            let newest_valid = newest_valid_shards(volume);
            for shard in &volume.shards {
                let mut excluded = HashSet::<u32>::new();
                let mut overrides = HashMap::<u32, PathBuf>::new();
                for (internal, entry) in shard.entries.iter().copied().enumerate() {
                    let Some(record_index) = volume.stable_to_record.get(&entry.stable_file_id)
                    else {
                        excluded.insert(internal as u32);
                        continue;
                    };
                    let record = &volume.metadata.records()[*record_index];
                    if record.size != entry.source_size
                        || record.modified_ns != entry.modified_ns
                        || newest_valid.get(&entry.stable_file_id).copied()
                            != Some(shard.generation)
                    {
                        excluded.insert(internal as u32);
                        continue;
                    }
                    if shard.index.file_relative_path(internal as u32) != Some(record.path.as_path())
                    {
                        overrides.insert(internal as u32, record.path.clone());
                    }
                }

                let overlay = PersistentSearchOverlay {
                    excluded_file_ids: &excluded,
                    path_overrides: &overrides,
                };
                let outcome = match query {
                    ContentQueryKind::Literal(value) => shard.index.search_with_limits_and_overlay(
                        value,
                        case_sensitive,
                        limits,
                        &overlay,
                    )?,
                    ContentQueryKind::Regex(value) => shard
                        .index
                        .search_regex_with_limits_and_overlay(
                            value,
                            case_sensitive,
                            limits,
                            &overlay,
                        )?,
                    ContentQueryKind::Wildcard(value) => shard
                        .index
                        .search_wildcard_with_limits_and_overlay(
                            value,
                            case_sensitive,
                            limits,
                            &overlay,
                        )?,
                };

                for hit in outcome.hits {
                    let Some(entry) = shard.entries.get(hit.file_id as usize).copied() else {
                        continue;
                    };
                    let Some(record_index) =
                        volume.stable_to_record.get(&entry.stable_file_id).copied()
                    else {
                        continue;
                    };
                    let file_key = (volume.volume.key.clone(), entry.stable_file_id);
                    let is_new_file = !seen_files.contains(&file_key);
                    if is_new_file && seen_files.len() >= limits.max_files {
                        continue;
                    }
                    let snippets = snippets_per_file.entry(file_key.clone()).or_insert(0);
                    if *snippets >= limits.max_snippets_per_file {
                        continue;
                    }
                    let location_key = (
                        volume.volume.key.clone(),
                        entry.stable_file_id,
                        hit.line_number,
                        hit.byte_offset_in_line,
                    );
                    if !seen_locations.insert(location_key) {
                        continue;
                    }
                    if out.len() >= limits.max_matches_seen {
                        return Ok(out);
                    }
                    seen_files.insert(file_key);
                    *snippets += 1;
                    let record = volume.metadata.records()[record_index].clone();
                    out.push(AppContentHit {
                        volume: volume.volume.key.clone(),
                        absolute_path: volume.volume.mount.join(&record.path),
                        record,
                        line_number: hit.line_number,
                        byte_offset_in_line: hit.byte_offset_in_line,
                    });
                }
            }
        }
        Ok(out)
    }
}

fn newest_valid_shards(volume: &LoadedContentVolume) -> HashMap<u64, u64> {
    let mut newest = HashMap::new();
    for shard in &volume.shards {
        for entry in &shard.entries {
            if newest.contains_key(&entry.stable_file_id) {
                continue;
            }
            let Some(record_index) = volume.stable_to_record.get(&entry.stable_file_id) else {
                continue;
            };
            let record = &volume.metadata.records()[*record_index];
            if record.size == entry.source_size && record.modified_ns == entry.modified_ns {
                newest.insert(entry.stable_file_id, shard.generation);
            }
        }
    }
    newest
}

pub fn content_progress(
    app_paths: &AppPaths,
    volume: &DiscoveredVolume,
) -> Result<Option<ContentBuildReport>> {
    let volume_store = app_paths.volume_store(&volume.key);
    Ok(load_content_set(&volume_store)?.map(|state| report_from_state(&state)))
}

pub fn build_or_resume_content<F>(
    app_paths: &AppPaths,
    volume: &DiscoveredVolume,
    extractor: &ExtractorConfig,
    options: ContentBuildOptions,
    should_stop: &mut F,
) -> Result<ContentBuildReport>
where
    F: FnMut() -> bool,
{
    loop {
        let report = build_content_step(app_paths, volume, extractor, options)?;
        if report.complete || should_stop() {
            return Ok(report);
        }
    }
}

pub fn build_content_step(
    app_paths: &AppPaths,
    volume: &DiscoveredVolume,
    extractor: &ExtractorConfig,
    options: ContentBuildOptions,
) -> Result<ContentBuildReport> {
    app_paths.ensure()?;
    let volume_store = app_paths.volume_store(&volume.key);
    let metadata_manifest = load_volume_manifest(&volume_store)?
        .ok_or_else(|| AppError::InvalidState("content build requires metadata manifest".into()))?;
    let metadata_file = metadata_manifest
        .metadata_file
        .as_deref()
        .ok_or_else(|| AppError::InvalidState("content build requires metadata snapshot".into()))?;
    let metadata =
        MetadataIndex::load_snapshot(volume_store.join("metadata").join(metadata_file))?;
    let searchable = metadata
        .records()
        .iter()
        .filter(|record| record.content_searchable)
        .collect::<Vec<_>>();
    let total_files = searchable.len();

    let latest = load_content_set(&volume_store)?;
    if let Some(state) = latest.as_ref()
        && state.metadata_generation == metadata_manifest.metadata_generation
        && state.complete
        && state.cursor >= total_files
    {
        ensure_volume_phase(&volume_store, &metadata_manifest, VolumePhase::Ready)?;
        return Ok(report_from_state(state));
    }

    let mut state = if let Some(previous) = latest {
        if previous.metadata_generation == metadata_manifest.metadata_generation {
            previous
        } else {
            ContentSetState {
                generation: next_content_set_generation(&volume_store)?,
                metadata_generation: metadata_manifest.metadata_generation,
                cursor: 0,
                total_files,
                skipped_files: previous.skipped_files,
                complete: total_files == 0,
                shards: previous.shards,
            }
        }
    } else {
        ContentSetState {
            generation: next_content_set_generation(&volume_store)?,
            metadata_generation: metadata_manifest.metadata_generation,
            cursor: 0,
            total_files,
            skipped_files: 0,
            complete: total_files == 0,
            shards: Vec::new(),
        }
    };

    if state.total_files != total_files {
        state.total_files = total_files;
        state.cursor = state.cursor.min(total_files);
    }

    if state.complete || state.cursor >= total_files {
        state.complete = true;
        state.cursor = total_files;
        write_content_set(&volume_store, &state)?;
        ensure_volume_phase(&volume_store, &metadata_manifest, VolumePhase::Ready)?;
        return Ok(report_from_state(&state));
    }

    ensure_volume_phase(
        &volume_store,
        &metadata_manifest,
        VolumePhase::ContentBuilding,
    )?;

    let max_files = options.max_files_per_shard.max(1);
    let max_bytes = options.max_source_bytes_per_shard.max(1);
    let start = state.cursor;
    let mut end = start;
    let mut selected_bytes = 0_u64;
    while end < total_files && end.saturating_sub(start) < max_files {
        let size = searchable[end].size;
        if end > start && selected_bytes.saturating_add(size) > max_bytes {
            break;
        }
        selected_bytes = selected_bytes.saturating_add(size);
        end += 1;
    }
    if end == start {
        end = (start + 1).min(total_files);
    }

    let attempted = &searchable[start..end];
    let relative_paths = attempted
        .iter()
        .map(|record| record.path.clone())
        .collect::<Vec<_>>();
    let content_dir = volume_store.join("content");
    fs::create_dir_all(&content_dir)?;
    let generation = next_content_generation(&content_dir)?;
    let parent_generation = state.shards.last().copied().unwrap_or(0);
    let (published, skipped) = publish_generation_from_paths_with_extraction(
        &volume.mount,
        &content_dir,
        generation,
        parent_generation,
        &relative_paths,
        extractor,
    )?;
    let index = load_generation_with_verification(
        &volume.mount,
        &content_dir,
        published.generation,
        extractor,
    )?;
    let path_to_record = attempted
        .iter()
        .map(|record| (record.path.clone(), *record))
        .collect::<HashMap<_, _>>();
    let mut map = Vec::with_capacity(index.file_count());
    for internal in 0..index.file_count() as u32 {
        let relative = index.file_relative_path(internal).ok_or_else(|| {
            AppError::InvalidState("content shard file catalog is incomplete".into())
        })?;
        let record = path_to_record.get(relative).ok_or_else(|| {
            AppError::InvalidState(format!(
                "content shard path missing from metadata batch: {}",
                relative.display()
            ))
        })?;
        map.push(ContentShardEntry {
            stable_file_id: record.file_id,
            source_size: record.size,
            modified_ns: record.modified_ns,
        });
    }
    write_shard_map(
        &content_dir,
        published.generation,
        metadata_manifest.metadata_generation,
        &map,
    )?;

    state.generation = next_content_set_generation(&volume_store)?;
    state.cursor = end;
    state.skipped_files = state.skipped_files.saturating_add(skipped.len());
    state.shards.push(published.generation);
    state.complete = state.cursor >= total_files;
    write_content_set(&volume_store, &state)?;
    if state.complete {
        ensure_volume_phase(&volume_store, &metadata_manifest, VolumePhase::Ready)?;
    }
    Ok(report_from_state(&state))
}

fn ensure_volume_phase(
    volume_store: &Path,
    metadata_manifest: &VolumeManifest,
    phase: VolumePhase,
) -> Result<()> {
    let current = load_volume_manifest(volume_store)?.unwrap_or_else(|| metadata_manifest.clone());
    if current.phase == phase {
        return Ok(());
    }
    let next = VolumeManifest {
        generation: current.generation.saturating_add(1).max(1),
        key: current.key.clone(),
        mount: current.mount.clone(),
        phase,
        metadata_generation: current.metadata_generation,
        metadata_file: current.metadata_file.clone(),
        metadata_records: current.metadata_records,
        inaccessible_directories: current.inaccessible_directories,
    };
    write_volume_manifest(volume_store, &next)?;
    Ok(())
}

fn report_from_state(state: &ContentSetState) -> ContentBuildReport {
    ContentBuildReport {
        complete: state.complete,
        content_set_generation: state.generation,
        indexed_cursor: state.cursor,
        total_files: state.total_files,
        skipped_files: state.skipped_files,
        shard_count: state.shards.len(),
    }
}

fn next_content_generation(content_dir: &Path) -> Result<u64> {
    let mut max = 0_u64;
    if content_dir.exists() {
        for entry in fs::read_dir(content_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(value) = name
                .strip_prefix("gen-")
                .and_then(|value| value.strip_suffix(".prv2"))
                .and_then(|value| value.parse::<u64>().ok())
            {
                max = max.max(value);
            }
        }
    }
    Ok(max.saturating_add(1).max(1))
}

fn next_content_set_generation(volume_store: &Path) -> Result<u64> {
    Ok(numbered_files(volume_store, "content-set-", ".state")?
        .into_iter()
        .map(|(generation, _)| generation)
        .max()
        .unwrap_or(0)
        .saturating_add(1)
        .max(1))
}

fn write_content_set(volume_store: &Path, state: &ContentSetState) -> Result<()> {
    let path = volume_store.join(format!("content-set-{:020}.state", state.generation));
    let shards = state
        .shards
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let content = format!(
        "version={}\nmetadata_generation={}\ncursor={}\ntotal_files={}\nskipped_files={}\ncomplete={}\nshards={}\n",
        CONTENT_SET_MAGIC_VERSION,
        state.metadata_generation,
        state.cursor,
        state.total_files,
        state.skipped_files,
        u8::from(state.complete),
        shards
    );
    atomic_write_new(&path, content.as_bytes())
}

fn load_content_set(volume_store: &Path) -> Result<Option<ContentSetState>> {
    let mut states = numbered_files(volume_store, "content-set-", ".state")?;
    states.sort_unstable_by_key(|(generation, _)| std::cmp::Reverse(*generation));
    for (generation, path) in states {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        let values = parse_key_values(&text);
        if parse_u64(&values, "version") != Some(CONTENT_SET_MAGIC_VERSION as u64) {
            continue;
        }
        let Some(metadata_generation) = parse_u64(&values, "metadata_generation") else {
            continue;
        };
        let Some(cursor) = parse_u64(&values, "cursor") else {
            continue;
        };
        let Some(total_files) = parse_u64(&values, "total_files") else {
            continue;
        };
        let Some(skipped_files) = parse_u64(&values, "skipped_files") else {
            continue;
        };
        let complete = matches!(values.get("complete").map(String::as_str), Some("1"));
        let shards = values
            .get("shards")
            .map(|value| {
                value
                    .split(',')
                    .filter(|value| !value.is_empty())
                    .filter_map(|value| value.parse::<u64>().ok())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        return Ok(Some(ContentSetState {
            generation,
            metadata_generation,
            cursor: cursor as usize,
            total_files: total_files as usize,
            skipped_files: skipped_files as usize,
            complete,
            shards,
        }));
    }
    Ok(None)
}

fn write_shard_map(
    content_dir: &Path,
    generation: u64,
    metadata_generation: u64,
    entries: &[ContentShardEntry],
) -> Result<()> {
    let mut payload = Vec::with_capacity(entries.len() * CONTENT_MAP_RECORD_BYTES);
    for entry in entries {
        payload.extend_from_slice(&entry.stable_file_id.to_le_bytes());
        payload.extend_from_slice(&entry.source_size.to_le_bytes());
        payload.extend_from_slice(&(entry.modified_ns as u64).to_le_bytes());
        payload.extend_from_slice(&((entry.modified_ns >> 64) as u64).to_le_bytes());
    }
    let mut bytes = vec![0_u8; CONTENT_MAP_HEADER_BYTES];
    bytes[0..8].copy_from_slice(CONTENT_MAP_MAGIC);
    bytes[8..12].copy_from_slice(&CONTENT_MAP_VERSION.to_le_bytes());
    bytes[16..24].copy_from_slice(&generation.to_le_bytes());
    bytes[24..32].copy_from_slice(&metadata_generation.to_le_bytes());
    bytes[32..40].copy_from_slice(&(entries.len() as u64).to_le_bytes());
    bytes[40..48].copy_from_slice(&crate::persistent::crc64_ecma(&payload).to_le_bytes());
    bytes.extend_from_slice(&payload);
    atomic_write_new(
        &content_dir.join(format!("content-map-{generation:020}.bin")),
        &bytes,
    )
}

fn load_shard_map(content_dir: &Path, generation: u64) -> Result<Vec<ContentShardEntry>> {
    let bytes = fs::read(content_dir.join(format!("content-map-{generation:020}.bin")))?;
    if bytes.len() < CONTENT_MAP_HEADER_BYTES || &bytes[0..8] != CONTENT_MAP_MAGIC {
        return Err(AppError::InvalidState("content shard map magic".into()));
    }
    if read_u32(&bytes, 8)? != CONTENT_MAP_VERSION {
        return Err(AppError::InvalidState("content shard map version".into()));
    }
    if read_u64(&bytes, 16)? != generation {
        return Err(AppError::InvalidState(
            "content shard map generation mismatch".into(),
        ));
    }
    let count = read_u64(&bytes, 32)? as usize;
    let expected = CONTENT_MAP_HEADER_BYTES
        .checked_add(
            count
                .checked_mul(CONTENT_MAP_RECORD_BYTES)
                .ok_or_else(|| AppError::InvalidState("content map length overflow".into()))?,
        )
        .ok_or_else(|| AppError::InvalidState("content map length overflow".into()))?;
    if bytes.len() != expected {
        return Err(AppError::InvalidState("content shard map length".into()));
    }
    let payload = &bytes[CONTENT_MAP_HEADER_BYTES..];
    if crate::persistent::crc64_ecma(payload) != read_u64(&bytes, 40)? {
        return Err(AppError::InvalidState("content shard map checksum".into()));
    }
    let mut entries = Vec::with_capacity(count);
    for chunk in payload.chunks_exact(CONTENT_MAP_RECORD_BYTES) {
        let stable_file_id = u64::from_le_bytes(chunk[0..8].try_into().expect("8 bytes"));
        let source_size = u64::from_le_bytes(chunk[8..16].try_into().expect("8 bytes"));
        let modified_low = u64::from_le_bytes(chunk[16..24].try_into().expect("8 bytes"));
        let modified_high = u64::from_le_bytes(chunk[24..32].try_into().expect("8 bytes"));
        entries.push(ContentShardEntry {
            stable_file_id,
            source_size,
            modified_ns: u128::from(modified_low) | (u128::from(modified_high) << 64),
        });
    }
    Ok(entries)
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

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let value = bytes
        .get(offset..offset.saturating_add(4))
        .ok_or_else(|| AppError::InvalidState("truncated content u32".into()))?;
    Ok(u32::from_le_bytes(
        value
            .try_into()
            .map_err(|_| AppError::InvalidState("invalid content u32".into()))?,
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let value = bytes
        .get(offset..offset.saturating_add(8))
        .ok_or_else(|| AppError::InvalidState("truncated content u64".into()))?;
    Ok(u64::from_le_bytes(
        value
            .try_into()
            .map_err(|_| AppError::InvalidState("invalid content u64".into()))?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{
        AppPaths, DiscoveredVolume, VolumeKey, begin_metadata_refresh, build_or_resume_metadata,
    };
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "personalrag-content-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn volume(root: &Path) -> DiscoveredVolume {
        DiscoveredVolume {
            key: VolumeKey("content-test".to_string()),
            mount: root.to_path_buf(),
            serial: 1,
        }
    }

    fn build_metadata(paths: &AppPaths, volume: &DiscoveredVolume) {
        let mut never_stop = || false;
        build_or_resume_metadata(
            paths,
            volume,
            std::slice::from_ref(&paths.root),
            2,
            &mut never_stop,
        )
        .unwrap();
    }

    #[test]
    fn content_shards_are_searchable_before_full_build_and_resume() {
        let base = temp_dir("resume");
        let root = base.join("root");
        let app_root = base.join("app");
        fs::create_dir_all(&root).unwrap();
        for index in 0..5 {
            fs::write(
                root.join(format!("file-{index}.txt")),
                format!("needle-{index}\n"),
            )
            .unwrap();
        }
        let paths = AppPaths::for_root(&app_root);
        paths.ensure().unwrap();
        let volume = volume(&root);
        build_metadata(&paths, &volume);
        let extractor = ExtractorConfig::discover();
        let options = ContentBuildOptions {
            max_files_per_shard: 2,
            max_source_bytes_per_shard: u64::MAX,
        };

        let first = build_content_step(&paths, &volume, &extractor, options).unwrap();
        assert!(!first.complete);
        assert_eq!(first.indexed_cursor, 2);
        let partial =
            FederatedContentIndex::load(&paths, std::slice::from_ref(&volume), &extractor).unwrap();
        assert_eq!(
            partial
                .search(
                    ContentQueryKind::Literal("needle-0"),
                    false,
                    SearchLimits::default()
                )
                .unwrap()
                .len(),
            1
        );

        let mut never_stop = || false;
        let final_report =
            build_or_resume_content(&paths, &volume, &extractor, options, &mut never_stop).unwrap();
        assert!(final_report.complete);
        assert_eq!(final_report.indexed_cursor, 5);
        let complete =
            FederatedContentIndex::load(&paths, std::slice::from_ref(&volume), &extractor).unwrap();
        assert_eq!(
            complete
                .search(
                    ContentQueryKind::Literal("needle-4"),
                    false,
                    SearchLimits::default()
                )
                .unwrap()
                .len(),
            1
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn stale_shard_content_is_excluded_after_metadata_refresh() {
        let base = temp_dir("stale");
        let root = base.join("root");
        let app_root = base.join("app");
        fs::create_dir_all(&root).unwrap();
        let file = root.join("value.txt");
        fs::write(&file, "old-needle").unwrap();
        let paths = AppPaths::for_root(&app_root);
        paths.ensure().unwrap();
        let volume = volume(&root);
        build_metadata(&paths, &volume);
        let extractor = ExtractorConfig::discover();
        let mut never_stop = || false;
        build_or_resume_content(
            &paths,
            &volume,
            &extractor,
            ContentBuildOptions::default(),
            &mut never_stop,
        )
        .unwrap();

        fs::write(&file, "new-content-with-different-size").unwrap();
        begin_metadata_refresh(&paths, &volume).unwrap();
        build_or_resume_metadata(
            &paths,
            &volume,
            std::slice::from_ref(&paths.root),
            2,
            &mut never_stop,
        )
        .unwrap();

        let index =
            FederatedContentIndex::load(&paths, std::slice::from_ref(&volume), &extractor).unwrap();
        assert!(
            index
                .search(
                    ContentQueryKind::Literal("old-needle"),
                    false,
                    SearchLimits::default()
                )
                .unwrap()
                .is_empty()
        );
        fs::remove_dir_all(base).unwrap();
    }
}
