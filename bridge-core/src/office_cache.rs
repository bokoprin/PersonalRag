use std::{
    collections::{BTreeMap, HashSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Mutex,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::extractor::{
    ExtractionBudget, ExtractorRegistry, OfficeKind, PreparedContent, INGESTION_VERSION,
};

const CACHE_MAGIC: &[u8; 8] = b"PROFC001";
const LIVE_MAGIC: &str = "PROFLIVE1";
const DEFAULT_SOFT_LIMIT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const DEFAULT_TARGET_BYTES: u64 = 1_600 * 1024 * 1024;
const DEFAULT_GRACE_SECS: u64 = 7 * 24 * 60 * 60;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Eq)]
struct EntryFingerprint {
    name: String,
    method: u16,
    flags: u16,
    crc32: u32,
    compressed_size: u64,
    uncompressed_size: u64,
    compressed_hash_a: u64,
    compressed_hash_b: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfficeFingerprint {
    kind: OfficeKind,
    entries: Vec<EntryFingerprint>,
}

impl OfficeFingerprint {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64 + self.entries.len() * 64);
        out.extend_from_slice(CACHE_MAGIC);
        out.extend_from_slice(&INGESTION_VERSION.to_le_bytes());
        out.push(match self.kind {
            OfficeKind::Docx => 1,
            OfficeKind::Xlsx => 2,
            OfficeKind::Pptx => 3,
        });
        out.extend_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for entry in &self.entries {
            let name = entry.name.as_bytes();
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(name);
            out.extend_from_slice(&entry.method.to_le_bytes());
            out.extend_from_slice(&entry.flags.to_le_bytes());
            out.extend_from_slice(&entry.crc32.to_le_bytes());
            out.extend_from_slice(&entry.compressed_size.to_le_bytes());
            out.extend_from_slice(&entry.uncompressed_size.to_le_bytes());
            out.extend_from_slice(&entry.compressed_hash_a.to_le_bytes());
            out.extend_from_slice(&entry.compressed_hash_b.to_le_bytes());
        }
        out
    }

    fn key(&self) -> String {
        let bytes = self.encode();
        let (a, b) = hash128(&bytes);
        format!("{a:016x}{b:016x}")
    }
}

#[derive(Clone, Debug)]
struct CentralEntry {
    name: String,
    method: u16,
    flags: u16,
    crc32: u32,
    compressed_size: u64,
    uncompressed_size: u64,
    local_offset: u64,
}

#[derive(Clone, Debug)]
pub struct OfficeExtractionConfig {
    pub max_workers: usize,
    pub memory_budget_bytes: u64,
    pub cache_soft_limit_bytes: u64,
    pub cache_target_bytes: u64,
    pub cache_grace: Duration,
}

impl Default for OfficeExtractionConfig {
    fn default() -> Self {
        let cpus = thread::available_parallelism().map_or(1, usize::from);
        Self {
            max_workers: cpus.clamp(1, 4),
            memory_budget_bytes: 512 * 1024 * 1024,
            cache_soft_limit_bytes: DEFAULT_SOFT_LIMIT_BYTES,
            cache_target_bytes: DEFAULT_TARGET_BYTES,
            cache_grace: Duration::from_secs(DEFAULT_GRACE_SECS),
        }
    }
}

#[derive(Clone, Debug)]
pub struct OfficeExtractionRequest {
    pub source_index: usize,
    pub path: PathBuf,
    pub source_bytes: u64,
}

#[derive(Debug)]
pub enum OfficePreparedContent {
    Cached {
        source_index: usize,
        path: PathBuf,
        cache_key: String,
        cache_hit: bool,
        extracted_bytes: u64,
    },
    Extracted {
        source_index: usize,
        text: String,
        cache_key: Option<String>,
    },
    Failed {
        source_index: usize,
        error: String,
    },
}

