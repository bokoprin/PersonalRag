use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use crate::fold_ascii;
use crate::format::section;
use crate::format::{
    BuilderKind, FNV_OFFSET, FNV_PRIME, FOOTER_MAGIC, HEADER_SIZE, MANIFEST_MAGIC, Q3DirKind,
    Q3Encoding, Result, SECTION_COUNT, SEG_MAGIC, SearchError, align8, k2, k3, put_u16, put_u32,
    write_u32, write_u64,
};
use crate::index::{
    AccelerationProfile, MemoryAccelerationRequest, build_accelerators_from_memory,
};
use crate::types::DocumentInput;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuildMode {
    Direct,
    Dedup,
    Adaptive,
}

impl BuildMode {
    fn label(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Dedup => "dedup",
            Self::Adaptive => "adaptive",
        }
    }
}

#[derive(Clone, Debug)]
pub struct BuildOptions {
    pub mode: BuildMode,
    pub segment_docs: usize,
    pub workers: usize,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            mode: BuildMode::Adaptive,
            segment_docs: 5_000,
            workers: 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuildTuning {
    pub segment_docs: usize,
    pub build_workers: usize,
    pub scan_workers: usize,
    pub memory_budget_bytes: u64,
    pub logical_cpus: usize,
}

const MIB: u64 = 1024 * 1024;

/// Selects one of the measured production build points from an explicit process-memory budget.
///
/// The thresholds are intentionally conservative relative to the 100k source-like measurements
/// from the R5 workload suite. They are a policy layer, not a correctness dependency; callers can
/// still override `BuildOptions` explicitly.
#[must_use]
pub fn recommend_build_tuning(memory_budget_bytes: u64, logical_cpus: usize) -> BuildTuning {
    let cpus = logical_cpus.max(1);
    let (segment_docs, build_workers) = if cpus >= 4 {
        if memory_budget_bytes >= 330 * MIB {
            (5_000, 4)
        } else if memory_budget_bytes >= 255 * MIB {
            (2_500, 4)
        } else if memory_budget_bytes >= 220 * MIB {
            (2_500, 2)
        } else {
            (2_500, 1)
        }
    } else if cpus >= 2 {
        if memory_budget_bytes >= 255 * MIB {
            (5_000, 2)
        } else if memory_budget_bytes >= 220 * MIB {
            (2_500, 2)
        } else {
            (2_500, 1)
        }
    } else if memory_budget_bytes >= 220 * MIB {
        (5_000, 1)
    } else {
        (2_500, 1)
    };
    BuildTuning {
        segment_docs,
        build_workers,
        scan_workers: cpus.min(2),
        memory_budget_bytes,
        logical_cpus: cpus,
    }
}

#[cfg(target_os = "linux")]
fn linux_available_memory_bytes() -> Option<u64> {
    let meminfo = fs::read_to_string("/proc/meminfo").ok()?;
    let host = meminfo.lines().find_map(|line| {
        let rest = line.strip_prefix("MemAvailable:")?;
        let kib = rest.split_whitespace().next()?.parse::<u64>().ok()?;
        kib.checked_mul(1024)
    });
    let cgroup = (|| {
        let maximum = fs::read_to_string("/sys/fs/cgroup/memory.max").ok()?;
        let maximum = maximum.trim().parse::<u64>().ok()?;
        let current = fs::read_to_string("/sys/fs/cgroup/memory.current")
            .ok()?
            .trim()
            .parse::<u64>()
            .ok()?;
        Some(maximum.saturating_sub(current))
    })();
    match (host, cgroup) {
        (Some(host), Some(cgroup)) => Some(host.min(cgroup)),
        (Some(host), None) => Some(host),
        (None, Some(cgroup)) => Some(cgroup),
        (None, None) => None,
    }
}

#[cfg(windows)]
fn windows_available_memory_bytes() -> Option<u64> {
    #[repr(C)]
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_phys: u64,
        avail_phys: u64,
        total_page_file: u64,
        avail_page_file: u64,
        total_virtual: u64,
        avail_virtual: u64,
        avail_extended_virtual: u64,
    }
    unsafe extern "system" {
        fn GlobalMemoryStatusEx(status: *mut MemoryStatusEx) -> i32;
    }
    let mut status = MemoryStatusEx {
        length: u32::try_from(core::mem::size_of::<MemoryStatusEx>()).ok()?,
        memory_load: 0,
        total_phys: 0,
        avail_phys: 0,
        total_page_file: 0,
        avail_page_file: 0,
        total_virtual: 0,
        avail_virtual: 0,
        avail_extended_virtual: 0,
    };
    (unsafe { GlobalMemoryStatusEx(&raw mut status) } != 0).then_some(status.avail_phys)
}

#[must_use]
pub fn detected_available_memory_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        linux_available_memory_bytes()
    }
    #[cfg(windows)]
    {
        windows_available_memory_bytes()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        None
    }
}

#[must_use]
pub fn recommend_system_build_tuning() -> BuildTuning {
    let available = detected_available_memory_bytes().unwrap_or(512 * MIB);
    // Keep headroom for the GUI, filesystem cache, and temporary allocations outside the core.
    let budget = available.saturating_mul(70) / 100;
    let cpus = std::thread::available_parallelism().map_or(1, usize::from);
    recommend_build_tuning(budget, cpus)
}

#[derive(Clone, Debug)]
pub struct BuildReport {
    pub docs: usize,
    pub segments: usize,
    pub index_bytes: u64,
    pub elapsed: Duration,
    pub output_dir: PathBuf,
}

#[derive(Clone, Debug, Default)]
pub struct DiskPathBuildProgress {
    pub source_files: usize,
    pub processed_files: usize,
    pub indexed_files: usize,
    pub skipped_files: usize,
    pub bytes_read: u64,
    /// Approximate normalized content bytes currently retained by the hydration batch.
    pub prepared_bytes: u64,
    pub current_path: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DiskPathBuildTimings {
    /// End-to-end hydration wall time. This can overlap segment building.
    pub hydration_wall: Duration,
    /// Summed worker time across all segments; may exceed wall time when workers overlap.
    pub segment_sample_work: Duration,
    pub segment_core_work: Duration,
    pub name_grams_work: Duration,
    pub dedup_work: Duration,
    pub content_grams_work: Duration,
    pub content_post_work: Duration,
    pub name_post_work: Duration,
    pub segment_write_work: Duration,
    pub acceleration_work: Duration,
    /// Manifest serialization/write wall time after segment workers complete.
    pub manifest_write_wall: Duration,
}

#[derive(Clone, Debug)]
pub struct DiskPathBuildReport {
    pub build: BuildReport,
    pub display_paths: Vec<String>,
    /// Source-list index for each indexed document, in document-ID order.
    ///
    /// Application adapters can use this to carry scan-time metadata into their own catalog
    /// without re-statting every file after the portable build has skipped unreadable entries.
    pub source_indices: Vec<u32>,
    pub source_files: usize,
    pub processed_files: usize,
    pub skipped_files: usize,
    pub bytes_read: u64,
    pub timings: DiskPathBuildTimings,
}

/// A file selected by an application-owned scanner.
///
/// Supplying the already-known display path and size lets the portable build pipeline avoid a
/// second metadata lookup and per-file canonicalization. This does not change the on-disk index
/// format; it is only a faster ingestion boundary for scanners that already collected metadata.
#[derive(Clone, Debug)]
pub struct DiskPathInput {
    pub path: PathBuf,
    pub display_path: String,
    pub size_bytes: u64,
    /// Optional application-prepared UTF-8/text payload.
    ///
    /// The portable core treats this as an opaque text source and deliberately has no knowledge
    /// of the original source format. When absent, `path` is read.
    pub content_path: Option<PathBuf>,
    /// Whether the file body should be read and indexed.
    ///
    /// Applications can set this to `false` for images, executables, archives and other binary
    /// formats while keeping the filename/path searchable.
    pub index_content: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct DiskPathBuildConfig<'a> {
    pub max_docs: Option<usize>,
    pub max_file_bytes: u64,
    pub build: &'a BuildOptions,
    pub scan_workers: usize,
    /// Maximum aggregate content bytes hydrated into one application-owned batch.
    ///
    /// `0` disables the byte limit and retains the historical file-count-only batching.
    pub hydration_batch_bytes: u64,
    pub cancel: Option<&'a AtomicBool>,
}

#[derive(Clone, Copy, Debug, Default)]
struct SampleStats {
    duplicate_ratio: f64,
    run_unique_ratio: f64,
}

#[derive(Clone, Debug)]
struct ManifestEntry {
    file: String,
    base: u32,
    count: u32,
    kind: BuilderKind,
    sample: SampleStats,
    bytes: u64,
}

type PipelineBuildOutput = (
    Vec<ManifestEntry>,
    usize,
    Vec<String>,
    Vec<u32>,
    Vec<DocumentInput>,
);

struct DiskPipelineResult {
    report: DiskPathBuildReport,
    retained_documents: Vec<DocumentInput>,
}

#[derive(Clone, Default)]
struct GramSet {
    q1: Vec<u8>,
    q2: Vec<u16>,
    q3: Vec<u32>,
}

#[derive(Default)]
struct RawPostingData {
    q1off: Vec<u32>,
    q1post: Vec<u32>,
    q2off: Vec<u32>,
    q2post: Vec<u32>,
    q3dir: Vec<u32>,
    q3post: Vec<u32>,
}

/// Streaming builder for the normal <= u16::MAX document segment case.
///
/// q2/q3 occurrences are emitted directly as packed `(gram, document)` pairs. Duplicate
/// occurrences within the same filename are deliberately kept until the global radix pass,
/// where identical packed pairs are adjacent and can be removed at negligible extra cost.
/// This avoids allocating/sorting/deduplicating three temporary GramSet vectors per file.
struct NamePostingEmitter {
    q1: Vec<Vec<u32>>,
    q2_pairs: Vec<u32>,
    q3_shards: Vec<Vec<u32>>,
}

impl NamePostingEmitter {
    fn with_capacity(total_name_bytes: usize) -> Self {
        let q3_shard_capacity = total_name_bytes.div_ceil(256);
        Self {
            q1: (0..256).map(|_| Vec::<u32>::new()).collect(),
            q2_pairs: Vec::with_capacity(total_name_bytes),
            q3_shards: (0..256)
                .map(|_| Vec::<u32>::with_capacity(q3_shard_capacity))
                .collect(),
        }
    }

    fn emit(&mut self, document_id: usize, bytes: &[u8]) -> Result<()> {
        let id = u16::try_from(document_id)
            .map_err(|_| SearchError::Format("name posting id overflow".into()))?;
        let id_u32 = u32::from(id);
        let mut q1_seen = [0u64; 4];
        let mut previous2 = 0u8;
        let mut previous1 = 0u8;

        for (index, &byte) in bytes.iter().enumerate() {
            let word = usize::from(byte >> 6);
            let bit = 1u64 << (byte & 63);
            if q1_seen[word] & bit == 0 {
                q1_seen[word] |= bit;
                self.q1[usize::from(byte)].push(id_u32);
            }
            if index >= 1 {
                self.q2_pairs
                    .push((u32::from(k2(previous1, byte)) << 16) | id_u32);
            }
            if index >= 2 {
                let key = k3(previous2, previous1, byte);
                let shard = (key >> 16) as usize;
                self.q3_shards[shard].push(((key & 0xffff) << 16) | id_u32);
            }
            previous2 = previous1;
            previous1 = byte;
        }
        Ok(())
    }

    fn finish(self) -> Result<RawPostingData> {
        let mut out = RawPostingData {
            q1off: vec![0; 257],
            q2off: vec![0; 65_537],
            ..RawPostingData::default()
        };
        finish_raw_q1(&mut out, &self.q1)?;

        let mut scratch = Vec::<u32>::new();
        let mut counts = vec![0usize; 65_536];
        stable_group_u32_by_upper16(&self.q2_pairs, &mut scratch, &mut counts);
        let mut position = 0usize;
        let mut previous_q2_pair: Option<u32> = None;
        for key in 0..65_536u32 {
            out.q2off[key as usize] = u32::try_from(out.q2post.len())
                .map_err(|_| SearchError::Format("q2 posting overflow".into()))?;
            while position < scratch.len() && (scratch[position] >> 16) == key {
                let pair = scratch[position];
                if previous_q2_pair != Some(pair) {
                    out.q2post.push(pair & 0xffff);
                    previous_q2_pair = Some(pair);
                }
                position += 1;
            }
        }
        out.q2off[65_536] = u32::try_from(out.q2post.len())
            .map_err(|_| SearchError::Format("q2 posting overflow".into()))?;

        for (high, shard) in self.q3_shards.iter().enumerate() {
            if shard.is_empty() {
                continue;
            }
            stable_group_u32_by_upper16(shard, &mut scratch, &mut counts);
            position = 0;
            let mut previous_q3_pair: Option<u32> = None;
            while position < scratch.len() {
                let suffix = scratch[position] >> 16;
                let key = ((high as u32) << 16) | suffix;
                let start = u32::try_from(out.q3post.len())
                    .map_err(|_| SearchError::Format("name q3 posting overflow".into()))?;
                while position < scratch.len() && (scratch[position] >> 16) == suffix {
                    let pair = scratch[position];
                    if previous_q3_pair != Some(pair) {
                        out.q3post.push(pair & 0xffff);
                        previous_q3_pair = Some(pair);
                    }
                    position += 1;
                }
                out.q3dir.push(key);
                out.q3dir.push(start);
                let end = u32::try_from(out.q3post.len())
                    .map_err(|_| SearchError::Format("name q3 posting overflow".into()))?;
                out.q3dir.push(end - start);
            }
        }
        Ok(out)
    }
}

#[derive(Default)]
struct ContentPostingData {
    q1mask: Vec<u8>,
    q3dir: Vec<u8>,
    q3blob: Vec<u8>,
}

struct SegmentData {
    doc_base: u32,
    doc_count: u32,
    kind: BuilderKind,
    name_off: Vec<u64>,
    names: Vec<u8>,
    unit_text_off: Vec<u64>,
    texts: Vec<u8>,
    unit_doc_off: Vec<u64>,
    unit_docs: Vec<u32>,
    doc_unit: Vec<u32>,
    content: ContentPostingData,
    name_index: RawPostingData,
    q2_pairs: Option<Vec<u32>>,
}

trait DiskPathSource: Sync {
    fn path(&self) -> &Path;

    fn known_display_path(&self) -> Option<&str> {
        None
    }

    fn known_size_bytes(&self) -> Option<u64> {
        None
    }

    fn index_content(&self) -> bool {
        true
    }

    fn content_path(&self) -> &Path {
        self.path()
    }
}

impl DiskPathSource for PathBuf {
    fn path(&self) -> &Path {
        self.as_path()
    }
}

impl DiskPathSource for DiskPathInput {
    fn path(&self) -> &Path {
        self.path.as_path()
    }

    fn known_display_path(&self) -> Option<&str> {
        Some(self.display_path.as_str())
    }

    fn known_size_bytes(&self) -> Option<u64> {
        Some(self.size_bytes)
    }

    fn index_content(&self) -> bool {
        self.index_content
    }

    fn content_path(&self) -> &Path {
        self.content_path.as_deref().unwrap_or_else(|| self.path())
    }
}

fn hydrate_disk_document<T: DiskPathSource>(
    root: &Path,
    canonical_root: &Path,
    source: &T,
    max_file_bytes: u64,
) -> Option<DocumentInput> {
    let path = source.path();
    let size_bytes = match source.known_size_bytes() {
        Some(size_bytes) => size_bytes,
        None => fs::metadata(path).ok()?.len(),
    };
    if max_file_bytes > 0 && size_bytes > max_file_bytes {
        return None;
    }
    let display = if let Some(display_path) = source.known_display_path() {
        display_path.to_owned()
    } else {
        // std::filesystem::relative (used by the C++ oracle) applies weak canonicalization,
        // so a symlink-to-file is displayed relative to its resolved target.
        let resolved = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let relative = relative_path(&resolved, canonical_root)
            .or_else(|| path.strip_prefix(root).ok().map(Path::to_path_buf))
            .unwrap_or_else(|| path.to_path_buf());
        path_to_portable(&relative)
    };
    let profile_build = profile_build_enabled();
    let normalized_content = if source.index_content() {
        let read_started = profile_build.then(Instant::now);
        let mut content = fs::read(source.content_path()).ok()?;
        if let Some(started) = read_started {
            PROFILE_HYDRATION_READ_NS.fetch_add(
                started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
                Ordering::Relaxed,
            );
        }
        let normalize_started = profile_build.then(Instant::now);
        content.make_ascii_lowercase();
        if let Some(started) = normalize_started {
            PROFILE_HYDRATION_NORMALIZE_NS.fetch_add(
                started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
                Ordering::Relaxed,
            );
        }
        content
    } else {
        Vec::new()
    };
    let normalize_started = profile_build.then(Instant::now);
    let normalized_name = fold_ascii(display.as_bytes());
    if let Some(started) = normalize_started {
        PROFILE_HYDRATION_NORMALIZE_NS.fetch_add(
            started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
    }
    Some(DocumentInput::new(
        display.clone(),
        display.clone(),
        normalized_name,
        normalized_content,
    ))
}

static PROFILE_BUILD_ENABLED: OnceLock<bool> = OnceLock::new();
static PROFILE_HYDRATION_READ_NS: AtomicU64 = AtomicU64::new(0);
static PROFILE_HYDRATION_NORMALIZE_NS: AtomicU64 = AtomicU64::new(0);

fn profile_build_enabled() -> bool {
    *PROFILE_BUILD_ENABLED.get_or_init(|| std::env::var_os("PR_PROFILE_BUILD").is_some())
}

const HYDRATION_RESULT_CHUNK_FILES: usize = 64;
const HYDRATION_PROGRESS_FILE_STRIDE: usize = 128;
const HYDRATION_PROGRESS_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, Default)]
struct HydrationProgress {
    completed_files: usize,
    bytes_read: u64,
    current_index: usize,
}

fn hydrated_bytes(document: &Option<DocumentInput>) -> u64 {
    document
        .as_ref()
        .map_or(0, |document| document.normalized_content.len() as u64)
}

fn build_cancelled(cancel: Option<&AtomicBool>) -> bool {
    cancel.is_some_and(|flag| flag.load(Ordering::Acquire))
}

fn hydrate_disk_paths_parallel_observed<T, F>(
    root: &Path,
    canonical_root: &Path,
    files: &[T],
    max_file_bytes: u64,
    workers: usize,
    cancel: Option<&AtomicBool>,
    mut on_progress: F,
) -> Result<Vec<Option<DocumentInput>>>
where
    T: DiskPathSource,
    F: FnMut(HydrationProgress),
{
    if files.is_empty() {
        return Ok(Vec::new());
    }
    let workers = workers.max(1).min(files.len());
    let mut hydrated = std::iter::repeat_with(|| None)
        .take(files.len())
        .collect::<Vec<Option<DocumentInput>>>();
    let mut observed = HydrationProgress::default();
    let mut last_emitted_files = 0usize;
    let mut last_emit = Instant::now();
    let mut observe_document = |index: usize, document: Option<DocumentInput>| {
        observed.completed_files = observed.completed_files.saturating_add(1);
        observed.bytes_read = observed
            .bytes_read
            .saturating_add(hydrated_bytes(&document));
        observed.current_index = index;
        hydrated[index] = document;
        if observed.completed_files == files.len()
            || observed.completed_files.saturating_sub(last_emitted_files)
                >= HYDRATION_PROGRESS_FILE_STRIDE
            || last_emit.elapsed() >= HYDRATION_PROGRESS_INTERVAL
        {
            on_progress(observed);
            last_emitted_files = observed.completed_files;
            last_emit = Instant::now();
        }
    };

    if workers == 1 {
        for (index, source) in files.iter().enumerate() {
            if build_cancelled(cancel) {
                return Err(SearchError::InvalidArgument("build cancelled".into()));
            }
            observe_document(
                index,
                hydrate_disk_document(root, canonical_root, source, max_file_bytes),
            );
        }
        return Ok(hydrated);
    }

    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| -> Result<()> {
        let (result_tx, result_rx) = mpsc::sync_channel::<Vec<(usize, Option<DocumentInput>)>>(
            workers.saturating_mul(2).max(1),
        );
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let next = &next;
            let result_tx = result_tx.clone();
            handles.push(scope.spawn(move || {
                let mut local = Vec::with_capacity(HYDRATION_RESULT_CHUNK_FILES);
                let mut last_flush = Instant::now();
                loop {
                    if build_cancelled(cancel) {
                        break;
                    }
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(source) = files.get(index) else {
                        break;
                    };
                    local.push((
                        index,
                        hydrate_disk_document(root, canonical_root, source, max_file_bytes),
                    ));
                    if local.len() >= HYDRATION_RESULT_CHUNK_FILES
                        || last_flush.elapsed() >= HYDRATION_PROGRESS_INTERVAL
                    {
                        if result_tx.send(core::mem::take(&mut local)).is_err() {
                            return;
                        }
                        local = Vec::with_capacity(HYDRATION_RESULT_CHUNK_FILES);
                        last_flush = Instant::now();
                    }
                }
                if !local.is_empty() {
                    let _ = result_tx.send(local);
                }
            }));
        }
        drop(result_tx);
        for batch in result_rx {
            for (index, document) in batch {
                observe_document(index, document);
            }
        }
        for handle in handles {
            handle
                .join()
                .map_err(|_| SearchError::Format("disk hydration worker panicked".into()))?;
        }
        Ok(())
    })?;
    if build_cancelled(cancel) {
        return Err(SearchError::InvalidArgument("build cancelled".into()));
    }
    if observed.completed_files != files.len() {
        return Err(SearchError::Format(format!(
            "disk hydration ended early: {}/{} files",
            observed.completed_files,
            files.len()
        )));
    }
    Ok(hydrated)
}

fn next_hydration_batch_end<T: DiskPathSource>(
    files: &[T],
    start: usize,
    max_files: usize,
    max_bytes: u64,
) -> usize {
    let hard_end = start.saturating_add(max_files.max(1)).min(files.len());
    if max_bytes == 0 || start >= hard_end {
        return hard_end;
    }
    let mut end = start;
    let mut bytes = 0u64;
    while end < hard_end {
        let source = &files[end];
        let hint = if source.index_content() {
            source.known_size_bytes().unwrap_or(0)
        } else {
            0
        };
        if end > start && bytes.saturating_add(hint) > max_bytes {
            break;
        }
        bytes = bytes.saturating_add(hint);
        end += 1;
        if bytes >= max_bytes {
            break;
        }
    }
    end.max(start.saturating_add(1)).min(hard_end)
}

fn hydrate_disk_paths_parallel<T: DiskPathSource>(
    root: &Path,
    canonical_root: &Path,
    files: &[T],
    max_file_bytes: u64,
    workers: usize,
) -> Result<Vec<Option<DocumentInput>>> {
    hydrate_disk_paths_parallel_observed(
        root,
        canonical_root,
        files,
        max_file_bytes,
        workers,
        None,
        |_| {},
    )
}

pub fn build_disk_corpus(
    root: impl AsRef<Path>,
    max_docs: Option<usize>,
    max_file_bytes: u64,
) -> Result<Vec<DocumentInput>> {
    build_disk_corpus_parallel(root, max_docs, max_file_bytes, 1)
}

pub fn build_disk_corpus_parallel(
    root: impl AsRef<Path>,
    max_docs: Option<usize>,
    max_file_bytes: u64,
    workers: usize,
) -> Result<Vec<DocumentInput>> {
    let root = root.as_ref();
    if !root.is_dir() {
        return Err(SearchError::InvalidArgument(format!(
            "disk corpus root is not a directory: {}",
            root.display()
        )));
    }
    let mut files = Vec::new();
    collect_files(root, &mut files)?;
    files.sort_by_key(|path| path_to_portable(path));
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let hydrated =
        hydrate_disk_paths_parallel(root, &canonical_root, &files, max_file_bytes, workers)?;
    Ok(hydrated
        .into_iter()
        .flatten()
        .take(max_docs.unwrap_or(usize::MAX))
        .collect())
}

struct SegmentBuildTask {
    documents: Vec<DocumentInput>,
    doc_base: usize,
    segment_index: usize,
}

#[derive(Default)]
struct BuildTimingAccumulator {
    segment_sample_ns: AtomicU64,
    segment_core_ns: AtomicU64,
    name_grams_ns: AtomicU64,
    dedup_ns: AtomicU64,
    content_grams_ns: AtomicU64,
    content_post_ns: AtomicU64,
    name_post_ns: AtomicU64,
    segment_write_ns: AtomicU64,
    acceleration_ns: AtomicU64,
}