impl OfficePreparedContent {
    #[must_use]
    pub fn source_index(&self) -> usize {
        match self {
            Self::Cached { source_index, .. }
            | Self::Extracted { source_index, .. }
            | Self::Failed { source_index, .. } => *source_index,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct OfficeExtractionBatchReport {
    pub requests: usize,
    pub workers: usize,
    pub cache_hits: usize,
    pub cache_misses: usize,
    pub cache_write_fallbacks: usize,
    pub failures: usize,
}

#[derive(Clone, Debug, Default)]
pub struct OfficeCacheGcReport {
    pub before_bytes: u64,
    pub after_bytes: u64,
    pub deleted_objects: usize,
    pub reclaimed_bytes: u64,
}

#[derive(Debug)]
pub struct OfficeExtractionService {
    root: PathBuf,
    budget: ExtractionBudget,
    config: OfficeExtractionConfig,
    cache_enabled: bool,
}

impl OfficeExtractionService {
    #[must_use]
    pub fn cache_root_for_index_path(index_or_build_dir: &Path) -> PathBuf {
        let name = index_or_build_dir
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        if name == "portable-index" || name.starts_with("portable-index-build-") {
            return index_or_build_dir
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("office-extraction-cache");
        }
        index_or_build_dir.with_extension("office-cache")
    }

    #[must_use]
    pub fn new(
        root: PathBuf,
        budget: ExtractionBudget,
        mut config: OfficeExtractionConfig,
    ) -> Self {
        config.max_workers = config.max_workers.max(1);
        config.memory_budget_bytes = config.memory_budget_bytes.max(1);
        config.cache_target_bytes = config.cache_target_bytes.min(config.cache_soft_limit_bytes);
        let cache_enabled = ensure_cache_layout(&root).is_ok();
        Self {
            root,
            budget,
            config,
            cache_enabled,
        }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn prepare_many(
        &self,
        requests: &[OfficeExtractionRequest],
        cancel: &AtomicBool,
    ) -> (Vec<OfficePreparedContent>, OfficeExtractionBatchReport) {
        if requests.is_empty() {
            return (Vec::new(), OfficeExtractionBatchReport::default());
        }
        let workers = bounded_worker_count(
            requests,
            self.config.max_workers,
            self.config.memory_budget_bytes,
        );
        let next = AtomicUsize::new(0);
        let results = Mutex::new(Vec::<OfficePreparedContent>::with_capacity(requests.len()));
        let hits = AtomicUsize::new(0);
        let misses = AtomicUsize::new(0);
        let fallbacks = AtomicUsize::new(0);
        let failures = AtomicUsize::new(0);
        let registry = ExtractorRegistry::new();

        thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| loop {
                    if cancel.load(Ordering::Acquire) {
                        break;
                    }
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(request) = requests.get(index) else {
                        break;
                    };
                    let prepared = self.prepare_one(request, &registry);
                    match &prepared {
                        OfficePreparedContent::Cached { cache_hit, .. } => {
                            if *cache_hit {
                                hits.fetch_add(1, Ordering::Relaxed);
                            } else {
                                misses.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        OfficePreparedContent::Extracted { .. } => {
                            misses.fetch_add(1, Ordering::Relaxed);
                            fallbacks.fetch_add(1, Ordering::Relaxed);
                        }
                        OfficePreparedContent::Failed { .. } => {
                            failures.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    results.lock().expect("office result lock").push(prepared);
                });
            }
        });

        let mut results = results.into_inner().expect("office result lock");
        results.sort_by_key(OfficePreparedContent::source_index);
        let report = OfficeExtractionBatchReport {
            requests: requests.len(),
            workers,
            cache_hits: hits.load(Ordering::Relaxed),
            cache_misses: misses.load(Ordering::Relaxed),
            cache_write_fallbacks: fallbacks.load(Ordering::Relaxed),
            failures: failures.load(Ordering::Relaxed),
        };
        (results, report)
    }

    pub fn read_search_text(&self, path: &Path) -> Result<(String, Option<String>, bool), String> {
        let request = OfficeExtractionRequest {
            source_index: 0,
            path: path.to_path_buf(),
            source_bytes: fs::metadata(path).map_err(|error| error.to_string())?.len(),
        };
        let registry = ExtractorRegistry::new();
        match self.prepare_one(&request, &registry) {
            OfficePreparedContent::Cached {
                path,
                cache_key,
                cache_hit,
                ..
            } => fs::read_to_string(path)
                .map(|text| (text, Some(cache_key), cache_hit))
                .map_err(|error| error.to_string()),
            OfficePreparedContent::Extracted { text, .. } => Ok((text, None, false)),
            OfficePreparedContent::Failed { error, .. } => Err(error),
        }
    }

    fn prepare_one(
        &self,
        request: &OfficeExtractionRequest,
        registry: &ExtractorRegistry,
    ) -> OfficePreparedContent {
        let fingerprint = match office_fingerprint(&request.path, self.budget) {
            Ok(value) => value,
            Err(error) => {
                return OfficePreparedContent::Failed {
                    source_index: request.source_index,
                    error,
                };
            }
        };
        let key = fingerprint.key();
        let cached_path = if self.cache_enabled {
            self.lookup(&key, &fingerprint).ok().flatten()
        } else {
            None
        };
        if let Some(path) = cached_path {
            let extracted_bytes = fs::metadata(&path).map_or(0, |metadata| metadata.len());
            return OfficePreparedContent::Cached {
                source_index: request.source_index,
                path,
                cache_key: key,
                cache_hit: true,
                extracted_bytes,
            };
        }

        let text = match registry.prepare(&request.path, self.budget) {
            Ok(PreparedContent::Extracted(document)) => document.text,
            Ok(_) => {
                return OfficePreparedContent::Failed {
                    source_index: request.source_index,
                    error: "Office extraction did not produce prepared text".to_owned(),
                };
            }
            Err(error) => {
                return OfficePreparedContent::Failed {
                    source_index: request.source_index,
                    error,
                };
            }
        };

        let published_path = if self.cache_enabled {
            self.publish_object(&key, &fingerprint, text.as_bytes())
                .ok()
        } else {
            None
        };
        if let Some(path) = published_path {
            return OfficePreparedContent::Cached {
                source_index: request.source_index,
                path,
                cache_key: key,
                cache_hit: false,
                extracted_bytes: text.len() as u64,
            };
        }
        OfficePreparedContent::Extracted {
            source_index: request.source_index,
            text,
            cache_key: Some(key),
        }
    }

    fn object_paths(&self, key: &str) -> (PathBuf, PathBuf) {
        let shard = key.get(..2).unwrap_or("00");
        let dir = self.root.join("objects").join(shard);
        (
            dir.join(format!("{key}.txt")),
            dir.join(format!("{key}.meta")),
        )
    }

    fn lookup(
        &self,
        key: &str,
        fingerprint: &OfficeFingerprint,
    ) -> Result<Option<PathBuf>, String> {
        let (text_path, meta_path) = self.object_paths(key);
        let meta = match fs::read(&meta_path) {
            Ok(value) => value,
            Err(_) => return Ok(None),
        };
        let fingerprint_bytes = fingerprint.encode();
        if meta.len() != fingerprint_bytes.len() + 24
            || meta.get(..fingerprint_bytes.len()) != Some(fingerprint_bytes.as_slice())
        {
            return Ok(None);
        }
        let tail = &meta[fingerprint_bytes.len()..];
        let expected_len = u64::from_le_bytes(tail[0..8].try_into().expect("length slice"));
        let expected_a = u64::from_le_bytes(tail[8..16].try_into().expect("hash slice"));
        let expected_b = u64::from_le_bytes(tail[16..24].try_into().expect("hash slice"));
        let metadata = match fs::metadata(&text_path) {
            Ok(value) => value,
            Err(_) => return Ok(None),
        };
        if metadata.len() != expected_len {
            return Ok(None);
        }
        let (actual_a, actual_b) = hash_file(&text_path)?;
        if actual_a != expected_a || actual_b != expected_b {
            return Ok(None);
        }
        Ok(Some(text_path))
    }

    fn publish_object(
        &self,
        key: &str,
        fingerprint: &OfficeFingerprint,
        text: &[u8],
    ) -> Result<PathBuf, String> {
        let (text_path, meta_path) = self.object_paths(key);
        if let Some(parent) = text_path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        // A key may already exist but have failed checksum validation. Cache is disposable, so
        // remove that stale pair before publishing a repaired object. A concurrent valid writer
        // is re-checked below before we replace anything.
        if let Ok(Some(path)) = self.lookup(key, fingerprint) {
            return Ok(path);
        }
        let _ = fs::remove_file(&text_path);
        let _ = fs::remove_file(&meta_path);
        fs::create_dir_all(self.root.join("tmp")).map_err(|error| error.to_string())?;
        let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let temp_text = self
            .root
            .join("tmp")
            .join(format!("{key}.{pid}.{unique}.txt.tmp"));
        let temp_meta = self
            .root
            .join("tmp")
            .join(format!("{key}.{pid}.{unique}.meta.tmp"));
        let (hash_a, hash_b) = hash128(text);
        let mut meta = fingerprint.encode();
        meta.extend_from_slice(&(text.len() as u64).to_le_bytes());
        meta.extend_from_slice(&hash_a.to_le_bytes());
        meta.extend_from_slice(&hash_b.to_le_bytes());

        write_complete(&temp_text, text)?;
        write_complete(&temp_meta, &meta)?;
        match fs::rename(&temp_text, &text_path) {
            Ok(()) => {}
            Err(_) if text_path.exists() => {
                let _ = fs::remove_file(&temp_text);
            }
            Err(error) => {
                let _ = fs::remove_file(&temp_text);
                let _ = fs::remove_file(&temp_meta);
                return Err(error.to_string());
            }
        }
        match fs::rename(&temp_meta, &meta_path) {
            Ok(()) => {}
            Err(_) if meta_path.exists() => {
                let _ = fs::remove_file(&temp_meta);
            }
            Err(error) => {
                let _ = fs::remove_file(&temp_meta);
                return Err(error.to_string());
            }
        }
        match self.lookup(key, fingerprint)? {
            Some(path) => Ok(path),
            None => Err("published Office cache object failed validation".to_owned()),
        }
    }

    pub fn load_live(&self) -> BTreeMap<String, String> {
        let path = self.root.join("LIVE");
        let Ok(text) = fs::read_to_string(path) else {
            return BTreeMap::new();
        };
        let mut lines = text.lines();
        if lines.next() != Some(LIVE_MAGIC) {
            return BTreeMap::new();
        }
        let mut out = BTreeMap::new();
        for line in lines {
            let Some((path_hex, key)) = line.split_once('\t') else {
                continue;
            };
            let Ok(path_bytes) = decode_hex(path_hex) else {
                continue;
            };
            let Ok(path) = String::from_utf8(path_bytes) else {
                continue;
            };
            if key.len() == 32 && key.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                out.insert(path, key.to_ascii_lowercase());
            }
        }
        out
    }

    pub fn publish_live(&self, live: &BTreeMap<String, String>) -> Result<(), String> {
        if !self.cache_enabled {
            return Ok(());
        }
        ensure_cache_layout(&self.root)?;
        let mut body = String::from(LIVE_MAGIC);
        body.push('\n');
        for (path, key) in live {
            body.push_str(&encode_hex(path.as_bytes()));
            body.push('\t');
            body.push_str(key);
            body.push('\n');
        }
        let unique = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let temp = self.root.join(format!(".LIVE.{unique}.tmp"));
        write_complete(&temp, body.as_bytes())?;
        let live_path = self.root.join("LIVE");
        match fs::rename(&temp, &live_path) {
            Ok(()) => Ok(()),
            Err(_) if live_path.exists() => {
                fs::remove_file(&live_path).map_err(|error| error.to_string())?;
                fs::rename(&temp, &live_path).map_err(|error| error.to_string())
            }
            Err(error) => Err(error.to_string()),
        }
    }

    pub fn gc(&self, live: &BTreeMap<String, String>) -> Result<OfficeCacheGcReport, String> {
        if !self.cache_enabled {
            return Ok(OfficeCacheGcReport::default());
        }
        let referenced = live.values().cloned().collect::<HashSet<_>>();
        let object_root = self.root.join("objects");
        let mut candidates = Vec::new();
        let mut before_bytes = 0u64;
        if object_root.exists() {
            for shard in fs::read_dir(&object_root).map_err(|error| error.to_string())? {
                let shard = shard.map_err(|error| error.to_string())?;
                if !shard
                    .file_type()
                    .map_err(|error| error.to_string())?
                    .is_dir()
                {
                    continue;
                }
                for entry in fs::read_dir(shard.path()).map_err(|error| error.to_string())? {
                    let entry = entry.map_err(|error| error.to_string())?;
                    let path = entry.path();
                    if path.extension().and_then(|value| value.to_str()) != Some("txt") {
                        continue;
                    }
                    let Some(stem) = path.file_stem().and_then(|value| value.to_str()) else {
                        continue;
                    };
                    if stem.len() != 32 || !stem.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                        continue;
                    }
                    let key = stem.to_ascii_lowercase();
                    let meta = path.with_extension("meta");
                    let text_meta = fs::metadata(&path).map_err(|error| error.to_string())?;
                    let meta_len = fs::metadata(&meta).map_or(0, |value| value.len());
                    let bytes = text_meta.len().saturating_add(meta_len);
                    before_bytes = before_bytes.saturating_add(bytes);
                    if referenced.contains(&key) {
                        continue;
                    }
                    let modified = text_meta.modified().unwrap_or(UNIX_EPOCH);
                    candidates.push((modified, key, path, meta, bytes));
                }
            }
        }
        let mut report = OfficeCacheGcReport {
            before_bytes,
            after_bytes: before_bytes,
            ..OfficeCacheGcReport::default()
        };
        if before_bytes <= self.config.cache_soft_limit_bytes {
            return Ok(report);
        }
        candidates.sort_by_key(|candidate| candidate.0);
        let now = SystemTime::now();
        for (modified, _key, text, meta, bytes) in candidates {
            if report.after_bytes <= self.config.cache_target_bytes {
                break;
            }
            if now.duration_since(modified).unwrap_or_default() < self.config.cache_grace {
                continue;
            }
            if fs::remove_file(&text).is_err() {
                continue;
            }
            let _ = fs::remove_file(meta);
            report.deleted_objects += 1;
            report.reclaimed_bytes = report.reclaimed_bytes.saturating_add(bytes);
            report.after_bytes = report.after_bytes.saturating_sub(bytes);
        }
        Ok(report)
    }
}

fn bounded_worker_count(
    requests: &[OfficeExtractionRequest],
    max_workers: usize,
    memory_budget: u64,
) -> usize {
    let mut sizes = requests
        .iter()
        .map(|request| request.source_bytes.max(1))
        .collect::<Vec<_>>();
    sizes.sort_unstable_by(|left, right| right.cmp(left));
    let mut sum = 0u64;
    let mut workers = 0usize;
    for size in sizes.into_iter().take(max_workers.max(1)) {
        if workers > 0 && sum.saturating_add(size) > memory_budget {
            break;
        }
        sum = sum.saturating_add(size);
        workers += 1;
    }
    workers.max(1).min(requests.len())
}

fn ensure_cache_layout(root: &Path) -> Result<(), String> {
    fs::create_dir_all(root.join("objects")).map_err(|error| error.to_string())?;
    fs::create_dir_all(root.join("tmp")).map_err(|error| error.to_string())?;
    Ok(())
}

fn write_complete(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = File::create(path).map_err(|error| error.to_string())?;
    file.write_all(bytes).map_err(|error| error.to_string())?;
    file.flush().map_err(|error| error.to_string())
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    let slice = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| "truncated ZIP field".to_owned())?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| "truncated ZIP field".to_owned())?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn office_fingerprint(path: &Path, budget: ExtractionBudget) -> Result<OfficeFingerprint, String> {
    let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
    if metadata.len() > budget.max_source_bytes {
        return Err(format!(
            "structured file exceeds extraction source budget: {} bytes",
            metadata.len()
        ));
    }
    let kind = OfficeKind::from_path(path)
        .ok_or_else(|| format!("unsupported Office container: {}", path.display()))?;
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    let file_len = metadata.len();
    if file_len < 22 {
        return Err("Office container is not a ZIP file".to_owned());
    }
    let tail_len = file_len.min(22 + 65_535) as usize;
    file.seek(SeekFrom::End(-(tail_len as i64)))
        .map_err(|error| error.to_string())?;
    let mut tail = vec![0u8; tail_len];
    file.read_exact(&mut tail)
        .map_err(|error| error.to_string())?;
    let eocd = (0..=tail.len() - 22)
        .rev()
        .find(|offset| tail.get(*offset..*offset + 4) == Some(&[0x50, 0x4b, 0x05, 0x06]))
        .ok_or_else(|| "ZIP end-of-central-directory not found".to_owned())?;
    if read_u16(&tail, eocd + 4)? != 0 || read_u16(&tail, eocd + 6)? != 0 {
        return Err("multi-disk ZIP containers are not supported".to_owned());
    }
    let entries = usize::from(read_u16(&tail, eocd + 10)?);
    let central_size = u64::from(read_u32(&tail, eocd + 12)?);
    let central_offset = u64::from(read_u32(&tail, eocd + 16)?);
    if central_offset
        .checked_add(central_size)
        .is_none_or(|end| end > file_len)
    {
        return Err("ZIP central directory is out of bounds".to_owned());
    }
    file.seek(SeekFrom::Start(central_offset))
        .map_err(|error| error.to_string())?;
    let mut central = vec![0u8; central_size as usize];
    file.read_exact(&mut central)
        .map_err(|error| error.to_string())?;
    let mut parsed = Vec::with_capacity(entries);
    let mut offset = 0usize;
    for _ in 0..entries {
        if central.get(offset..offset + 4) != Some(&[0x50, 0x4b, 0x01, 0x02]) {
            return Err("bad ZIP central directory entry".to_owned());
        }
        let flags = read_u16(&central, offset + 8)?;
        let method = read_u16(&central, offset + 10)?;
        let crc32 = read_u32(&central, offset + 16)?;
        let compressed_size = u64::from(read_u32(&central, offset + 20)?);
        let uncompressed_size = u64::from(read_u32(&central, offset + 24)?);
        let name_len = usize::from(read_u16(&central, offset + 28)?);
        let extra_len = usize::from(read_u16(&central, offset + 30)?);
        let comment_len = usize::from(read_u16(&central, offset + 32)?);
        let local_offset = u64::from(read_u32(&central, offset + 42)?);
        let name_start = offset + 46;
        let name_end = name_start
            .checked_add(name_len)
            .ok_or_else(|| "ZIP name overflow".to_owned())?;
        let name = std::str::from_utf8(
            central
                .get(name_start..name_end)
                .ok_or_else(|| "truncated ZIP name".to_owned())?,
        )
        .map_err(|_| "non UTF-8 ZIP entry name".to_owned())?
        .to_owned();
        if kind.include_entry(&name) {
            parsed.push(CentralEntry {
                name,
                method,
                flags,
                crc32,
                compressed_size,
                uncompressed_size,
                local_offset,
            });
        }
        offset = name_end
            .checked_add(extra_len)
            .and_then(|value| value.checked_add(comment_len))
            .ok_or_else(|| "ZIP central entry overflow".to_owned())?;
        if offset > central.len() {
            return Err("truncated ZIP central directory".to_owned());
        }
    }

    let mut fingerprints = Vec::with_capacity(parsed.len());
    for entry in parsed {
        file.seek(SeekFrom::Start(entry.local_offset))
            .map_err(|error| error.to_string())?;
        let mut header = [0u8; 30];
        file.read_exact(&mut header)
            .map_err(|error| error.to_string())?;
        if header[..4] != [0x50, 0x4b, 0x03, 0x04] {
            return Err("bad ZIP local entry".to_owned());
        }
        let name_len = u64::from(read_u16(&header, 26)?);
        let extra_len = u64::from(read_u16(&header, 28)?);
        let payload_offset = entry
            .local_offset
            .checked_add(30)
            .and_then(|value| value.checked_add(name_len))
            .and_then(|value| value.checked_add(extra_len))
            .ok_or_else(|| "ZIP local payload overflow".to_owned())?;
        if payload_offset
            .checked_add(entry.compressed_size)
            .is_none_or(|end| end > file_len)
        {
            return Err("ZIP payload is out of bounds".to_owned());
        }
        file.seek(SeekFrom::Start(payload_offset))
            .map_err(|error| error.to_string())?;
        let (hash_a, hash_b) = hash_reader(&mut file, entry.compressed_size)?;
        fingerprints.push(EntryFingerprint {
            name: entry.name,
            method: entry.method,
            flags: entry.flags,
            crc32: entry.crc32,
            compressed_size: entry.compressed_size,
            uncompressed_size: entry.uncompressed_size,
            compressed_hash_a: hash_a,
            compressed_hash_b: hash_b,
        });
    }
    if fingerprints.is_empty() {
        return Err("Office container has no searchable XML parts".to_owned());
    }
    Ok(OfficeFingerprint {
        kind,
        entries: fingerprints,
    })
}

fn hash_reader(reader: &mut File, mut remaining: u64) -> Result<(u64, u64), String> {
    let mut state_a = 0xcbf29ce484222325u64;
    let mut state_b = 0x84222325cbf29ce4u64;
    let mut buffer = [0u8; 64 * 1024];
    while remaining > 0 {
        let take = remaining.min(buffer.len() as u64) as usize;
        reader
            .read_exact(&mut buffer[..take])
            .map_err(|error| error.to_string())?;
        hash_update(&mut state_a, &mut state_b, &buffer[..take]);
        remaining -= take as u64;
    }
    Ok((state_a, state_b))
}

fn hash_file(path: &Path) -> Result<(u64, u64), String> {
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    let len = file.metadata().map_err(|error| error.to_string())?.len();
    hash_reader(&mut file, len)
}

fn hash128(bytes: &[u8]) -> (u64, u64) {
    let mut a = 0xcbf29ce484222325u64;
    let mut b = 0x84222325cbf29ce4u64;
    hash_update(&mut a, &mut b, bytes);
    (a, b)
}

fn hash_update(a: &mut u64, b: &mut u64, bytes: &[u8]) {
    for &byte in bytes {
        *a ^= u64::from(byte);
        *a = a.wrapping_mul(0x100000001b3);
        *b ^= u64::from(byte).rotate_left(1);
        *b = b.wrapping_mul(0x100000001b3).rotate_left(7);
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn decode_hex(value: &str) -> Result<Vec<u8>, ()> {
    if !value.len().is_multiple_of(2) {
        return Err(());
    }
    let mut out = Vec::with_capacity(value.len() / 2);
    let bytes = value.as_bytes();
    for index in (0..bytes.len()).step_by(2) {
        let high = hex_value(bytes[index]).ok_or(())?;
        let low = hex_value(bytes[index + 1]).ok_or(())?;
        out.push((high << 4) | low);
    }
    Ok(out)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