fn add_duration(counter: &AtomicU64, elapsed: Duration) {
    counter.fetch_add(
        elapsed.as_nanos().min(u128::from(u64::MAX)) as u64,
        Ordering::Relaxed,
    );
}

fn accumulated_duration(counter: &AtomicU64) -> Duration {
    Duration::from_nanos(counter.load(Ordering::Relaxed))
}

impl BuildTimingAccumulator {
    fn snapshot(
        &self,
        hydration_wall_ns: u64,
        manifest_write_wall: Duration,
    ) -> DiskPathBuildTimings {
        DiskPathBuildTimings {
            hydration_wall: Duration::from_nanos(hydration_wall_ns),
            segment_sample_work: accumulated_duration(&self.segment_sample_ns),
            segment_core_work: accumulated_duration(&self.segment_core_ns),
            name_grams_work: accumulated_duration(&self.name_grams_ns),
            dedup_work: accumulated_duration(&self.dedup_ns),
            content_grams_work: accumulated_duration(&self.content_grams_ns),
            content_post_work: accumulated_duration(&self.content_post_ns),
            name_post_work: accumulated_duration(&self.name_post_ns),
            segment_write_work: accumulated_duration(&self.segment_write_ns),
            acceleration_work: accumulated_duration(&self.acceleration_ns),
            manifest_write_wall,
        }
    }
}

fn should_collect_q2_seed(profile: AccelerationProfile, documents: &[DocumentInput]) -> bool {
    match profile {
        AccelerationProfile::Full | AccelerationProfile::Balanced => documents
            .iter()
            .any(|document| !document.normalized_content.is_empty()),
        AccelerationProfile::AdaptiveDelta => {
            documents.len() >= 64
                && documents
                    .iter()
                    .map(|doc| doc.normalized_content.len() as u64)
                    .sum::<u64>()
                    >= 64 * 1024
        }
        AccelerationProfile::None => false,
    }
}

struct OwnedSegmentBuildConfig<'a> {
    output_dir: &'a Path,
    mode: BuildMode,
    acceleration: AccelerationProfile,
    build_workers: usize,
    durable: bool,
    retain_documents: bool,
    timings: Option<&'a BuildTimingAccumulator>,
}

fn build_owned_segment_profile(
    task: SegmentBuildTask,
    config: OwnedSegmentBuildConfig<'_>,
) -> Result<(ManifestEntry, Option<Vec<DocumentInput>>)> {
    let OwnedSegmentBuildConfig {
        output_dir,
        mode,
        acceleration,
        build_workers,
        durable,
        retain_documents,
        timings,
    } = config;
    let profile_build = profile_build_enabled();
    let total_started = Instant::now();
    let sample_started = Instant::now();
    let sample = sample_stats(&task.documents);
    let sample_elapsed = sample_started.elapsed();
    let sample_ms = sample_elapsed.as_secs_f64() * 1000.0;
    if let Some(timings) = timings {
        add_duration(&timings.segment_sample_ns, sample_elapsed);
    }
    let kind = match mode {
        BuildMode::Direct => BuilderKind::Direct,
        BuildMode::Dedup => BuilderKind::Dedup,
        BuildMode::Adaptive if sample.duplicate_ratio >= 0.20 => BuilderKind::Dedup,
        BuildMode::Adaptive => BuilderKind::Direct,
    };
    let count = task.documents.len();
    let collect_q2 = should_collect_q2_seed(acceleration, &task.documents);
    let base_started = Instant::now();
    let mut data =
        build_segment_data_slice_impl(&task.documents, task.doc_base, kind, collect_q2, timings)?;
    let base_elapsed = base_started.elapsed();
    let base_ms = base_elapsed.as_secs_f64() * 1000.0;
    if let Some(timings) = timings {
        add_duration(&timings.segment_core_ns, base_elapsed);
    }
    let file = format!("seg-{:05}.prseg", task.segment_index);
    let path = output_dir.join(&file);
    let base_write_started = Instant::now();
    let written = write_segment(&path, &data, durable)?;
    let base_write_elapsed = base_write_started.elapsed();
    let base_write_ms = base_write_elapsed.as_secs_f64() * 1000.0;
    if let Some(timings) = timings {
        add_duration(&timings.segment_write_ns, base_write_elapsed);
    }
    let accel_started = Instant::now();
    if acceleration != AccelerationProfile::None {
        let report = build_accelerators_from_memory(MemoryAccelerationRequest {
            segment_path: &path,
            unit_text_off: &data.unit_text_off,
            text_blob: &data.texts,
            q3_directory: &data.content.q3dir,
            segment_checksum: written.checksum,
            q2_pairs: data.q2_pairs.take(),
            profile: acceleration,
            build_workers,
            durable,
        })?;
        if std::env::var_os("PR_PROFILE_BUILD").is_some() {
            eprintln!(
                "BUILD_ACCEL segment={} q2_bytes={} pos1_bytes={} pos2_bytes={} pos3_bytes={}",
                task.segment_index,
                report.q2_bytes,
                report.pos1_bytes,
                report.pos2_bytes,
                report.pos3_bytes,
            );
        }
    }
    let accel_elapsed = if acceleration == AccelerationProfile::None {
        Duration::ZERO
    } else {
        accel_started.elapsed()
    };
    let accel_ms = accel_elapsed.as_secs_f64() * 1000.0;
    if let Some(timings) = timings {
        add_duration(&timings.acceleration_ns, accel_elapsed);
    }
    if profile_build {
        eprintln!(
            "BUILD_SEGMENT_WALL segment={} docs={} sample_ms={:.3} base_ms={:.3} base_write_ms={:.3} accel_ms={:.3} total_ms={:.3}",
            task.segment_index,
            count,
            sample_ms,
            base_ms,
            base_write_ms,
            accel_ms,
            total_started.elapsed().as_secs_f64() * 1000.0,
        );
    }
    let entry = ManifestEntry {
        file,
        base: u32::try_from(task.doc_base)
            .map_err(|_| SearchError::Format("doc base overflow".into()))?,
        count: u32::try_from(count)
            .map_err(|_| SearchError::Format("segment doc count overflow".into()))?,
        kind,
        sample,
        bytes: written.bytes,
    };
    let retained = retain_documents.then_some(task.documents);
    Ok((entry, retained))
}

pub fn build_disk_index_pipelined(
    root: impl AsRef<Path>,
    max_docs: Option<usize>,
    max_file_bytes: u64,
    output_dir: impl AsRef<Path>,
    options: &BuildOptions,
    scan_workers: usize,
) -> Result<BuildReport> {
    build_disk_index_pipelined_impl(
        root.as_ref(),
        max_docs,
        max_file_bytes,
        output_dir.as_ref(),
        options,
        scan_workers,
        true,
    )
}

pub fn build_disk_index_pipelined_benchmark(
    root: impl AsRef<Path>,
    max_docs: Option<usize>,
    max_file_bytes: u64,
    output_dir: impl AsRef<Path>,
    options: &BuildOptions,
    scan_workers: usize,
) -> Result<BuildReport> {
    build_disk_index_pipelined_impl(
        root.as_ref(),
        max_docs,
        max_file_bytes,
        output_dir.as_ref(),
        options,
        scan_workers,
        false,
    )
}

/// Builds an index from an explicit, already-selected file list without materializing the whole
/// corpus as `DocumentInput` values at once.
///
/// This is intended for application adapters that own filesystem traversal, exclusions and
/// progress reporting. `files` must contain paths below `root` and should already be in the
/// desired deterministic document order. The builder hydrates only bounded batches, builds
/// immutable segments in parallel and returns the exact display-path order assigned to document
/// IDs so the caller can publish its own catalog beside the portable index.
pub fn build_disk_paths_index_pipelined<F>(
    root: impl AsRef<Path>,
    files: Vec<PathBuf>,
    output_dir: impl AsRef<Path>,
    config: DiskPathBuildConfig<'_>,
    on_progress: F,
) -> Result<DiskPathBuildReport>
where
    F: FnMut(&DiskPathBuildProgress),
{
    build_disk_paths_index_pipelined_profile_impl(
        root.as_ref(),
        files,
        DiskPipelineProfileConfig {
            max_docs: config.max_docs,
            max_file_bytes: config.max_file_bytes,
            output_dir: output_dir.as_ref(),
            options: config.build,
            scan_workers: config.scan_workers,
            hydration_batch_bytes: config.hydration_batch_bytes,
            durable: true,
            cancel: config.cancel,
            acceleration: AccelerationProfile::None,
            retain_documents: false,
        },
        on_progress,
        |_| Ok(()),
    )
    .map(|result| result.report)
}

/// Builds from scanner-selected files while reusing scan-time display-path and size metadata.
///
/// The output is byte-compatible with `build_disk_paths_index_pipelined` for equivalent inputs,
/// but avoids redundant filesystem metadata/canonicalization work during content hydration.
pub fn build_disk_path_inputs_index_pipelined<F>(
    root: impl AsRef<Path>,
    files: Vec<DiskPathInput>,
    output_dir: impl AsRef<Path>,
    config: DiskPathBuildConfig<'_>,
    on_progress: F,
) -> Result<DiskPathBuildReport>
where
    F: FnMut(&DiskPathBuildProgress),
{
    build_disk_paths_index_pipelined_profile_impl(
        root.as_ref(),
        files,
        DiskPipelineProfileConfig {
            max_docs: config.max_docs,
            max_file_bytes: config.max_file_bytes,
            output_dir: output_dir.as_ref(),
            options: config.build,
            scan_workers: config.scan_workers,
            hydration_batch_bytes: config.hydration_batch_bytes,
            durable: true,
            cancel: config.cancel,
            acceleration: AccelerationProfile::None,
            retain_documents: false,
        },
        on_progress,
        |_| Ok(()),
    )
    .map(|result| result.report)
}

/// Unified production builder: constructs the exact base segment and selected accelerators while
/// the normalized SegmentData is still resident, avoiding sidecar reopen/mmap passes.
pub fn build_disk_path_inputs_index_unified<F>(
    root: impl AsRef<Path>,
    files: Vec<DiskPathInput>,
    output_dir: impl AsRef<Path>,
    config: DiskPathBuildConfig<'_>,
    acceleration: AccelerationProfile,
    on_progress: F,
) -> Result<DiskPathBuildReport>
where
    F: FnMut(&DiskPathBuildProgress),
{
    build_disk_path_inputs_index_unified_observed(
        root,
        files,
        output_dir,
        config,
        acceleration,
        on_progress,
        |_| Ok(()),
    )
}

/// Unified production builder with a document observer invoked exactly once for every hydrated
/// document that is accepted into deterministic document-ID order. The observer runs before the
/// document is moved into the Perf12 segment builder, allowing application adapters to tee the
/// already-normalized bytes into another bounded pipeline without rereading source files.
pub fn build_disk_path_inputs_index_unified_observed<F, O>(
    root: impl AsRef<Path>,
    files: Vec<DiskPathInput>,
    output_dir: impl AsRef<Path>,
    config: DiskPathBuildConfig<'_>,
    acceleration: AccelerationProfile,
    on_progress: F,
    on_document: O,
) -> Result<DiskPathBuildReport>
where
    F: FnMut(&DiskPathBuildProgress),
    O: FnMut(&DocumentInput) -> Result<()>,
{
    build_disk_paths_index_pipelined_profile_impl(
        root.as_ref(),
        files,
        DiskPipelineProfileConfig {
            max_docs: config.max_docs,
            max_file_bytes: config.max_file_bytes,
            output_dir: output_dir.as_ref(),
            options: config.build,
            scan_workers: config.scan_workers,
            hydration_batch_bytes: config.hydration_batch_bytes,
            durable: true,
            cancel: config.cancel,
            acceleration,
            retain_documents: false,
        },
        on_progress,
        on_document,
    )
    .map(|result| result.report)
}

/// Unified production builder that retains ownership of hydrated documents after their Perf12
/// segments have finished building. This avoids rereading or cloning normalized content when an
/// application immediately builds a second index from the exact same corpus. Callers should only
/// use this path when the full retained corpus fits inside their explicit memory budget.
pub fn build_disk_path_inputs_index_unified_retained<F>(
    root: impl AsRef<Path>,
    files: Vec<DiskPathInput>,
    output_dir: impl AsRef<Path>,
    config: DiskPathBuildConfig<'_>,
    acceleration: AccelerationProfile,
    on_progress: F,
) -> Result<(DiskPathBuildReport, Vec<DocumentInput>)>
where
    F: FnMut(&DiskPathBuildProgress),
{
    let result = build_disk_paths_index_pipelined_profile_impl(
        root.as_ref(),
        files,
        DiskPipelineProfileConfig {
            max_docs: config.max_docs,
            max_file_bytes: config.max_file_bytes,
            output_dir: output_dir.as_ref(),
            options: config.build,
            scan_workers: config.scan_workers,
            hydration_batch_bytes: config.hydration_batch_bytes,
            durable: true,
            cancel: config.cancel,
            acceleration,
            retain_documents: true,
        },
        on_progress,
        |_| Ok(()),
    )?;
    Ok((result.report, result.retained_documents))
}

fn build_disk_index_pipelined_impl(
    root: &Path,
    max_docs: Option<usize>,
    max_file_bytes: u64,
    output_dir: &Path,
    options: &BuildOptions,
    scan_workers: usize,
    durable: bool,
) -> Result<BuildReport> {
    if !root.is_dir() {
        return Err(SearchError::InvalidArgument(format!(
            "disk corpus root is not a directory: {}",
            root.display()
        )));
    }
    let mut files = Vec::new();
    collect_files(root, &mut files)?;
    files.sort_by_key(|path| path_to_portable(path));
    Ok(build_disk_paths_index_pipelined_profile_impl(
        root,
        files,
        DiskPipelineProfileConfig {
            max_docs,
            max_file_bytes,
            output_dir,
            options,
            scan_workers,
            hydration_batch_bytes: 0,
            durable,
            cancel: None,
            acceleration: AccelerationProfile::None,
            retain_documents: false,
        },
        |_| {},
        |_| Ok(()),
    )?
    .report
    .build)
}

struct DiskPipelineProfileConfig<'a> {
    max_docs: Option<usize>,
    max_file_bytes: u64,
    output_dir: &'a Path,
    options: &'a BuildOptions,
    scan_workers: usize,
    hydration_batch_bytes: u64,
    durable: bool,
    cancel: Option<&'a AtomicBool>,
    acceleration: AccelerationProfile,
    retain_documents: bool,
}

fn build_disk_paths_index_pipelined_profile_impl<T, F, O>(
    root: &Path,
    files: Vec<T>,
    config: DiskPipelineProfileConfig<'_>,
    mut on_progress: F,
    mut on_document: O,
) -> Result<DiskPipelineResult>
where
    T: DiskPathSource,
    F: FnMut(&DiskPathBuildProgress),
    O: FnMut(&DocumentInput) -> Result<()>,
{
    let DiskPipelineProfileConfig {
        max_docs,
        max_file_bytes,
        output_dir,
        options,
        scan_workers,
        hydration_batch_bytes,
        durable,
        cancel,
        acceleration,
        retain_documents,
    } = config;
    if !root.is_dir() {
        return Err(SearchError::InvalidArgument(format!(
            "disk corpus root is not a directory: {}",
            root.display()
        )));
    }
    if options.segment_docs == 0 {
        return Err(SearchError::InvalidArgument(
            "segment_docs must be > 0".into(),
        ));
    }
    if output_dir.exists() {
        fs::remove_dir_all(output_dir)?;
    }
    fs::create_dir_all(output_dir)?;
    let started = Instant::now();
    let profile_build = profile_build_enabled();
    if profile_build {
        PROFILE_HYDRATION_READ_NS.store(0, Ordering::Relaxed);
        PROFILE_HYDRATION_NORMALIZE_NS.store(0, Ordering::Relaxed);
    }
    let timings = Arc::new(BuildTimingAccumulator::default());
    let mut hydration_wall_ns = 0u64;
    let canonical_root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let scan_workers = scan_workers.max(1);
    let build_workers = options.workers.max(1);
    let batch_paths = options.segment_docs.saturating_mul(2).max(1_024);
    let source_files = files.len();
    let limit = max_docs.unwrap_or(usize::MAX);
    let mut progress = DiskPathBuildProgress {
        source_files,
        ..DiskPathBuildProgress::default()
    };
    on_progress(&progress);

    let (entries, total_docs, display_paths, source_indices, retained_documents) =
        std::thread::scope(|scope| -> Result<PipelineBuildOutput> {
            let (ready_tx, ready_rx) = mpsc::channel::<usize>();
            let (result_tx, result_rx) =
                mpsc::channel::<(usize, Result<(ManifestEntry, Option<Vec<DocumentInput>>)>)>();
            let mut task_senders = Vec::with_capacity(build_workers);
            for worker_id in 0..build_workers {
                let (task_tx, task_rx) = mpsc::sync_channel::<SegmentBuildTask>(1);
                task_senders.push(task_tx);
                let ready_tx = ready_tx.clone();
                let result_tx = result_tx.clone();
                let timings = Arc::clone(&timings);
                scope.spawn(move || {
                    loop {
                        if ready_tx.send(worker_id).is_err() {
                            return;
                        }
                        let Ok(task) = task_rx.recv() else {
                            return;
                        };
                        let segment_index = task.segment_index;
                        let result = build_owned_segment_profile(
                            task,
                            OwnedSegmentBuildConfig {
                                output_dir,
                                mode: options.mode,
                                acceleration,
                                build_workers,
                                durable,
                                retain_documents,
                                timings: Some(timings.as_ref()),
                            },
                        );
                        if result_tx.send((segment_index, result)).is_err() {
                            return;
                        }
                    }
                });
            }
            drop(ready_tx);
            drop(result_tx);

            let mut pending_docs = Vec::with_capacity(options.segment_docs);
            let mut display_paths = Vec::with_capacity(source_files.min(limit));
            let mut source_indices = Vec::with_capacity(source_files.min(limit));
            let mut total_docs = 0usize;
            let mut next_segment = 0usize;
            let mut batch_start = 0usize;
            'batches: while batch_start < files.len() {
                if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                    return Err(SearchError::InvalidArgument("build cancelled".into()));
                }
                let batch_end = next_hydration_batch_end(
                    &files,
                    batch_start,
                    batch_paths,
                    hydration_batch_bytes,
                );
                let paths = &files[batch_start..batch_end];
                let batch_source_base = progress.processed_files;
                let batch_bytes_base = progress.bytes_read;
                let hydration_started = Instant::now();
                let hydrated = hydrate_disk_paths_parallel_observed(
                    root,
                    &canonical_root,
                    paths,
                    max_file_bytes,
                    scan_workers,
                    cancel,
                    |hydration| {
                        progress.processed_files =
                            batch_source_base.saturating_add(hydration.completed_files);
                        progress.bytes_read = batch_bytes_base.saturating_add(hydration.bytes_read);
                        progress.prepared_bytes = hydration.bytes_read;
                        progress.current_path = paths
                            .get(hydration.current_index)
                            .map(|source| source.path().to_path_buf());
                        on_progress(&progress);
                    },
                )?;
                hydration_wall_ns = hydration_wall_ns.saturating_add(
                    hydration_started
                        .elapsed()
                        .as_nanos()
                        .min(u128::from(u64::MAX)) as u64,
                );
                progress.processed_files = batch_source_base.saturating_add(paths.len());
                let mut batch_indexed = 0usize;
                for (batch_offset, document) in hydrated.into_iter().enumerate() {
                    let Some(document) = document else {
                        continue;
                    };
                    if total_docs >= limit {
                        break 'batches;
                    }
                    on_document(&document)?;
                    display_paths.push(document.display_path.clone());
                    source_indices.push(
                        u32::try_from(batch_source_base.saturating_add(batch_offset)).map_err(
                            |_| SearchError::InvalidArgument("source file index overflow".into()),
                        )?,
                    );
                    pending_docs.push(document);
                    batch_indexed += 1;
                    total_docs += 1;
                    if total_docs > u32::MAX as usize {
                        return Err(SearchError::InvalidArgument(
                            "document count exceeds u32 id space".into(),
                        ));
                    }
                    if pending_docs.len() == options.segment_docs {
                        if cancel.is_some_and(|flag| flag.load(Ordering::Acquire)) {
                            return Err(SearchError::InvalidArgument("build cancelled".into()));
                        }
                        let worker = ready_rx.recv().map_err(|_| {
                            SearchError::Format("pipeline builder readiness channel closed".into())
                        })?;
                        let doc_base = total_docs - pending_docs.len();
                        task_senders[worker]
                            .send(SegmentBuildTask {
                                documents: core::mem::take(&mut pending_docs),
                                doc_base,
                                segment_index: next_segment,
                            })
                            .map_err(|_| {
                                SearchError::Format("pipeline builder task channel closed".into())
                            })?;
                        pending_docs = Vec::with_capacity(options.segment_docs);
                        next_segment += 1;
                    }
                }
                progress.indexed_files = progress.indexed_files.saturating_add(batch_indexed);
                progress.skipped_files = progress
                    .processed_files
                    .saturating_sub(progress.indexed_files);
                progress.prepared_bytes = 0;
                on_progress(&progress);
                batch_start = batch_end;
            }
            if !pending_docs.is_empty() {
                let worker = ready_rx.recv().map_err(|_| {
                    SearchError::Format("pipeline builder readiness channel closed".into())
                })?;
                let doc_base = total_docs - pending_docs.len();
                task_senders[worker]
                    .send(SegmentBuildTask {
                        documents: pending_docs,
                        doc_base,
                        segment_index: next_segment,
                    })
                    .map_err(|_| {
                        SearchError::Format("pipeline builder task channel closed".into())
                    })?;
                next_segment += 1;
            }
            drop(task_senders);

            let mut entries = std::iter::repeat_with(|| None)
                .take(next_segment)
                .collect::<Vec<Option<ManifestEntry>>>();
            let mut retained_segments = std::iter::repeat_with(|| None)
                .take(next_segment)
                .collect::<Vec<Option<Vec<DocumentInput>>>>();
            for _ in 0..next_segment {
                let (segment_index, result) = result_rx.recv().map_err(|_| {
                    SearchError::Format("pipeline builder result channel closed".into())
                })?;
                let (entry, retained) = result?;
                let slot = entries.get_mut(segment_index).ok_or_else(|| {
                    SearchError::Format("pipeline segment index out of bounds".into())
                })?;
                *slot = Some(entry);
                if retain_documents {
                    let retained = retained.ok_or_else(|| {
                        SearchError::Format("pipeline retained document segment is missing".into())
                    })?;
                    retained_segments[segment_index] = Some(retained);
                }
            }
            let entries = entries
                .into_iter()
                .enumerate()
                .map(|(index, entry)| {
                    entry.ok_or_else(|| {
                        SearchError::Format(format!("pipeline segment {index} was not built"))
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let retained_documents = if retain_documents {
                let mut documents = Vec::with_capacity(total_docs);
                for (index, segment) in retained_segments.into_iter().enumerate() {
                    let segment = segment.ok_or_else(|| {
                        SearchError::Format(format!(
                            "pipeline retained segment {index} was not built"
                        ))
                    })?;
                    documents.extend(segment);
                }
                documents
            } else {
                Vec::new()
            };
            Ok((
                entries,
                total_docs,
                display_paths,
                source_indices,
                retained_documents,
            ))
        })?;

    let manifest_write_started = Instant::now();
    write_manifest(output_dir, options.mode, total_docs, &entries, durable)?;
    let manifest_write_wall = manifest_write_started.elapsed();
    let index_bytes = entries.iter().map(|entry| entry.bytes).sum::<u64>();
    let build = BuildReport {
        docs: total_docs,
        segments: entries.len(),
        index_bytes,
        elapsed: started.elapsed(),
        output_dir: output_dir.to_path_buf(),
    };
    progress.processed_files = progress.processed_files.min(source_files);
    progress.indexed_files = total_docs;
    progress.skipped_files = progress.processed_files.saturating_sub(total_docs);
    progress.prepared_bytes = 0;
    progress.current_path = None;
    on_progress(&progress);
    if profile_build {
        eprintln!(
            "BUILD_HYDRATION source_files={} docs={} read_ms={:.3} normalize_ms={:.3} wall_ms={:.3}",
            source_files,
            total_docs,
            PROFILE_HYDRATION_READ_NS.load(Ordering::Relaxed) as f64 / 1_000_000.0,
            PROFILE_HYDRATION_NORMALIZE_NS.load(Ordering::Relaxed) as f64 / 1_000_000.0,
            hydration_wall_ns as f64 / 1_000_000.0,
        );
    }
    Ok(DiskPipelineResult {
        report: DiskPathBuildReport {
            build,
            display_paths,
            source_indices,
            source_files,
            processed_files: progress.processed_files,
            skipped_files: progress.skipped_files,
            bytes_read: progress.bytes_read,
            timings: timings.snapshot(hydration_wall_ns, manifest_write_wall),
        },
        retained_documents,
    })
}

fn collect_files(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let file_type = match entry.file_type() {
            Ok(value) => value,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            collect_files(&entry.path(), out)?;
        } else if fs::metadata(entry.path()).is_ok_and(|metadata| metadata.is_file()) {
            // Match std::filesystem::is_regular_file used by the C++ oracle:
            // follow symlinks to regular files, but do not recurse into symlinked directories.
            out.push(entry.path());
        }
    }
    Ok(())
}

fn relative_path(path: &Path, base: &Path) -> Option<PathBuf> {
    let path_components = path.components().collect::<Vec<_>>();
    let base_components = base.components().collect::<Vec<_>>();
    let common = path_components
        .iter()
        .zip(&base_components)
        .take_while(|(left, right)| left == right)
        .count();
    if common == 0 {
        return None;
    }
    let mut out = PathBuf::new();
    for component in &base_components[common..] {
        if matches!(component, std::path::Component::Normal(_)) {
            out.push("..");
        }
    }
    for component in &path_components[common..] {
        out.push(component.as_os_str());
    }
    Some(out)
}

fn path_to_portable(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

pub(crate) struct UnifiedIndexAssembler<'a> {
    output_dir: PathBuf,
    options: &'a BuildOptions,
    acceleration: AccelerationProfile,
    durable: bool,
    pending: Vec<DocumentInput>,
    entries: Vec<ManifestEntry>,
    total_docs: usize,
    started: Instant,
}

impl<'a> UnifiedIndexAssembler<'a> {
    pub(crate) fn new(
        output_dir: impl AsRef<Path>,
        options: &'a BuildOptions,
        acceleration: AccelerationProfile,
        durable: bool,
    ) -> Result<Self> {
        if options.segment_docs == 0 {
            return Err(SearchError::InvalidArgument(
                "segment_docs must be > 0".into(),
            ));
        }
        let output_dir = output_dir.as_ref().to_path_buf();
        if output_dir.exists() {
            fs::remove_dir_all(&output_dir)?;
        }
        fs::create_dir_all(&output_dir)?;
        Ok(Self {
            output_dir,
            options,
            acceleration,
            durable,
            pending: Vec::with_capacity(options.segment_docs),
            entries: Vec::new(),
            total_docs: 0,
            started: Instant::now(),
        })
    }

    pub(crate) fn push(&mut self, document: DocumentInput) -> Result<()> {
        if self.total_docs >= u32::MAX as usize {
            return Err(SearchError::InvalidArgument(
                "document count exceeds u32 id space".into(),
            ));
        }
        self.pending.push(document);
        self.total_docs += 1;
        if self.pending.len() == self.options.segment_docs {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let segment_index = self.entries.len();
        let count = self.pending.len();
        let doc_base = self.total_docs - count;
        let task = SegmentBuildTask {
            documents: core::mem::replace(
                &mut self.pending,
                Vec::with_capacity(self.options.segment_docs),
            ),
            doc_base,
            segment_index,
        };
        let (entry, _) = build_owned_segment_profile(
            task,
            OwnedSegmentBuildConfig {
                output_dir: &self.output_dir,
                mode: self.options.mode,
                acceleration: self.acceleration,
                build_workers: 1,
                durable: self.durable,
                retain_documents: false,
                timings: None,
            },
        )?;
        self.entries.push(entry);
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<BuildReport> {
        self.flush()?;
        write_manifest(
            &self.output_dir,
            self.options.mode,
            self.total_docs,
            &self.entries,
            self.durable,
        )?;
        let index_bytes = self.entries.iter().map(|entry| entry.bytes).sum();
        Ok(BuildReport {
            docs: self.total_docs,
            segments: self.entries.len(),
            index_bytes,
            elapsed: self.started.elapsed(),
            output_dir: self.output_dir,
        })
    }
}

pub fn build_index(
    documents: &[DocumentInput],
    output_dir: impl AsRef<Path>,
    options: &BuildOptions,
) -> Result<BuildReport> {
    build_index_impl(documents, output_dir.as_ref(), options, true)
}

/// Benchmark-only builder that preserves the exact on-disk format but skips fsync/directory-sync.
/// Never use this path to publish a production generation.
pub fn build_index_benchmark(
    documents: &[DocumentInput],
    output_dir: impl AsRef<Path>,
    options: &BuildOptions,
) -> Result<BuildReport> {
    build_index_impl(documents, output_dir.as_ref(), options, false)
}

pub fn build_index_unified(
    documents: &[DocumentInput],
    output_dir: impl AsRef<Path>,
    options: &BuildOptions,
    acceleration: AccelerationProfile,
) -> Result<BuildReport> {
    build_index_profile_impl(documents, output_dir.as_ref(), options, acceleration, true)
}

pub fn build_index_unified_benchmark(
    documents: &[DocumentInput],
    output_dir: impl AsRef<Path>,
    options: &BuildOptions,
    acceleration: AccelerationProfile,
) -> Result<BuildReport> {
    build_index_profile_impl(documents, output_dir.as_ref(), options, acceleration, false)
}

fn build_index_impl(
    documents: &[DocumentInput],
    output_dir: &Path,
    options: &BuildOptions,
    durable: bool,
) -> Result<BuildReport> {
    build_index_profile_impl(
        documents,
        output_dir,
        options,
        AccelerationProfile::None,
        durable,
    )
}

fn build_index_profile_impl(
    documents: &[DocumentInput],
    output_dir: &Path,
    options: &BuildOptions,
    acceleration: AccelerationProfile,
    durable: bool,
) -> Result<BuildReport> {
    if options.segment_docs == 0 {
        return Err(SearchError::InvalidArgument(
            "segment_docs must be > 0".into(),
        ));
    }
    if documents.len() > u32::MAX as usize {
        return Err(SearchError::InvalidArgument(
            "document count exceeds u32 id space".into(),
        ));
    }
    let output_dir = output_dir.to_path_buf();
    if output_dir.exists() {
        fs::remove_dir_all(&output_dir)?;
    }
    fs::create_dir_all(&output_dir)?;
    let started = Instant::now();
    let segment_count = documents.len().div_ceil(options.segment_docs);
    let entries: Arc<Mutex<Vec<Option<ManifestEntry>>>> =
        Arc::new(Mutex::new(vec![None; segment_count]));
    let next = AtomicUsize::new(0);
    let first_error: Arc<Mutex<Option<SearchError>>> = Arc::new(Mutex::new(None));
    let worker_count = options.workers.max(1).min(segment_count.max(1));

    std::thread::scope(|scope| {
        for _ in 0..worker_count {
            let entries = Arc::clone(&entries);
            let first_error = Arc::clone(&first_error);
            let output_dir = &output_dir;
            let next = &next;
            scope.spawn(move || {
                loop {
                    if first_error.lock().expect("error mutex poisoned").is_some() {
                        break;
                    }
                    let segment_index = next.fetch_add(1, Ordering::Relaxed);
                    if segment_index >= segment_count {
                        break;
                    }
                    let begin = segment_index * options.segment_docs;
                    let end = (begin + options.segment_docs).min(documents.len());
                    let result = build_one_segment_profile(
                        SliceSegmentTask {
                            documents,
                            begin,
                            end,
                            segment_index,
                        },
                        output_dir,
                        options.mode,
                        acceleration,
                        options.workers,
                        durable,
                    );
                    match result {
                        Ok(entry) => {
                            entries.lock().expect("entry mutex poisoned")[segment_index] =
                                Some(entry);
                        }
                        Err(error) => {
                            *first_error.lock().expect("error mutex poisoned") = Some(error);
                            break;
                        }
                    }
                }
            });
        }
    });
    if let Some(error) = first_error.lock().expect("error mutex poisoned").take() {
        return Err(error);
    }
    let entries = Arc::try_unwrap(entries)
        .map_err(|_| SearchError::Format("segment entry ownership leak".into()))?
        .into_inner()
        .map_err(|_| SearchError::Format("segment entry mutex poisoned".into()))?
        .into_iter()
        .enumerate()
        .map(|(index, entry)| {
            entry.ok_or_else(|| SearchError::Format(format!("segment {index} was not built")))
        })
        .collect::<Result<Vec<_>>>()?;
    write_manifest(
        &output_dir,
        options.mode,
        documents.len(),
        &entries,
        durable,
    )?;
    let index_bytes = entries.iter().map(|entry| entry.bytes).sum::<u64>();
    Ok(BuildReport {
        docs: documents.len(),
        segments: entries.len(),
        index_bytes,
        elapsed: started.elapsed(),
        output_dir,
    })
}

struct SliceSegmentTask<'a> {
    documents: &'a [DocumentInput],
    begin: usize,
    end: usize,
    segment_index: usize,
}

fn build_one_segment_profile(
    task: SliceSegmentTask<'_>,
    output_dir: &Path,
    mode: BuildMode,
    acceleration: AccelerationProfile,
    build_workers: usize,
    durable: bool,
) -> Result<ManifestEntry> {
    let SliceSegmentTask {
        documents,
        begin,
        end,
        segment_index,
    } = task;
    let sample = sample_stats(&documents[begin..end]);
    let kind = match mode {
        BuildMode::Direct => BuilderKind::Direct,
        BuildMode::Dedup => BuilderKind::Dedup,
        BuildMode::Adaptive if sample.duplicate_ratio >= 0.20 => BuilderKind::Dedup,
        BuildMode::Adaptive => BuilderKind::Direct,
    };
    let segment_docs = &documents[begin..end];
    let mut data = build_segment_data_slice_impl(
        segment_docs,
        begin,
        kind,
        should_collect_q2_seed(acceleration, segment_docs),
        None,
    )?;
    let file = format!("seg-{segment_index:05}.prseg");
    let path = output_dir.join(&file);
    let written = write_segment(&path, &data, durable)?;
    if acceleration != AccelerationProfile::None {
        build_accelerators_from_memory(MemoryAccelerationRequest {
            segment_path: &path,
            unit_text_off: &data.unit_text_off,
            text_blob: &data.texts,
            q3_directory: &data.content.q3dir,
            segment_checksum: written.checksum,
            q2_pairs: data.q2_pairs.take(),
            profile: acceleration,
            build_workers,
            durable,
        })?;
    }
    Ok(ManifestEntry {
        file,
        base: u32::try_from(begin).map_err(|_| SearchError::Format("doc base overflow".into()))?,
        count: u32::try_from(end - begin)
            .map_err(|_| SearchError::Format("segment doc count overflow".into()))?,
        kind,
        sample,
        bytes: written.bytes,
    })
}

fn is_word_byte(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric() || byte >= 0x80
}

fn sample_stats(documents: &[DocumentInput]) -> SampleStats {
    let sample = &documents[..documents.len().min(512)];
    if sample.is_empty() {
        return SampleStats {
            duplicate_ratio: 0.0,
            run_unique_ratio: 1.0,
        };
    }
    let mut contents: HashSet<&[u8]> = HashSet::with_capacity(sample.len());
    let mut runs: HashSet<&[u8]> = HashSet::new();
    let mut run_occurrences = 0usize;
    for document in sample {
        contents.insert(&document.normalized_content);
        let text = document.normalized_content.as_slice();
        if text.is_empty() {
            continue;
        }
        let mut start = 0usize;
        let mut kind = is_word_byte(text[0]);
        for index in 1..text.len() {
            let current = is_word_byte(text[index]);
            if current != kind {
                runs.insert(&text[start..index]);
                run_occurrences += 1;
                start = index;
                kind = current;
            }
        }
        runs.insert(&text[start..]);
        run_occurrences += 1;
    }
    SampleStats {
        duplicate_ratio: 1.0 - contents.len() as f64 / sample.len() as f64,
        run_unique_ratio: if run_occurrences == 0 {
            1.0
        } else {
            runs.len() as f64 / run_occurrences as f64
        },
    }
}

fn build_segment_data_slice_impl(
    docs: &[DocumentInput],
    doc_base: usize,
    kind: BuilderKind,
    collect_q2: bool,
    timings: Option<&BuildTimingAccumulator>,
) -> Result<SegmentData> {
    let doc_count = docs.len();
    let profile_build = profile_build_enabled();
    let total_started = Instant::now();
    let phase_started = Instant::now();
    let mut name_off = Vec::with_capacity(doc_count + 1);
    let total_name_bytes = docs.iter().map(|doc| doc.normalized_name.len()).sum();
    let mut names = Vec::with_capacity(total_name_bytes);
    let use_streaming_name_postings = doc_count <= u16::MAX as usize;
    let mut name_posting_emitter =
        use_streaming_name_postings.then(|| NamePostingEmitter::with_capacity(total_name_bytes));
    let mut name_grams = (!use_streaming_name_postings).then(|| Vec::with_capacity(doc_count));
    name_off.push(0);
    for (document_id, doc) in docs.iter().enumerate() {
        names.extend_from_slice(&doc.normalized_name);
        name_off.push(names.len() as u64);
        if let Some(emitter) = name_posting_emitter.as_mut() {
            emitter.emit(document_id, &doc.normalized_name)?;
        } else if let Some(grams) = name_grams.as_mut() {
            grams.push(direct_grams(&doc.normalized_name, true));
        }
    }
    let name_gramselapsed = phase_started.elapsed();
    let name_grams_ms = name_gramselapsed.as_secs_f64() * 1000.0;
    if let Some(timings) = timings {
        add_duration(&timings.name_grams_ns, name_gramselapsed);
    }
    let phase_started = Instant::now();

    let mut doc_unit = vec![0u32; doc_count];
    let mut unit_sources = Vec::<usize>::new();
    let mut unit_docs = Vec::<Vec<u32>>::new();
    if kind == BuilderKind::Dedup {
        let mut seen: HashMap<&[u8], u32> = HashMap::with_capacity(doc_count * 2);
        for (doc_index, doc) in docs.iter().enumerate() {
            let unit = if let Some(&unit) = seen.get(doc.normalized_content.as_slice()) {
                unit
            } else {
                let unit = u32::try_from(unit_sources.len())
                    .map_err(|_| SearchError::Format("unit id overflow".into()))?;
                seen.insert(doc.normalized_content.as_slice(), unit);
                unit_sources.push(doc_index);
                unit_docs.push(Vec::new());
                unit
            };
            unit_docs[unit as usize].push(
                u32::try_from(doc_index)
                    .map_err(|_| SearchError::Format("document id overflow".into()))?,
            );
            doc_unit[doc_index] = unit;
        }
    } else {
        for (doc_index, doc_unit_slot) in doc_unit.iter_mut().enumerate() {
            let unit = u32::try_from(doc_index)
                .map_err(|_| SearchError::Format("unit id overflow".into()))?;
            unit_sources.push(doc_index);
            unit_docs.push(vec![unit]);
            *doc_unit_slot = unit;
        }
    }
    let dedupelapsed = phase_started.elapsed();
    let dedup_ms = dedupelapsed.as_secs_f64() * 1000.0;
    if let Some(timings) = timings {
        add_duration(&timings.dedup_ns, dedupelapsed);
    }
    let phase_started = Instant::now();

    let mut unit_text_off = Vec::with_capacity(unit_sources.len() + 1);
    let mut texts = Vec::new();
    let mut unit_doc_off = Vec::with_capacity(unit_sources.len() + 1);
    let mut unit_docs_flat = Vec::new();
    let mut content_q1mask = vec![0u8; unit_sources.len() * 32];
    let use_packed_shards = unit_sources.len() <= u16::MAX as usize;
    let mut content_q3_shards = (0..256).map(|_| Vec::<u32>::new()).collect::<Vec<_>>();
    let mut content_q3_pairs = Vec::<u64>::new();
    unit_text_off.push(0);
    unit_doc_off.push(0);
    // 2^24 q3 keys -> 2 MiB bitset. Reuse it for every ContentUnit and clear only touched
    // words, eliminating the per-unit O(n log n) q3 sort/dedup.
    let mut q3_seen = vec![0u64; 1usize << 18];
    let mut q3_touched_words = Vec::<usize>::new();
    let mut q2_pairs =
        (collect_q2 && unit_sources.len() <= u16::MAX as usize).then(Vec::<u32>::new);
    let mut q2_seen = collect_q2.then(|| vec![0u64; 1usize << 10]);
    let mut q2_touched_words = Vec::<usize>::new();
    for (unit_index, &source) in unit_sources.iter().enumerate() {
        let content = &docs[source].normalized_content;
        texts.extend_from_slice(content);
        unit_text_off.push(texts.len() as u64);
        unit_docs_flat.extend_from_slice(&unit_docs[unit_index]);
        unit_doc_off.push(unit_docs_flat.len() as u64);
        let unit_id = u32::try_from(unit_index)
            .map_err(|_| SearchError::Format("content id overflow".into()))?;
        let mask_base = unit_index * 32;
        q3_touched_words.clear();
        q2_touched_words.clear();
        let mut previous2 = 0u8;
        let mut previous1 = 0u8;
        for (index, &byte) in content.iter().enumerate() {
            content_q1mask[mask_base + usize::from(byte / 8)] |= 1u8 << (byte % 8);
            if index >= 1
                && let (Some(seen), Some(pairs)) = (q2_seen.as_mut(), q2_pairs.as_mut())
            {
                let key = k2(previous1, byte);
                let word_index = usize::from(key >> 6);
                let bit = 1u64 << (key & 63);
                let word = seen[word_index];
                if word & bit == 0 {
                    if word == 0 {
                        q2_touched_words.push(word_index);
                    }
                    seen[word_index] = word | bit;
                    pairs.push((u32::from(key) << 16) | unit_id);
                }
            }
            if index >= 2 {
                let key = k3(previous2, previous1, byte);
                let word_index = (key >> 6) as usize;
                let bit = 1u64 << (key & 63);
                let word = q3_seen[word_index];
                if word & bit == 0 {
                    if word == 0 {
                        q3_touched_words.push(word_index);
                    }
                    q3_seen[word_index] = word | bit;
                    if use_packed_shards {
                        let high = (key >> 16) as usize;
                        let packed = ((key & 0xffff) << 16) | unit_id;
                        content_q3_shards[high].push(packed);
                    } else {
                        content_q3_pairs.push((u64::from(key) << 32) | u64::from(unit_id));
                    }
                }
            }
            previous2 = previous1;
            previous1 = byte;
        }
        for &word_index in &q3_touched_words {
            q3_seen[word_index] = 0;
        }
        if let Some(seen) = q2_seen.as_mut() {
            for &word_index in &q2_touched_words {
                seen[word_index] = 0;
            }
        }
    }
    let content_gramselapsed = phase_started.elapsed();
    let content_grams_ms = content_gramselapsed.as_secs_f64() * 1000.0;
    if let Some(timings) = timings {
        add_duration(&timings.content_grams_ns, content_gramselapsed);
    }
    let phase_started = Instant::now();
    let content = if use_packed_shards {
        build_content_postings_from_packed_shards(
            unit_sources.len(),
            content_q1mask,
            content_q3_shards,
        )?
    } else {
        build_content_postings_from_pairs(unit_sources.len(), content_q1mask, content_q3_pairs)?
    };
    let content_postelapsed = phase_started.elapsed();
    let content_post_ms = content_postelapsed.as_secs_f64() * 1000.0;
    if let Some(timings) = timings {
        add_duration(&timings.content_post_ns, content_postelapsed);
    }
    let phase_started = Instant::now();
    let name_index = if let Some(emitter) = name_posting_emitter {
        emitter.finish()?
    } else {
        build_raw_postings(name_grams.unwrap_or_default())?
    };
    let name_postelapsed = phase_started.elapsed();
    let name_post_ms = name_postelapsed.as_secs_f64() * 1000.0;
    if let Some(timings) = timings {
        add_duration(&timings.name_post_ns, name_postelapsed);
    }
    if profile_build {
        eprintln!(
            "BUILD_PHASE base={} docs={} units={} name_grams_ms={:.3} dedup_ms={:.3} content_grams_ms={:.3} content_post_ms={:.3} name_post_ms={:.3} total_ms={:.3}",
            doc_base,
            doc_count,
            unit_sources.len(),
            name_grams_ms,
            dedup_ms,
            content_grams_ms,
            content_post_ms,
            name_post_ms,
            total_started.elapsed().as_secs_f64() * 1000.0,
        );
    }

    Ok(SegmentData {
        doc_base: u32::try_from(doc_base)
            .map_err(|_| SearchError::Format("doc base overflow".into()))?,
        doc_count: u32::try_from(doc_count)
            .map_err(|_| SearchError::Format("doc count overflow".into()))?,
        kind,
        name_off,
        names,
        unit_text_off,
        texts,
        unit_doc_off,
        unit_docs: unit_docs_flat,
        doc_unit,
        content,
        name_index,
        q2_pairs,
    })
}

fn direct_grams(bytes: &[u8], include_q2: bool) -> GramSet {
    let mut gram = GramSet {
        q1: Vec::with_capacity(bytes.len().min(256)),
        q2: if include_q2 {
            Vec::with_capacity(bytes.len())
        } else {
            Vec::new()
        },
        q3: Vec::with_capacity(bytes.len()),
    };
    for (index, &a) in bytes.iter().enumerate() {
        gram.q1.push(a);
        if include_q2 && index + 1 < bytes.len() {
            gram.q2.push(k2(a, bytes[index + 1]));
        }
        if index + 2 < bytes.len() {
            gram.q3.push(k3(a, bytes[index + 1], bytes[index + 2]));
        }
    }
    gram.q1.sort_unstable();
    gram.q1.dedup();
    gram.q2.sort_unstable();
    gram.q2.dedup();
    gram.q3.sort_unstable();
    gram.q3.dedup();
    gram
}

fn build_raw_postings(grams: Vec<GramSet>) -> Result<RawPostingData> {
    if grams.len() <= u16::MAX as usize {
        return build_raw_postings_radix(grams);
    }
    build_raw_postings_comparison(grams)
}

fn stable_group_u32_by_upper16(values: &[u32], scratch: &mut Vec<u32>, counts: &mut [usize]) {
    debug_assert_eq!(counts.len(), 65_536);
    scratch.clear();
    scratch.resize(values.len(), 0);
    counts.fill(0);
    for &value in values {
        counts[(value >> 16) as usize] += 1;
    }
    let mut sum = 0usize;
    for count in counts.iter_mut() {
        let current = *count;
        *count = sum;
        sum += current;
    }
    for &value in values {
        let bucket = (value >> 16) as usize;
        scratch[counts[bucket]] = value;
        counts[bucket] += 1;
    }
}

fn radix_sort_u32_16x2(values: &mut Vec<u32>) {
    if values.len() < 2 {
        return;
    }
    let mut scratch = vec![0u32; values.len()];
    let mut counts = vec![0usize; 65_536];
    for shift in [0u32, 16] {
        counts.fill(0);
        for &value in values.iter() {
            counts[((value >> shift) & 0xffff) as usize] += 1;
        }
        let mut sum = 0usize;
        for count in &mut counts {
            let current = *count;
            *count = sum;
            sum += current;
        }
        for &value in values.iter() {
            let bucket = ((value >> shift) & 0xffff) as usize;
            scratch[counts[bucket]] = value;
            counts[bucket] += 1;
        }
        core::mem::swap(values, &mut scratch);
    }
}

fn radix_sort_u64_16x3(values: &mut Vec<u64>) {
    if values.len() < 2 {
        return;
    }
    let mut scratch = vec![0u64; values.len()];
    let mut counts = vec![0usize; 65_536];
    for shift in [0u32, 16, 32] {
        counts.fill(0);
        for &value in values.iter() {
            counts[((value >> shift) & 0xffff) as usize] += 1;
        }
        let mut sum = 0usize;
        for count in &mut counts {
            let current = *count;
            *count = sum;
            sum += current;
        }
        for &value in values.iter() {
            let bucket = ((value >> shift) & 0xffff) as usize;
            scratch[counts[bucket]] = value;
            counts[bucket] += 1;
        }
        core::mem::swap(values, &mut scratch);
    }
}

fn finish_raw_q1(out: &mut RawPostingData, q1: &[Vec<u32>]) -> Result<()> {
    let mut current = 0u32;
    for (key, postings) in q1.iter().enumerate() {
        out.q1off[key] = current;
        out.q1post.extend_from_slice(postings);
        current = u32::try_from(out.q1post.len())
            .map_err(|_| SearchError::Format("q1 posting overflow".into()))?;
    }
    out.q1off[256] = current;
    Ok(())
}

fn build_raw_postings_radix(grams: Vec<GramSet>) -> Result<RawPostingData> {
    let mut out = RawPostingData {
        q1off: vec![0; 257],
        q2off: vec![0; 65_537],
        ..RawPostingData::default()
    };
    let mut q1 = (0..256).map(|_| Vec::<u32>::new()).collect::<Vec<_>>();
    let mut q2_pairs = Vec::<u32>::new();
    let mut q3_pairs = Vec::<u64>::new();
    for (id, gram) in grams.into_iter().enumerate() {
        let id = u16::try_from(id)
            .map_err(|_| SearchError::Format("radix posting id overflow".into()))?;
        let id_u32 = u32::from(id);
        for &key in &gram.q1 {
            q1[key as usize].push(id_u32);
        }
        for &key in &gram.q2 {
            q2_pairs.push((u32::from(key) << 16) | id_u32);
        }
        for &key in &gram.q3 {
            q3_pairs.push((u64::from(key) << 16) | u64::from(id));
        }
    }
    finish_raw_q1(&mut out, &q1)?;

    radix_sort_u32_16x2(&mut q2_pairs);
    let mut position = 0usize;
    for key in 0..65_536u32 {
        out.q2off[key as usize] = u32::try_from(out.q2post.len())
            .map_err(|_| SearchError::Format("q2 posting overflow".into()))?;
        while position < q2_pairs.len() && (q2_pairs[position] >> 16) == key {
            out.q2post.push(q2_pairs[position] & 0xffff);
            position += 1;
        }
    }
    out.q2off[65_536] = u32::try_from(out.q2post.len())
        .map_err(|_| SearchError::Format("q2 posting overflow".into()))?;

    radix_sort_u64_16x3(&mut q3_pairs);
    position = 0;
    while position < q3_pairs.len() {
        let key = (q3_pairs[position] >> 16) as u32;
        let start = u32::try_from(out.q3post.len())
            .map_err(|_| SearchError::Format("name q3 posting overflow".into()))?;
        while position < q3_pairs.len() && (q3_pairs[position] >> 16) as u32 == key {
            out.q3post.push((q3_pairs[position] & 0xffff) as u32);
            position += 1;
        }
        out.q3dir.push(key);
        out.q3dir.push(start);
        let end = u32::try_from(out.q3post.len())
            .map_err(|_| SearchError::Format("name q3 posting overflow".into()))?;
        out.q3dir.push(end - start);
    }
    Ok(out)
}

fn build_raw_postings_comparison(grams: Vec<GramSet>) -> Result<RawPostingData> {
    let mut out = RawPostingData {
        q1off: vec![0; 257],
        q2off: vec![0; 65_537],
        ..RawPostingData::default()
    };
    let mut q1 = (0..256).map(|_| Vec::<u32>::new()).collect::<Vec<_>>();
    let mut q2_pairs = Vec::<u64>::new();
    let mut q3_pairs = Vec::<u64>::new();
    for (id, gram) in grams.into_iter().enumerate() {
        let id =
            u32::try_from(id).map_err(|_| SearchError::Format("posting id overflow".into()))?;
        for &key in &gram.q1 {
            q1[key as usize].push(id);
        }
        for &key in &gram.q2 {
            q2_pairs.push((u64::from(key) << 32) | u64::from(id));
        }
        for &key in &gram.q3 {
            q3_pairs.push((u64::from(key) << 32) | u64::from(id));
        }
    }
    finish_raw_q1(&mut out, &q1)?;

    q2_pairs.sort_unstable();
    let mut position = 0usize;
    for key in 0..65_536u32 {
        out.q2off[key as usize] = u32::try_from(out.q2post.len())
            .map_err(|_| SearchError::Format("q2 posting overflow".into()))?;
        while position < q2_pairs.len() && ((q2_pairs[position] >> 32) as u16) == key as u16 {
            out.q2post.push(q2_pairs[position] as u32);
            position += 1;
        }
    }
    out.q2off[65_536] = u32::try_from(out.q2post.len())
        .map_err(|_| SearchError::Format("q2 posting overflow".into()))?;

    q3_pairs.sort_unstable();
    position = 0;
    while position < q3_pairs.len() {
        let key = (q3_pairs[position] >> 32) as u32;
        let start = u32::try_from(out.q3post.len())
            .map_err(|_| SearchError::Format("name q3 posting overflow".into()))?;
        while position < q3_pairs.len() && (q3_pairs[position] >> 32) as u32 == key {
            out.q3post.push(q3_pairs[position] as u32);
            position += 1;
        }
        out.q3dir.push(key);
        out.q3dir.push(start);
        let end = u32::try_from(out.q3post.len())
            .map_err(|_| SearchError::Format("name q3 posting overflow".into()))?;
        out.q3dir.push(end - start);
    }
    Ok(out)
}

fn build_content_postings_from_packed_shards(
    unit_count: usize,
    q1mask: Vec<u8>,
    mut shards: Vec<Vec<u32>>,
) -> Result<ContentPostingData> {
    if q1mask.len() != unit_count.saturating_mul(32) || shards.len() != 256 {
        return Err(SearchError::Format(
            "content packed shard shape mismatch".into(),
        ));
    }
    let universe = u32::try_from(unit_count)
        .map_err(|_| SearchError::Format("content universe overflow".into()))?;
    let mut full_dir = Vec::<u32>::new();
    let mut q3blob = Vec::<u8>::new();
    for (high, shard) in shards.iter_mut().enumerate() {
        shard.sort_unstable();
        let mut position = 0usize;
        while position < shard.len() {
            let suffix = shard[position] >> 16;
            let begin = position;
            while position < shard.len() && shard[position] >> 16 == suffix {
                position += 1;
            }
            let ids = shard[begin..position]
                .iter()
                .map(|packed| packed & 0xffff)
                .collect::<Vec<_>>();
            let (encoding, offset, bytes) = encode_q3(&ids, universe, &mut q3blob)?;
            let count = u32::try_from(ids.len())
                .map_err(|_| SearchError::Format("q3 posting count overflow".into()))?;
            if count > 0x3fff_ffff {
                return Err(SearchError::Format(
                    "q3 posting packed count overflow".into(),
                ));
            }
            let key = ((high as u32) << 16) | suffix;
            let packed = ((encoding as u32 - 1) << 30) | count;
            full_dir.extend_from_slice(&[key, packed, offset, bytes]);
        }
    }
    let q3dir = compact_q3_directory(&full_dir)?;
    Ok(ContentPostingData {
        q1mask,
        q3dir,
        q3blob,
    })
}

fn build_content_postings_from_pairs(
    unit_count: usize,
    q1mask: Vec<u8>,
    mut q3_pairs: Vec<u64>,
) -> Result<ContentPostingData> {
    if q1mask.len() != unit_count.saturating_mul(32) {
        return Err(SearchError::Format("content q1 mask size mismatch".into()));
    }
    q3_pairs.sort_unstable();
    let mut full_dir = Vec::<u32>::new();
    let mut q3blob = Vec::<u8>::new();
    let mut position = 0usize;
    let universe = u32::try_from(unit_count)
        .map_err(|_| SearchError::Format("content universe overflow".into()))?;
    while position < q3_pairs.len() {
        let key = (q3_pairs[position] >> 32) as u32;
        let begin = position;
        while position < q3_pairs.len() && (q3_pairs[position] >> 32) as u32 == key {
            position += 1;
        }
        let ids = q3_pairs[begin..position]
            .iter()
            .map(|pair| *pair as u32)
            .collect::<Vec<_>>();
        let (encoding, offset, bytes) = encode_q3(&ids, universe, &mut q3blob)?;
        let count = u32::try_from(ids.len())
            .map_err(|_| SearchError::Format("q3 posting count overflow".into()))?;
        if count > 0x3fff_ffff {
            return Err(SearchError::Format(
                "q3 posting packed count overflow".into(),
            ));
        }
        let packed = ((encoding as u32 - 1) << 30) | count;
        full_dir.extend_from_slice(&[key, packed, offset, bytes]);
    }
    let q3dir = compact_q3_directory(&full_dir)?;
    Ok(ContentPostingData {
        q1mask,
        q3dir,
        q3blob,
    })
}

fn varint_size(mut value: u32) -> usize {
    let mut size = 1;
    while value >= 0x80 {
        value >>= 7;
        size += 1;
    }
    size
}

fn append_varint(out: &mut Vec<u8>, mut value: u32) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn encode_q3(ids: &[u32], universe: u32, blob: &mut Vec<u8>) -> Result<(Q3Encoding, u32, u32)> {
    let mut delta_bytes = 0usize;
    let mut previous = 0u32;
    for (index, &id) in ids.iter().enumerate() {
        let delta = if index == 0 { id } else { id - previous };
        delta_bytes += varint_size(delta);
        previous = id;
    }
    let mut blocks = 0usize;
    let mut last_block = None;
    for &id in ids {
        let block = id / 256;
        if last_block != Some(block) {
            blocks += 1;
            last_block = Some(block);
        }
    }
    let block_bytes = blocks * 36;
    let density = if universe == 0 {
        0.0
    } else {
        ids.len() as f64 / f64::from(universe)
    };
    let encoding = if ids.len() <= 32 {
        Q3Encoding::InlineU32
    } else if density >= 0.20 {
        Q3Encoding::DenseBitset
    } else if block_bytes * 4 <= delta_bytes * 5 {
        Q3Encoding::Block256Bitmap
    } else {
        Q3Encoding::DeltaVarint
    };
    let offset = u32::try_from(blob.len())
        .map_err(|_| SearchError::Format("q3 payload exceeds 4GiB".into()))?;
    match encoding {
        Q3Encoding::InlineU32 => {
            for &id in ids {
                put_u32(blob, id);
            }
        }
        Q3Encoding::DeltaVarint => {
            let mut previous = 0u32;
            for (index, &id) in ids.iter().enumerate() {
                append_varint(blob, if index == 0 { id } else { id - previous });
                previous = id;
            }
        }
        Q3Encoding::Block256Bitmap => {
            let mut index = 0usize;
            while index < ids.len() {
                let block = ids[index] / 256;
                put_u32(blob, block);
                let mask_offset = blob.len();
                blob.resize(mask_offset + 32, 0);
                while index < ids.len() && ids[index] / 256 == block {
                    let bit = ids[index] & 255;
                    blob[mask_offset + (bit / 8) as usize] |= 1u8 << (bit % 8);
                    index += 1;
                }
            }
        }
        Q3Encoding::DenseBitset => {
            let bytes = usize::try_from(u64::from(universe).div_ceil(8))
                .map_err(|_| SearchError::Format("dense bitset too large".into()))?;
            let mask_offset = blob.len();
            blob.resize(mask_offset + bytes, 0);
            for &id in ids {
                blob[mask_offset + (id / 8) as usize] |= 1u8 << (id % 8);
            }
        }
    }
    let bytes = u32::try_from(blob.len() - offset as usize)
        .map_err(|_| SearchError::Format("q3 encoded posting too large".into()))?;
    Ok((encoding, offset, bytes))
}

fn compact_q3_directory(full: &[u32]) -> Result<Vec<u8>> {
    if !full.len().is_multiple_of(4) {
        return Err(SearchError::Format("bad full q3 directory".into()));
    }
    let entries = full.len() / 4;
    let mut prefix = [0u32; 257];
    for record in full.chunks_exact(4) {
        prefix[((record[0] >> 16) + 1) as usize] += 1;
    }
    for index in 1..prefix.len() {
        prefix[index] += prefix[index - 1];
    }
    let mut out = Vec::with_capacity(257 * 4 + entries * 10);
    for value in prefix {
        put_u32(&mut out, value);
    }
    for record in full.chunks_exact(4) {
        put_u16(&mut out, record[0] as u16);
        put_u32(&mut out, record[1]);
        put_u32(&mut out, record[2]);
    }
    Ok(out)
}

fn section_sizes(data: &SegmentData) -> [u64; SECTION_COUNT] {
    let mut sizes = [0u64; SECTION_COUNT];
    sizes[section::DOC_NAME_OFF] = data.name_off.len() as u64 * 8;
    sizes[section::NAME_BLOB] = data.names.len() as u64;
    sizes[section::UNIT_TEXT_OFF] = data.unit_text_off.len() as u64 * 8;
    sizes[section::TEXT_BLOB] = data.texts.len() as u64;
    sizes[section::UNIT_DOC_OFF] = data.unit_doc_off.len() as u64 * 8;
    sizes[section::UNIT_DOCS] = data.unit_docs.len() as u64 * 4;
    sizes[section::DOC_UNIT] = data.doc_unit.len() as u64 * 4;
    sizes[section::CQ1MASK] = data.content.q1mask.len() as u64;
    sizes[section::CQ3DIR] = data.content.q3dir.len() as u64;
    sizes[section::CQ3POST] = data.content.q3blob.len() as u64;
    sizes[section::NQ1OFF] = data.name_index.q1off.len() as u64 * 4;
    sizes[section::NQ1POST] = data.name_index.q1post.len() as u64 * 4;
    sizes[section::NQ2OFF] = data.name_index.q2off.len() as u64 * 4;
    sizes[section::NQ2POST] = data.name_index.q2post.len() as u64 * 4;
    sizes[section::NQ3DIR] = data.name_index.q3dir.len() as u64 * 4;
    sizes[section::NQ3POST] = data.name_index.q3post.len() as u64 * 4;
    sizes
}

fn update_fnv(hash: &mut u64, bytes: &[u8]) {
    for &byte in bytes {
        *hash = (*hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME);
    }
}

fn write_hashed(file: &mut File, hash: &mut u64, bytes: &[u8]) -> Result<()> {
    file.write_all(bytes)?;
    update_fnv(hash, bytes);
    Ok(())
}

fn write_padding(file: &mut File, hash: &mut u64, count: usize) -> Result<()> {
    const ZEROES: [u8; 8] = [0; 8];
    let mut remaining = count;
    while remaining != 0 {
        let chunk = remaining.min(ZEROES.len());
        write_hashed(file, hash, &ZEROES[..chunk])?;
        remaining -= chunk;
    }
    Ok(())
}

fn stream_u32_values(
    file: &mut File,
    hash: &mut u64,
    values: &[u32],
    scratch: &mut Vec<u8>,
) -> Result<()> {
    const VALUES_PER_CHUNK: usize = 16 * 1024;
    for values in values.chunks(VALUES_PER_CHUNK) {
        scratch.clear();
        scratch.reserve(values.len() * 4);
        for value in values {
            scratch.extend_from_slice(&value.to_le_bytes());
        }
        write_hashed(file, hash, scratch)?;
    }
    Ok(())
}

fn stream_u64_values(
    file: &mut File,
    hash: &mut u64,
    values: &[u64],
    scratch: &mut Vec<u8>,
) -> Result<()> {
    const VALUES_PER_CHUNK: usize = 8 * 1024;
    for values in values.chunks(VALUES_PER_CHUNK) {
        scratch.clear();
        scratch.reserve(values.len() * 8);
        for value in values {
            scratch.extend_from_slice(&value.to_le_bytes());
        }
        write_hashed(file, hash, scratch)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct WrittenSegmentMeta {
    checksum: u64,
    bytes: u64,
}

fn write_segment(path: &Path, data: &SegmentData, durable: bool) -> Result<WrittenSegmentMeta> {
    let sizes = section_sizes(data);
    let mut offsets = [(0u64, 0u64); SECTION_COUNT];
    let mut cursor = HEADER_SIZE as u64;
    for (index, size) in sizes.iter().copied().enumerate() {
        cursor = align8(cursor);
        offsets[index] = (cursor, size);
        cursor = cursor
            .checked_add(size)
            .ok_or_else(|| SearchError::Format("segment size overflow".into()))?;
    }
    let footer_offset = cursor;
    let final_size = footer_offset
        .checked_add(16)
        .ok_or_else(|| SearchError::Format("segment footer overflow".into()))?;

    let mut header = [0u8; HEADER_SIZE];
    header[..8].copy_from_slice(SEG_MAGIC);
    write_u32(&mut header, 8, 5);
    write_u32(&mut header, 12, data.kind as u32);
    write_u32(&mut header, 16, data.doc_base);
    write_u32(&mut header, 20, data.doc_count);
    write_u32(
        &mut header,
        24,
        u32::try_from(data.unit_text_off.len() - 1)
            .map_err(|_| SearchError::Format("unit count overflow".into()))?,
    );
    write_u32(&mut header, 28, SECTION_COUNT as u32);
    for (index, &(offset, size)) in offsets.iter().enumerate() {
        write_u64(&mut header, 32 + index * 16, offset);
        write_u64(&mut header, 40 + index * 16, size);
    }
    write_u32(&mut header, 480, Q3DirKind::Prefix10 as u32);

    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    let mut hash = FNV_OFFSET;
    write_hashed(&mut file, &mut hash, &header)?;
    let mut position = HEADER_SIZE as u64;
    let mut scratch = Vec::with_capacity(64 * 1024);

    for (section_index, &(offset, size)) in offsets.iter().enumerate() {
        if offset < position {
            return Err(SearchError::Format("section write order regression".into()));
        }
        let padding = usize::try_from(offset - position)
            .map_err(|_| SearchError::Format("section padding too large".into()))?;
        write_padding(&mut file, &mut hash, padding)?;
        position = offset;
        match section_index {
            section::DOC_NAME_OFF => {
                stream_u64_values(&mut file, &mut hash, &data.name_off, &mut scratch)?;
            }
            section::NAME_BLOB => write_hashed(&mut file, &mut hash, &data.names)?,
            section::UNIT_TEXT_OFF => {
                stream_u64_values(&mut file, &mut hash, &data.unit_text_off, &mut scratch)?;
            }
            section::TEXT_BLOB => write_hashed(&mut file, &mut hash, &data.texts)?,
            section::UNIT_DOC_OFF => {
                stream_u64_values(&mut file, &mut hash, &data.unit_doc_off, &mut scratch)?;
            }
            section::UNIT_DOCS => {
                stream_u32_values(&mut file, &mut hash, &data.unit_docs, &mut scratch)?;
            }
            section::DOC_UNIT => {
                stream_u32_values(&mut file, &mut hash, &data.doc_unit, &mut scratch)?;
            }
            section::CQ1MASK => write_hashed(&mut file, &mut hash, &data.content.q1mask)?,
            section::CQ3DIR => write_hashed(&mut file, &mut hash, &data.content.q3dir)?,
            section::CQ3POST => write_hashed(&mut file, &mut hash, &data.content.q3blob)?,
            section::NQ1OFF => {
                stream_u32_values(&mut file, &mut hash, &data.name_index.q1off, &mut scratch)?;
            }
            section::NQ1POST => {
                stream_u32_values(&mut file, &mut hash, &data.name_index.q1post, &mut scratch)?;
            }
            section::NQ2OFF => {
                stream_u32_values(&mut file, &mut hash, &data.name_index.q2off, &mut scratch)?;
            }
            section::NQ2POST => {
                stream_u32_values(&mut file, &mut hash, &data.name_index.q2post, &mut scratch)?;
            }
            section::NQ3DIR => {
                stream_u32_values(&mut file, &mut hash, &data.name_index.q3dir, &mut scratch)?;
            }
            section::NQ3POST => {
                stream_u32_values(&mut file, &mut hash, &data.name_index.q3post, &mut scratch)?;
            }
            section::CQ1OFF
            | section::CQ1POST
            | section::CQ1RARE
            | section::CQ2OFF
            | section::CQ2POST
            | section::TEXT_BLOCK_DIR
            | section::TEXT_BLOCK_BLOB => {}
            _ => return Err(SearchError::Format("unknown segment section".into())),
        }
        position = position
            .checked_add(size)
            .ok_or_else(|| SearchError::Format("segment position overflow".into()))?;
    }
    if position != footer_offset {
        return Err(SearchError::Format(
            "segment streaming size mismatch".into(),
        ));
    }
    file.write_all(FOOTER_MAGIC)?;
    file.write_all(&hash.to_le_bytes())?;
    if file.metadata()?.len() != final_size {
        return Err(SearchError::Format("segment final size mismatch".into()));
    }
    if durable {
        file.sync_all()?;
    }
    set_read_only(path)?;
    Ok(WrittenSegmentMeta {
        checksum: hash,
        bytes: final_size,
    })
}

fn set_read_only(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o444))?;
    }
    #[cfg(not(unix))]
    {
        // Keep Windows files deletable so rebuilding the dedicated index directory is reliable.
        // Immutability is enforced by the generation/manifest publish protocol, not DOS attributes.
        let _ = path;
    }
    Ok(())
}

fn cpp_defaultfloat10(value: f64) -> String {
    if value == 0.0 {
        return "0".to_owned();
    }
    let exponent = value.abs().log10().floor() as i32;
    if !(-4..10).contains(&exponent) {
        let raw = format!("{value:.9e}");
        let (mantissa, exp) = raw.split_once('e').expect("scientific format has exponent");
        let mut mantissa = mantissa
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_owned();
        if mantissa == "-0" {
            mantissa = "0".to_owned();
        }
        let parsed = exp.parse::<i32>().expect("formatted exponent is numeric");
        return format!("{mantissa}e{parsed:+03}");
    }
    let decimals = usize::try_from((9 - exponent).max(0)).expect("nonnegative decimal count");
    let fixed = format!("{value:.decimals$}");
    fixed.trim_end_matches('0').trim_end_matches('.').to_owned()
}

fn write_manifest(
    output_dir: &Path,
    mode: BuildMode,
    docs: usize,
    entries: &[ManifestEntry],
    durable: bool,
) -> Result<()> {
    let mut text = String::new();
    text.push_str(MANIFEST_MAGIC);
    text.push('\n');
    text.push_str(&format!("mode {}\n", mode.label()));
    text.push_str(&format!("docs {docs}\n"));
    text.push_str(&format!("segments {}\n", entries.len()));
    for entry in entries {
        text.push_str(&format!(
            "segment {} {} {} {} {} {} {}\n",
            entry.file,
            entry.base,
            entry.count,
            entry.kind.as_str(),
            cpp_defaultfloat10(entry.sample.duplicate_ratio),
            cpp_defaultfloat10(entry.sample.run_unique_ratio),
            entry.bytes
        ));
    }
    let temp = output_dir.join("manifest.tmp");
    let final_path = output_dir.join("manifest.txt");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temp)?;
    file.write_all(text.as_bytes())?;
    if durable {
        file.sync_all()?;
    }
    drop(file);
    set_read_only(&temp)?;
    fs::rename(&temp, &final_path)?;
    if durable {
        sync_directory(output_dir)?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

#[allow(dead_code)]
fn read_all(path: &Path) -> Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod raw_posting_tests {
    use super::{
        NamePostingEmitter, build_raw_postings_comparison, build_raw_postings_radix, direct_grams,
    };

    #[test]
    fn radix_name_postings_match_comparison_sort_exactly() {
        let names = [
            b"src/main.rs".as_slice(),
            b"src/lib.rs".as_slice(),
            b"docs/README.md".as_slice(),
            "日本語/検索テスト.txt".as_bytes(),
            b"very/long/path/with/repeated/repeated/repeated/name.json".as_slice(),
        ];
        let grams = (0..5000)
            .map(|index| direct_grams(names[index % names.len()], true))
            .collect::<Vec<_>>();
        let radix = build_raw_postings_radix(grams.clone()).unwrap();
        let comparison = build_raw_postings_comparison(grams).unwrap();
        assert_eq!(radix.q1off, comparison.q1off);
        assert_eq!(radix.q1post, comparison.q1post);
        assert_eq!(radix.q2off, comparison.q2off);
        assert_eq!(radix.q2post, comparison.q2post);
        assert_eq!(radix.q3dir, comparison.q3dir);
        assert_eq!(radix.q3post, comparison.q3post);
    }
    #[test]
    fn streaming_name_postings_match_legacy_gram_pipeline_exactly() {
        let names = [
            b"aaaa/repeated/repeated/repeated.txt".as_slice(),
            b"src/component_001/component_001.cpp".as_slice(),
            b"UPPER/lower/Mixed_Case_123.md".as_slice(),
            "日本語/検索検索検索/資料.txt".as_bytes(),
            b"x".as_slice(),
            b"".as_slice(),
        ];
        let count = 5000usize;
        let total_name_bytes = (0..count)
            .map(|index| names[index % names.len()].len())
            .sum();
        let mut emitter = NamePostingEmitter::with_capacity(total_name_bytes);
        let mut legacy = Vec::with_capacity(count);
        for index in 0..count {
            let name = names[index % names.len()];
            emitter.emit(index, name).unwrap();
            legacy.push(direct_grams(name, true));
        }
        let streaming = emitter.finish().unwrap();
        let legacy = build_raw_postings_radix(legacy).unwrap();
        assert_eq!(streaming.q1off, legacy.q1off);
        assert_eq!(streaming.q1post, legacy.q1post);
        assert_eq!(streaming.q2off, legacy.q2off);
        assert_eq!(streaming.q2post, legacy.q2post);
        assert_eq!(streaming.q3dir, legacy.q3dir);
        assert_eq!(streaming.q3post, legacy.q3post);
    }
}
