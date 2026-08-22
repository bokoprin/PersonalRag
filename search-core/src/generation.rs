use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::builder::{
    BuildOptions, BuildReport, UnifiedIndexAssembler, build_index, build_index_unified,
};
use crate::format::{Result, SearchError, fnv1a};
use crate::index::{AccelerationProfile, LazyPersistentIndex, verify_index};
use crate::integration::{PlannedUpsert, UpdatePlan};
use crate::mapped_file::MappedFile;
use crate::types::{DocumentInput, Generation, LogicalDocId};

const GENERATION_MAGIC: &str = "PRGEN001";
const CURRENT_MAGIC: &str = "PRCUR001";
const DOCMAP_MAGIC: &[u8; 8] = b"PRMAP001";
const TOMBSTONE_MAGIC: &[u8; 8] = b"PRTMB001";
const SIDECAR_FOOTER_BYTES: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogicalDocument {
    pub logical_id: LogicalDocId,
    pub document: DocumentInput,
}

impl LogicalDocument {
    #[must_use]
    pub fn new(logical_id: LogicalDocId, document: DocumentInput) -> Self {
        Self {
            logical_id,
            document,
        }
    }
}

/// Logical identity for an index that has already been built in physical document order.
///
/// This is the adoption boundary used by application-owned high-throughput ingestion pipelines:
/// the portable generation layer adds logical IDs without rebuilding document payloads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogicalDocumentIdentity {
    pub logical_id: LogicalDocId,
    pub key: String,
    pub display_path: String,
}

impl LogicalDocumentIdentity {
    #[must_use]
    pub fn new(
        logical_id: LogicalDocId,
        key: impl Into<String>,
        display_path: impl Into<String>,
    ) -> Self {
        Self {
            logical_id,
            key: key.into(),
            display_path: display_path.into(),
        }
    }
}

/// Proof that a portable base index completed a full read-back checksum verification.
///
/// The token has no public constructor and is consumed by generation adoption, preventing the
/// high-throughput adoption path from accidentally skipping its one required full index verify.
#[derive(Debug)]
pub struct VerifiedBuiltIndex {
    path: PathBuf,
}

pub fn verify_built_index_for_generation_adoption(
    built_index: impl AsRef<Path>,
) -> Result<VerifiedBuiltIndex> {
    let path = built_index.as_ref().to_path_buf();
    verify_index(&path)?;
    Ok(VerifiedBuiltIndex { path })
}

#[derive(Clone, Debug)]
pub struct GenerationReport {
    pub generation: Generation,
    pub live_docs: usize,
    pub delta_count: usize,
    pub build: Option<BuildReport>,
    pub compacted: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompactionAutoPolicy {
    pub max_delta_count: usize,
    pub max_delta_bytes_ratio: f64,
    pub max_tombstone_ratio: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct CompactionMetrics {
    pub live_docs: usize,
    pub delta_count: usize,
    pub base_bytes: u64,
    pub delta_bytes: u64,
    pub tombstone_events: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CompactionReasons {
    pub delta_count: bool,
    pub delta_bytes: bool,
    pub tombstones: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CompactionDecision {
    pub policy: CompactionAutoPolicy,
    pub metrics: CompactionMetrics,
    pub reasons: CompactionReasons,
    pub recommended: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceKind {
    Base,
    Delta,
}

impl SourceKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Delta => "delta",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        match value {
            "base" => Ok(Self::Base),
            "delta" => Ok(Self::Delta),
            _ => Err(SearchError::Format(format!(
                "bad generation source kind {value}"
            ))),
        }
    }
}

#[derive(Clone, Debug)]
struct SourceDescriptor {
    kind: SourceKind,
    generation: Generation,
    index_dir: String,
    map_file: String,
    tombstone_file: Option<String>,
}

#[derive(Clone, Debug)]
struct GenerationManifest {
    generation: Generation,
    sources: Vec<SourceDescriptor>,
}

#[derive(Clone, Copy, Debug)]
struct DocMapEntry {
    logical_id: LogicalDocId,
    key_start: u32,
    key_len: u32,
    display_start: u32,
    display_len: u32,
}

struct CompactDocMap {
    mapped: MappedFile,
    entries: Vec<DocMapEntry>,
}

impl CompactDocMap {
    fn open(path: &Path) -> Result<Self> {
        let mapped = MappedFile::open(path)?;
        let bytes = mapped.as_slice();
        if bytes.len() < 24 || bytes.get(..8) != Some(DOCMAP_MAGIC.as_slice()) {
            return Err(SearchError::Format("bad logical map".into()));
        }
        let payload_end = bytes.len() - SIDECAR_FOOTER_BYTES;
        let expected = u64::from_le_bytes(bytes[payload_end..].try_into().expect("fixed slice"));
        if fnv1a(&bytes[..payload_end]) != expected {
            return Err(SearchError::Format("logical map checksum mismatch".into()));
        }
        if payload_end > u32::MAX as usize {
            return Err(SearchError::Format(
                "logical map exceeds compact 4GiB address space".into(),
            ));
        }
        let count = u64::from_le_bytes(bytes[8..16].try_into().expect("fixed slice"));
        let count = usize::try_from(count)
            .map_err(|_| SearchError::Format("logical map count too large".into()))?;
        let mut position = 16usize;
        let mut entries = Vec::with_capacity(count);
        for _ in 0..count {
            let header = bytes
                .get(position..position + 16)
                .ok_or_else(|| SearchError::Format("truncated logical map entry".into()))?;
            let logical_id = u64::from_le_bytes(header[0..8].try_into().expect("fixed slice"));
            let key_len = u32::from_le_bytes(header[8..12].try_into().expect("fixed slice"));
            let display_len = u32::from_le_bytes(header[12..16].try_into().expect("fixed slice"));
            position += 16;
            let key_start = u32::try_from(position)
                .map_err(|_| SearchError::Format("logical map key offset overflow".into()))?;
            let key_end = position
                .checked_add(key_len as usize)
                .ok_or_else(|| SearchError::Format("logical map key overflow".into()))?;
            let display_start = u32::try_from(key_end)
                .map_err(|_| SearchError::Format("logical map display offset overflow".into()))?;
            let display_end = key_end
                .checked_add(display_len as usize)
                .ok_or_else(|| SearchError::Format("logical map path overflow".into()))?;
            if display_end > payload_end {
                return Err(SearchError::Format("truncated logical map strings".into()));
            }
            std::str::from_utf8(&bytes[position..key_end])
                .map_err(|_| SearchError::Format("logical map key is not UTF-8".into()))?;
            std::str::from_utf8(&bytes[key_end..display_end])
                .map_err(|_| SearchError::Format("logical map path is not UTF-8".into()))?;
            entries.push(DocMapEntry {
                logical_id,
                key_start,
                key_len,
                display_start,
                display_len,
            });
            position = display_end;
        }
        if position != payload_end {
            return Err(SearchError::Format("logical map trailing data".into()));
        }
        Ok(Self { mapped, entries })
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
    fn get(&self, index: usize) -> Option<&DocMapEntry> {
        self.entries.get(index)
    }
    fn iter(&self) -> impl Iterator<Item = &DocMapEntry> {
        self.entries.iter()
    }

    fn string_at(&self, start: u32, len: u32) -> Result<&str> {
        let begin = start as usize;
        let end = begin
            .checked_add(len as usize)
            .ok_or_else(|| SearchError::Format("logical map string range overflow".into()))?;
        let bytes = self
            .mapped
            .as_slice()
            .get(begin..end)
            .ok_or_else(|| SearchError::Format("logical map string out of bounds".into()))?;
        std::str::from_utf8(bytes)
            .map_err(|_| SearchError::Format("logical map string is not UTF-8".into()))
    }

    fn key(&self, entry: &DocMapEntry) -> Result<&str> {
        self.string_at(entry.key_start, entry.key_len)
    }

    fn display_path(&self, entry: &DocMapEntry) -> Result<&str> {
        self.string_at(entry.display_start, entry.display_len)
    }
}

struct VersionSource {
    kind: SourceKind,
    component_path: PathBuf,
    index: LazyPersistentIndex,
    map: CompactDocMap,
    visible: Vec<u8>,
    tombstone_count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VersionLocation {
    source: usize,
    physical_doc: u32,
}

type MergedTaskResult = Option<Result<Vec<LogicalDocId>>>;

pub struct MergedIndex {
    root: PathBuf,
    generation: Generation,
    manifest_path: PathBuf,
    sources: Vec<VersionSource>,
    live: HashMap<LogicalDocId, VersionLocation>,
    live_order: Vec<LogicalDocId>,
    query_tasks: Vec<(usize, usize)>,
}

struct MergedPoolJob {
    query: Arc<[u8]>,
    names: bool,
    positional_sources: Arc<[bool]>,
    variable_sources: Arc<[bool]>,
    adaptive_sources: Arc<[bool]>,
    active_workers: usize,
    next: AtomicUsize,
    remaining: AtomicUsize,
    results: Mutex<Vec<MergedTaskResult>>,
    done_lock: Mutex<()>,
    done_cv: std::sync::Condvar,
}

struct MergedPoolState {
    generation: u64,
    job: Option<Arc<MergedPoolJob>>,
}

struct MergedPoolCoordinator {
    state: Mutex<MergedPoolState>,
    cv: std::sync::Condvar,
    shutdown: std::sync::atomic::AtomicBool,
}

impl MergedIndex {
    pub fn open(root: impl AsRef<Path>, verify_checksum: bool) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let (generation, manifest_relative) = read_current(&root)?;
        let manifest_path = root.join(&manifest_relative);
        let manifest = read_generation_manifest(&manifest_path)?;
        if manifest.generation != generation {
            return Err(SearchError::Format(
                "CURRENT/generation manifest mismatch".into(),
            ));
        }
        if manifest.sources.is_empty() || manifest.sources[0].kind != SourceKind::Base {
            return Err(SearchError::Format(
                "generation manifest must start with a base source".into(),
            ));
        }

        let mut sources = Vec::with_capacity(manifest.sources.len());
        let mut live = HashMap::<LogicalDocId, VersionLocation>::new();
        for (source_index, descriptor) in manifest.sources.iter().enumerate() {
            if descriptor.generation > generation {
                return Err(SearchError::Format(
                    "source generation exceeds published generation".into(),
                ));
            }
            let index_dir = safe_join(&root, &descriptor.index_dir)?;
            let map_path = safe_join(&index_dir, &descriptor.map_file)?;
            if verify_checksum {
                verify_index(&index_dir)?;
            }
            let index = LazyPersistentIndex::open(&index_dir)?;
            let map = CompactDocMap::open(&map_path)?;
            if map.len() as u64 != index.docs() {
                return Err(SearchError::Format(
                    "logical document map/index count mismatch".into(),
                ));
            }
            let mut seen = HashSet::with_capacity(map.len());
            for entry in map.iter() {
                if entry.logical_id == 0
                    || map.key(entry)?.is_empty()
                    || !seen.insert(entry.logical_id)
                {
                    return Err(SearchError::Format(
                        "invalid or duplicate logical id in source map".into(),
                    ));
                }
            }

            let tombstone_count = if descriptor.kind == SourceKind::Delta {
                let tombstone_file = descriptor.tombstone_file.as_deref().ok_or_else(|| {
                    SearchError::Format("delta source missing tombstone file".into())
                })?;
                let tombstones = read_tombstones(&safe_join(&index_dir, tombstone_file)?)?;
                let count = tombstones.len();
                for logical_id in tombstones {
                    live.remove(&logical_id);
                }
                count
            } else if descriptor.tombstone_file.is_some() {
                return Err(SearchError::Format(
                    "base source must not carry tombstones".into(),
                ));
            } else {
                0
            };
            for (physical_doc, entry) in map.iter().enumerate() {
                let physical_doc = u32::try_from(physical_doc)
                    .map_err(|_| SearchError::Format("physical document id overflow".into()))?;
                if descriptor.kind == SourceKind::Base && live.contains_key(&entry.logical_id) {
                    return Err(SearchError::Format(
                        "duplicate logical id in base generation".into(),
                    ));
                }
                live.insert(
                    entry.logical_id,
                    VersionLocation {
                        source: source_index,
                        physical_doc,
                    },
                );
            }
            let visible = vec![0u8; map.len()];
            sources.push(VersionSource {
                kind: descriptor.kind,
                component_path: index_dir,
                index,
                map,
                visible,
                tombstone_count,
            });
        }
        for location in live.values() {
            let source = sources
                .get_mut(location.source)
                .ok_or_else(|| SearchError::Format("live source location out of bounds".into()))?;
            let slot = source
                .visible
                .get_mut(location.physical_doc as usize)
                .ok_or_else(|| {
                    SearchError::Format("live physical document out of bounds".into())
                })?;
            *slot = 1;
        }
        let mut live_order = live.keys().copied().collect::<Vec<_>>();
        live_order.sort_unstable();
        let mut query_tasks = Vec::new();
        for (source_index, source) in sources.iter().enumerate() {
            query_tasks
                .extend((0..source.index.segment_count()).map(|segment| (source_index, segment)));
        }
        Ok(Self {
            root,
            generation,
            manifest_path,
            sources,
            live,
            live_order,
            query_tasks,
        })
    }

    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn manifest_path(&self) -> &Path {
        &self.manifest_path
    }

    #[must_use]
    pub fn live_docs(&self) -> usize {
        self.live_order.len()
    }

    #[must_use]
    pub fn delta_count(&self) -> usize {
        self.sources.len().saturating_sub(1)
    }

    #[must_use]
    pub fn tuned_compaction_policy(&self) -> CompactionAutoPolicy {
        let max_delta_count = if self.live_docs() < 100_000 {
            24
        } else if self.live_docs() < 500_000 {
            32
        } else {
            48
        };
        CompactionAutoPolicy {
            max_delta_count,
            max_delta_bytes_ratio: 0.20,
            max_tombstone_ratio: 0.20,
        }
    }

    pub fn compaction_metrics(&self) -> Result<CompactionMetrics> {
        let mut base_bytes = 0u64;
        let mut delta_bytes = 0u64;
        let mut tombstone_events = 0u64;
        for source in &self.sources {
            let bytes = directory_bytes_recursive(&source.component_path)?;
            match source.kind {
                SourceKind::Base => {
                    base_bytes = base_bytes
                        .checked_add(bytes)
                        .ok_or_else(|| SearchError::Format("base byte count overflow".into()))?;
                }
                SourceKind::Delta => {
                    delta_bytes = delta_bytes
                        .checked_add(bytes)
                        .ok_or_else(|| SearchError::Format("delta byte count overflow".into()))?;
                    tombstone_events = tombstone_events
                        .checked_add(source.tombstone_count as u64)
                        .ok_or_else(|| SearchError::Format("tombstone count overflow".into()))?;
                }
            }
        }
        Ok(CompactionMetrics {
            live_docs: self.live_docs(),
            delta_count: self.delta_count(),
            base_bytes,
            delta_bytes,
            tombstone_events,
        })
    }

    pub fn compaction_decision(&self, policy: CompactionAutoPolicy) -> Result<CompactionDecision> {
        if policy.max_delta_count == 0
            || !policy.max_delta_bytes_ratio.is_finite()
            || !policy.max_tombstone_ratio.is_finite()
            || !(0.0..=1.0).contains(&policy.max_delta_bytes_ratio)
            || !(0.0..=1.0).contains(&policy.max_tombstone_ratio)
        {
            return Err(SearchError::InvalidArgument(
                "invalid compaction auto policy".into(),
            ));
        }
        let metrics = self.compaction_metrics()?;
        let delta_ratio = if metrics.base_bytes == 0 {
            0.0
        } else {
            metrics.delta_bytes as f64 / metrics.base_bytes as f64
        };
        let tombstone_ratio = if metrics.live_docs == 0 {
            0.0
        } else {
            metrics.tombstone_events as f64 / metrics.live_docs as f64
        };
        let reasons = CompactionReasons {
            delta_count: metrics.delta_count >= policy.max_delta_count,
            delta_bytes: delta_ratio >= policy.max_delta_bytes_ratio,
            tombstones: tombstone_ratio >= policy.max_tombstone_ratio,
        };
        Ok(CompactionDecision {
            policy,
            metrics,
            reasons,
            recommended: reasons.delta_count || reasons.delta_bytes || reasons.tombstones,
        })
    }

    pub fn auto_compaction_decision(&self) -> Result<CompactionDecision> {
        self.compaction_decision(self.tuned_compaction_policy())
    }

    pub fn search_content(&self, query: impl AsRef<[u8]>) -> Result<Vec<LogicalDocId>> {
        self.search_with_workers(query.as_ref(), false, 0)
    }

    /// Check one live logical document without exposing its physical component location.
    ///
    /// Application adapters use this for path-ordered First-N scans after incremental inserts,
    /// where stable logical-ID order is intentionally independent from path order.
    pub fn logical_document_contains(
        &self,
        logical_id: LogicalDocId,
        query: impl AsRef<[u8]>,
        names: bool,
    ) -> Result<bool> {
        let Some(location) = self.live.get(&logical_id) else {
            return Ok(false);
        };
        self.sources[location.source].index.document_contains(
            location.physical_doc,
            query.as_ref(),
            names,
        )
    }

    pub fn search_name(&self, query: impl AsRef<[u8]>) -> Result<Vec<LogicalDocId>> {
        self.search_with_workers(query.as_ref(), true, 0)
    }

    pub fn search_content_with_workers(
        &self,
        query: impl AsRef<[u8]>,
        workers: usize,
    ) -> Result<Vec<LogicalDocId>> {
        self.search_with_workers(query.as_ref(), false, workers)
    }

    fn recommended_global_workers(&self, query: &[u8], names: bool) -> Result<usize> {
        if self.query_tasks.len() < 2 {
            return Ok(1);
        }
        let available = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(4)
            .min(self.query_tasks.len());
        if available <= 1 || names {
            return Ok(available.max(1));
        }
        let folded = crate::fold_ascii(query);
        let mut estimated = 0u64;
        let mut has_accelerated = false;
        for source in &self.sources {
            let plan = source.index.plan_content_query(&folded, None)?;
            estimated = estimated
                .checked_add(plan.estimated_candidates)
                .ok_or_else(|| SearchError::Format("merged candidate estimate overflow".into()))?;
            has_accelerated |= matches!(
                plan.mode,
                crate::index::ContentPlanMode::PositionalDriven
                    | crate::index::ContentPlanMode::VariableGramDriven
                    | crate::index::ContentPlanMode::AdaptiveGramDriven
            );
        }
        // PRPOS001 removes verifier reads for covered dense literals, so those tasks can safely
        // use the global pool. Verifier-heavy q2/long queries without PRPOS remain single-worker
        // to preserve mmap locality and stable tail latency. One- and three-byte queries are exact
        // directly from q1/q3 and can also use the global queue when there is enough work.
        if !has_accelerated && !matches!(folded.len(), 1 | 3) {
            return Ok(1);
        }
        Ok(if estimated < 16_384 { 1 } else { available })
    }

    fn accelerated_source_modes(
        &self,
        query: &[u8],
        names: bool,
    ) -> Result<(Vec<bool>, Vec<bool>, Vec<bool>)> {
        if names {
            return Ok((
                vec![false; self.sources.len()],
                vec![false; self.sources.len()],
                vec![false; self.sources.len()],
            ));
        }
        let mut positional = Vec::with_capacity(self.sources.len());
        let mut variable = Vec::with_capacity(self.sources.len());
        let mut adaptive = Vec::with_capacity(self.sources.len());
        for source in &self.sources {
            let mode = source.index.plan_content_query(query, None)?.mode;
            positional.push(mode == crate::index::ContentPlanMode::PositionalDriven);
            variable.push(mode == crate::index::ContentPlanMode::VariableGramDriven);
            adaptive.push(mode == crate::index::ContentPlanMode::AdaptiveGramDriven);
        }
        Ok((positional, variable, adaptive))
    }

    fn search_with_workers(
        &self,
        query: &[u8],
        names: bool,
        workers: usize,
    ) -> Result<Vec<LogicalDocId>> {
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let workers = if workers == 0 {
            self.recommended_global_workers(query, names)?
        } else {
            workers
        }
        .max(1)
        .min(self.query_tasks.len().max(1));
        let (positional_sources, variable_sources, adaptive_sources) =
            self.accelerated_source_modes(query, names)?;
        if workers <= 1 || self.query_tasks.len() <= 1 {
            let mut out = Vec::new();
            for &(source_index, segment_index) in &self.query_tasks {
                let source = &self.sources[source_index];
                let hits = if names {
                    source.index.search_segment_name(segment_index, query)?
                } else if adaptive_sources[source_index] {
                    source
                        .index
                        .search_segment_adaptive_content(segment_index, query)?
                } else if variable_sources[source_index] {
                    source
                        .index
                        .search_segment_variable_content(segment_index, query)?
                } else if positional_sources[source_index] {
                    source
                        .index
                        .search_segment_positional_content(segment_index, query)?
                } else {
                    source.index.search_segment_content(segment_index, query)?
                };
                self.append_visible_hits(source, &hits, &mut out)?;
            }
            out.sort_unstable();
            out.dedup();
            return Ok(out);
        }
        let next = AtomicUsize::new(0);
        let results: Arc<Mutex<Vec<MergedTaskResult>>> = Arc::new(Mutex::new(
            std::iter::repeat_with(|| None)
                .take(self.query_tasks.len())
                .collect(),
        ));
        std::thread::scope(|scope| {
            for _ in 0..workers {
                let results = Arc::clone(&results);
                let next = &next;
                let positional_sources = &positional_sources;
                let variable_sources = &variable_sources;
                let adaptive_sources = &adaptive_sources;
                scope.spawn(move || {
                    loop {
                        let task_index = next.fetch_add(1, AtomicOrdering::Relaxed);
                        if task_index >= self.query_tasks.len() {
                            break;
                        }
                        let (source_index, segment_index) = self.query_tasks[task_index];
                        let source = &self.sources[source_index];
                        let result = (|| {
                            let hits = if names {
                                source.index.search_segment_name(segment_index, query)?
                            } else if adaptive_sources[source_index] {
                                source
                                    .index
                                    .search_segment_adaptive_content(segment_index, query)?
                            } else if variable_sources[source_index] {
                                source
                                    .index
                                    .search_segment_variable_content(segment_index, query)?
                            } else if positional_sources[source_index] {
                                source
                                    .index
                                    .search_segment_positional_content(segment_index, query)?
                            } else {
                                source.index.search_segment_content(segment_index, query)?
                            };
                            let mut visible = Vec::new();
                            self.append_visible_hits(source, &hits, &mut visible)?;
                            Ok(visible)
                        })();
                        results.lock().expect("merged query result mutex poisoned")[task_index] =
                            Some(result);
                    }
                });
            }
        });
        let mut out = Vec::new();
        for (index, result) in Arc::try_unwrap(results)
            .map_err(|_| SearchError::Format("merged query result ownership leak".into()))?
            .into_inner()
            .map_err(|_| SearchError::Format("merged query result mutex poisoned".into()))?
            .into_iter()
            .enumerate()
        {
            let hits = result.ok_or_else(|| {
                SearchError::Format(format!("merged query task {index} missing"))
            })??;
            out.extend(hits);
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    fn append_visible_hits(
        &self,
        source: &VersionSource,
        hits: &[u32],
        out: &mut Vec<LogicalDocId>,
    ) -> Result<()> {
        for &physical_doc in hits {
            let map_index = usize::try_from(physical_doc)
                .map_err(|_| SearchError::Format("physical document id overflow".into()))?;
            let entry = source.map.get(map_index).ok_or_else(|| {
                SearchError::Format("query hit outside logical document map".into())
            })?;
            if source.visible.get(map_index).copied() == Some(1) {
                out.push(entry.logical_id);
            }
        }
        Ok(())
    }

    pub fn prefers_index_driven_first_n(
        &self,
        query: impl AsRef<[u8]>,
        names: bool,
        limit: usize,
    ) -> Result<bool> {
        let query = query.as_ref();
        if query.is_empty() || limit == 0 || names {
            return Ok(false);
        }
        let folded = crate::fold_ascii(query);
        let mut estimated = 0u64;
        for source in &self.sources {
            estimated = estimated
                .checked_add(
                    source
                        .index
                        .estimated_content_candidates_for_planner(&folded)?,
                )
                .ok_or_else(|| SearchError::Format("merged candidate estimate overflow".into()))?;
        }
        Ok(estimated <= (limit as u64).saturating_mul(16))
    }

    pub fn first_n(
        &self,
        query: impl AsRef<[u8]>,
        names: bool,
        limit: usize,
    ) -> Result<Vec<LogicalDocId>> {
        let query = query.as_ref();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        if self.prefers_index_driven_first_n(query, names, limit)? {
            let folded = crate::fold_ascii(query);
            let mut hits = self.search_content(&folded)?;
            hits.truncate(limit);
            return Ok(hits);
        }
        let mut out = Vec::with_capacity(limit.min(self.live_order.len()));
        for logical_id in &self.live_order {
            let location = self.live.get(logical_id).ok_or_else(|| {
                SearchError::Format("live logical id missing version location".into())
            })?;
            if self.sources[location.source].index.document_contains(
                location.physical_doc,
                query,
                names,
            )? {
                out.push(*logical_id);
                if out.len() == limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    pub fn visit_live_documents<F>(&self, mut visitor: F) -> Result<()>
    where
        F: FnMut(LogicalDocument) -> Result<()>,
    {
        for logical_id in &self.live_order {
            let location = self.live.get(logical_id).ok_or_else(|| {
                SearchError::Format("live logical id missing version location".into())
            })?;
            let source = &self.sources[location.source];
            let map = source
                .map
                .get(location.physical_doc as usize)
                .ok_or_else(|| SearchError::Format("logical map out of bounds".into()))?;
            let (normalized_name, normalized_content) =
                source.index.document_bytes(location.physical_doc)?;
            visitor(LogicalDocument {
                logical_id: *logical_id,
                document: DocumentInput::new(
                    source.map.key(map)?.to_owned(),
                    source.map.display_path(map)?.to_owned(),
                    normalized_name,
                    normalized_content,
                ),
            })?;
        }
        Ok(())
    }

    pub fn live_documents(&self) -> Result<Vec<LogicalDocument>> {
        let mut out = Vec::with_capacity(self.live_order.len());
        self.visit_live_documents(|document| {
            out.push(document);
            Ok(())
        })?;
        Ok(out)
    }
}

/// Long-lived application-facing search session for a published base+delta generation.
///
/// A single global worker pool serves all source/segment tasks, so incremental generations do
/// not multiply thread counts by the number of deltas. The session is immutable and bound to the
/// generation that was current when it was opened; Latest-Wins application code opens a fresh
/// session after publishing a new generation.
pub struct MergedSearchSession {
    index: Arc<MergedIndex>,
    coordinator: Arc<MergedPoolCoordinator>,
    threads: Vec<std::thread::JoinHandle<()>>,
    submit_lock: Mutex<()>,
}

impl MergedSearchSession {
    pub fn open(root: impl AsRef<Path>, verify_checksum: bool, workers: usize) -> Result<Self> {
        let index = Arc::new(MergedIndex::open(root, verify_checksum)?);
        let worker_count = workers
            .max(1)
            .min(std::thread::available_parallelism().map_or(1, usize::from))
            .min(index.query_tasks.len().max(1));
        let coordinator = Arc::new(MergedPoolCoordinator {
            state: Mutex::new(MergedPoolState {
                generation: 0,
                job: None,
            }),
            cv: std::sync::Condvar::new(),
            shutdown: std::sync::atomic::AtomicBool::new(false),
        });
        let mut threads = Vec::with_capacity(worker_count);
        for worker_id in 0..worker_count {
            let index = Arc::clone(&index);
            let coordinator = Arc::clone(&coordinator);
            threads.push(std::thread::spawn(move || {
                let mut seen_generation = 0u64;
                loop {
                    let (generation, job) = {
                        let mut state = coordinator
                            .state
                            .lock()
                            .expect("merged query pool coordinator poisoned");
                        while state.generation == seen_generation
                            && !coordinator
                                .shutdown
                                .load(std::sync::atomic::Ordering::Acquire)
                        {
                            state = coordinator
                                .cv
                                .wait(state)
                                .expect("merged query pool coordinator poisoned");
                        }
                        if coordinator
                            .shutdown
                            .load(std::sync::atomic::Ordering::Acquire)
                        {
                            return;
                        }
                        (state.generation, state.job.as_ref().map(Arc::clone))
                    };
                    seen_generation = generation;
                    let Some(job) = job else {
                        continue;
                    };
                    if worker_id >= job.active_workers {
                        continue;
                    }
                    loop {
                        let task_index = job.next.fetch_add(1, AtomicOrdering::Relaxed);
                        if task_index >= index.query_tasks.len() {
                            break;
                        }
                        let (source_index, segment_index) = index.query_tasks[task_index];
                        let source = &index.sources[source_index];
                        let result = (|| {
                            let hits = if job.names {
                                source
                                    .index
                                    .search_segment_name(segment_index, job.query.as_ref())?
                            } else if job.adaptive_sources[source_index] {
                                source.index.search_segment_adaptive_content(
                                    segment_index,
                                    job.query.as_ref(),
                                )?
                            } else if job.variable_sources[source_index] {
                                source.index.search_segment_variable_content(
                                    segment_index,
                                    job.query.as_ref(),
                                )?
                            } else if job.positional_sources[source_index] {
                                source.index.search_segment_positional_content(
                                    segment_index,
                                    job.query.as_ref(),
                                )?
                            } else {
                                source
                                    .index
                                    .search_segment_content(segment_index, job.query.as_ref())?
                            };
                            let mut visible = Vec::new();
                            index.append_visible_hits(source, &hits, &mut visible)?;
                            Ok(visible)
                        })();
                        job.results
                            .lock()
                            .expect("merged query pool result mutex poisoned")[task_index] =
                            Some(result);
                    }
                    if job.remaining.fetch_sub(1, AtomicOrdering::AcqRel) == 1 {
                        job.done_cv.notify_one();
                    }
                }
            }));
        }
        Ok(Self {
            index,
            coordinator,
            threads,
            submit_lock: Mutex::new(()),
        })
    }

    #[must_use]
    pub fn index(&self) -> &MergedIndex {
        &self.index
    }

    pub fn search_content(&self, query: impl AsRef<[u8]>) -> Result<Vec<LogicalDocId>> {
        let query = query.as_ref();
        let workers = self
            .index
            .recommended_global_workers(query, false)?
            .min(self.threads.len().max(1));
        self.search_with_workers(query, false, workers)
    }

    pub fn search_content_with_workers(
        &self,
        query: impl AsRef<[u8]>,
        workers: usize,
    ) -> Result<Vec<LogicalDocId>> {
        self.search_with_workers(query.as_ref(), false, workers)
    }

    pub fn search_name(&self, query: impl AsRef<[u8]>) -> Result<Vec<LogicalDocId>> {
        let workers = self.threads.len().max(1);
        self.search_with_workers(query.as_ref(), true, workers)
    }

    fn search_with_workers(
        &self,
        query: &[u8],
        names: bool,
        workers: usize,
    ) -> Result<Vec<LogicalDocId>> {
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let workers = workers
            .max(1)
            .min(self.threads.len().max(1))
            .min(self.index.query_tasks.len().max(1));
        if workers <= 1 || self.index.query_tasks.len() <= 1 {
            return self.index.search_with_workers(query, names, 1);
        }
        let _submit = self
            .submit_lock
            .lock()
            .map_err(|_| SearchError::Format("merged query pool submit mutex poisoned".into()))?;
        let (positional_sources, variable_sources, adaptive_sources) =
            self.index.accelerated_source_modes(query, names)?;
        let job = Arc::new(MergedPoolJob {
            query: Arc::<[u8]>::from(query),
            names,
            positional_sources: Arc::<[bool]>::from(positional_sources),
            variable_sources: Arc::<[bool]>::from(variable_sources),
            adaptive_sources: Arc::<[bool]>::from(adaptive_sources),
            active_workers: workers,
            next: AtomicUsize::new(0),
            remaining: AtomicUsize::new(workers),
            results: Mutex::new(
                std::iter::repeat_with(|| None)
                    .take(self.index.query_tasks.len())
                    .collect(),
            ),
            done_lock: Mutex::new(()),
            done_cv: std::sync::Condvar::new(),
        });
        {
            let mut state = self.coordinator.state.lock().map_err(|_| {
                SearchError::Format("merged query pool coordinator poisoned".into())
            })?;
            state.generation = state.generation.checked_add(1).ok_or_else(|| {
                SearchError::Format("merged query pool generation overflow".into())
            })?;
            state.job = Some(Arc::clone(&job));
        }
        self.coordinator.cv.notify_all();
        let mut guard = job.done_lock.lock().map_err(|_| {
            SearchError::Format("merged query pool completion mutex poisoned".into())
        })?;
        while job.remaining.load(AtomicOrdering::Acquire) != 0 {
            guard = job.done_cv.wait(guard).map_err(|_| {
                SearchError::Format("merged query pool completion mutex poisoned".into())
            })?;
        }
        drop(guard);
        let mut results = job
            .results
            .lock()
            .map_err(|_| SearchError::Format("merged query pool result mutex poisoned".into()))?;
        let mut out = Vec::new();
        for (task_index, result) in results.iter_mut().enumerate() {
            let hits = result.take().ok_or_else(|| {
                SearchError::Format(format!("merged query pool task {task_index} missing"))
            })??;
            out.extend(hits);
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    pub fn first_n(
        &self,
        query: impl AsRef<[u8]>,
        names: bool,
        limit: usize,
    ) -> Result<Vec<LogicalDocId>> {
        let query = query.as_ref();
        if query.is_empty() || limit == 0 || names {
            return self.index.first_n(query, names, limit);
        }
        let folded = crate::fold_ascii(query);
        if self
            .index
            .prefers_index_driven_first_n(&folded, false, limit)?
        {
            let mut hits = self.search_content(&folded)?;
            hits.truncate(limit);
            Ok(hits)
        } else {
            self.index.first_n(&folded, false, limit)
        }
    }
}

impl Drop for MergedSearchSession {
    fn drop(&mut self) {
        self.coordinator
            .shutdown
            .store(true, std::sync::atomic::Ordering::Release);
        self.coordinator.cv.notify_all();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

pub fn initialize_generation(
    root: impl AsRef<Path>,
    documents: &[LogicalDocument],
    options: &BuildOptions,
) -> Result<GenerationReport> {
    let root = root.as_ref();
    if root.join("CURRENT").exists() {
        return Err(SearchError::InvalidArgument(
            "generation store is already initialized".into(),
        ));
    }
    fs::create_dir_all(root.join("components"))?;
    fs::create_dir_all(root.join("generations"))?;

    let mut ordered = documents.to_vec();
    ordered.sort_by_key(|item| item.logical_id);
    validate_logical_documents(&ordered)?;
    let base_relative = "components/base-g0000000000000000".to_owned();
    let base_path = root.join(&base_relative);
    if base_path.exists() {
        return Err(SearchError::InvalidArgument(
            "base component already exists".into(),
        ));
    }
    let temp_path = root.join("components").join(format!(
        ".base-{}-{}.tmp",
        std::process::id(),
        unique_nonce()?
    ));
    let inputs = ordered
        .iter()
        .map(|item| item.document.clone())
        .collect::<Vec<_>>();
    let build = build_index(&inputs, &temp_path, options)?;
    write_doc_map(&temp_path.join("logical-map.bin"), &ordered)?;
    // A generation is published only after a complete read-back checksum pass.
    verify_index(&temp_path)?;
    sync_directory(&temp_path)?;
    fs::rename(&temp_path, &base_path)?;
    sync_directory(&root.join("components"))?;

    let manifest = GenerationManifest {
        generation: 0,
        sources: vec![SourceDescriptor {
            kind: SourceKind::Base,
            generation: 0,
            index_dir: base_relative,
            map_file: "logical-map.bin".into(),
            tombstone_file: None,
        }],
    };
    let manifest_relative = generation_manifest_relative(0, "base");
    publish_generation_manifest(root, &manifest_relative, &manifest)?;
    publish_current(root, 0, &manifest_relative)?;
    Ok(GenerationReport {
        generation: 0,
        live_docs: ordered.len(),
        delta_count: 0,
        build: Some(build),
        compacted: false,
    })
}

/// Adopt an already-built normal portable index as generation zero without rebuilding it.
///
/// `documents` must be in the exact physical document order of `built_index`. This keeps the
/// existing bounded/pipelined ingestion fast path while enabling logical IDs and later deltas.
pub fn initialize_generation_from_built_index(
    root: impl AsRef<Path>,
    built_index: impl AsRef<Path>,
    documents: &[LogicalDocumentIdentity],
) -> Result<GenerationReport> {
    let verified = verify_built_index_for_generation_adoption(built_index)?;
    initialize_generation_from_verified_built_index(root, verified, documents)
}

/// Adopt an index that has already passed a full read-back checksum verification.
///
/// `verified` is consumed so the fast path cannot be called with an arbitrary path. The adoption
/// step only adds the logical map and generation metadata; it never rewrites the verified segment
/// payloads. Call `verify_generation_structure` after publication to validate CURRENT, the
/// generation manifest, the logical map checksum, and logical/physical document consistency.
pub fn initialize_generation_from_verified_built_index(
    root: impl AsRef<Path>,
    verified: VerifiedBuiltIndex,
    documents: &[LogicalDocumentIdentity],
) -> Result<GenerationReport> {
    let root = root.as_ref();
    let built_index = verified.path.as_path();
    if root == built_index {
        return Err(SearchError::InvalidArgument(
            "generation root and built index must be different paths".into(),
        ));
    }
    if root.join("CURRENT").exists() {
        return Err(SearchError::InvalidArgument(
            "generation store is already initialized".into(),
        ));
    }

    // The checksum-heavy segment verification has already been performed to create `verified`.
    // This cheap open revalidates the portable manifest shape and obtains the physical doc count.
    let physical = LazyPersistentIndex::open(built_index)?;
    let physical_docs = usize::try_from(physical.docs())
        .map_err(|_| SearchError::Format("physical document count too large".into()))?;
    if physical_docs != documents.len() {
        return Err(SearchError::InvalidArgument(format!(
            "logical identity count {} does not match physical document count {physical_docs}",
            documents.len()
        )));
    }
    validate_logical_identities(documents)?;

    fs::create_dir_all(root.join("components"))?;
    fs::create_dir_all(root.join("generations"))?;
    let base_relative = "components/base-g0000000000000000".to_owned();
    let base_path = root.join(&base_relative);
    if base_path.exists() {
        return Err(SearchError::InvalidArgument(
            "base component already exists".into(),
        ));
    }

    write_doc_map_identities(&built_index.join("logical-map.bin"), documents)?;
    sync_directory(built_index)?;
    fs::rename(built_index, &base_path)?;
    sync_directory(&root.join("components"))?;

    let manifest = GenerationManifest {
        generation: 0,
        sources: vec![SourceDescriptor {
            kind: SourceKind::Base,
            generation: 0,
            index_dir: base_relative,
            map_file: "logical-map.bin".into(),
            tombstone_file: None,
        }],
    };
    let manifest_relative = generation_manifest_relative(0, "base");
    publish_generation_manifest(root, &manifest_relative, &manifest)?;
    publish_current(root, 0, &manifest_relative)?;
    Ok(GenerationReport {
        generation: 0,
        live_docs: documents.len(),
        delta_count: 0,
        build: None,
        compacted: false,
    })
}

pub fn publish_incremental_update(
    root: impl AsRef<Path>,
    plan: &UpdatePlan,
    options: &BuildOptions,
) -> Result<GenerationReport> {
    publish_incremental_update_profile(root.as_ref(), plan, options, AccelerationProfile::None)
}

pub fn publish_incremental_update_unified(
    root: impl AsRef<Path>,
    plan: &UpdatePlan,
    options: &BuildOptions,
) -> Result<GenerationReport> {
    publish_incremental_update_profile(
        root.as_ref(),
        plan,
        options,
        AccelerationProfile::AdaptiveDelta,
    )
}

fn publish_incremental_update_profile(
    root: &Path,
    plan: &UpdatePlan,
    options: &BuildOptions,
    acceleration: AccelerationProfile,
) -> Result<GenerationReport> {
    let current = MergedIndex::open(root, true)?;
    if current.generation != plan.base_generation {
        return Err(SearchError::InvalidArgument(
            "incremental plan base generation does not match CURRENT".into(),
        ));
    }
    let expected_next = current
        .generation
        .checked_add(1)
        .ok_or_else(|| SearchError::InvalidArgument("generation overflow".into()))?;
    if plan.next_generation != expected_next {
        return Err(SearchError::InvalidArgument(
            "incremental plan next generation is invalid".into(),
        ));
    }

    validate_upserts(&plan.upserts)?;
    let delta_name = format!("delta-g{:016}", plan.next_generation);
    let delta_relative = format!("components/{delta_name}");
    let delta_path = root.join(&delta_relative);
    if delta_path.exists() {
        return Err(SearchError::InvalidArgument(format!(
            "delta component already exists: {}",
            delta_path.display()
        )));
    }
    let temp_path = root.join("components").join(format!(
        ".{delta_name}-{}-{}.tmp",
        std::process::id(),
        unique_nonce()?
    ));

    let mut upserts = plan.upserts.clone();
    upserts.sort_by_key(|item| item.logical_id);
    let inputs = upserts
        .iter()
        .map(|item| item.document.clone())
        .collect::<Vec<_>>();
    let build = if acceleration == AccelerationProfile::None {
        build_index(&inputs, &temp_path, options)?
    } else {
        build_index_unified(&inputs, &temp_path, options, acceleration)?
    };
    let logical_documents = upserts
        .iter()
        .map(|item| LogicalDocument::new(item.logical_id, item.document.clone()))
        .collect::<Vec<_>>();
    write_doc_map(&temp_path.join("logical-map.bin"), &logical_documents)?;
    write_tombstones(&temp_path.join("tombstones.bin"), &plan.tombstones)?;
    verify_index(&temp_path)?;
    sync_directory(&temp_path)?;
    fs::rename(&temp_path, &delta_path)?;
    sync_directory(&root.join("components"))?;

    let (_, current_relative) = read_current(root)?;
    let mut manifest = read_generation_manifest(&root.join(current_relative))?;
    manifest.generation = plan.next_generation;
    manifest.sources.push(SourceDescriptor {
        kind: SourceKind::Delta,
        generation: plan.next_generation,
        index_dir: delta_relative,
        map_file: "logical-map.bin".into(),
        tombstone_file: Some("tombstones.bin".into()),
    });
    let manifest_relative = generation_manifest_relative(plan.next_generation, "delta");
    publish_generation_manifest(root, &manifest_relative, &manifest)?;
    // CURRENT is the only visibility switch. Everything above may safely remain orphaned after a crash.
    publish_current(root, plan.next_generation, &manifest_relative)?;

    let published = MergedIndex::open(root, false)?;
    if published.live_docs() != plan.live_docs_after {
        return Err(SearchError::Format(
            "published generation live-doc count mismatch".into(),
        ));
    }
    Ok(GenerationReport {
        generation: plan.next_generation,
        live_docs: published.live_docs(),
        delta_count: published.delta_count(),
        build: Some(build),
        compacted: false,
    })
}

pub fn compact_generation(
    root: impl AsRef<Path>,
    options: &BuildOptions,
) -> Result<GenerationReport> {
    compact_generation_profile(root.as_ref(), options, AccelerationProfile::None)
}

pub fn compact_generation_unified(
    root: impl AsRef<Path>,
    options: &BuildOptions,
) -> Result<GenerationReport> {
    compact_generation_profile(root.as_ref(), options, AccelerationProfile::Balanced)
}

fn compact_generation_profile(
    root: &Path,
    options: &BuildOptions,
    acceleration: AccelerationProfile,
) -> Result<GenerationReport> {
    let current = MergedIndex::open(root, true)?;
    if current.delta_count() == 0 {
        return Ok(GenerationReport {
            generation: current.generation(),
            live_docs: current.live_docs(),
            delta_count: 0,
            build: None,
            compacted: false,
        });
    }
    let nonce = unique_nonce()?;
    let base_name = format!("base-g{:016}-c{nonce:016x}", current.generation());
    let base_relative = format!("components/{base_name}");
    let base_path = root.join(&base_relative);
    let temp_path = root.join("components").join(format!(
        ".{base_name}-{}-{}.tmp",
        std::process::id(),
        unique_nonce()?
    ));
    let (build, identities, live_docs) = if acceleration == AccelerationProfile::None {
        let documents = current.live_documents()?;
        let inputs = documents
            .iter()
            .map(|item| item.document.clone())
            .collect::<Vec<_>>();
        let build = build_index(&inputs, &temp_path, options)?;
        let identities = documents
            .iter()
            .map(|item| {
                LogicalDocumentIdentity::new(
                    item.logical_id,
                    item.document.key.clone(),
                    item.document.display_path.clone(),
                )
            })
            .collect::<Vec<_>>();
        (build, identities, documents.len())
    } else {
        let mut assembler = UnifiedIndexAssembler::new(&temp_path, options, acceleration, true)?;
        let mut identities = Vec::with_capacity(current.live_docs());
        current.visit_live_documents(|item| {
            identities.push(LogicalDocumentIdentity::new(
                item.logical_id,
                item.document.key.clone(),
                item.document.display_path.clone(),
            ));
            assembler.push(item.document)
        })?;
        let live_docs = identities.len();
        let build = assembler.finish()?;
        (build, identities, live_docs)
    };
    write_doc_map_identities(&temp_path.join("logical-map.bin"), &identities)?;
    verify_index(&temp_path)?;
    sync_directory(&temp_path)?;
    fs::rename(&temp_path, &base_path)?;
    sync_directory(&root.join("components"))?;

    let manifest = GenerationManifest {
        generation: current.generation(),
        sources: vec![SourceDescriptor {
            kind: SourceKind::Base,
            generation: current.generation(),
            index_dir: base_relative,
            map_file: "logical-map.bin".into(),
            tombstone_file: None,
        }],
    };
    let manifest_relative =
        generation_manifest_relative(current.generation(), &format!("c{nonce:016x}"));
    publish_generation_manifest(root, &manifest_relative, &manifest)?;
    publish_current(root, current.generation(), &manifest_relative)?;
    Ok(GenerationReport {
        generation: current.generation(),
        live_docs,
        delta_count: 0,
        build: Some(build),
        compacted: true,
    })
}

pub fn verify_generation(root: impl AsRef<Path>) -> Result<()> {
    let _ = MergedIndex::open(root, true)?;
    Ok(())
}

/// Verify generation metadata and logical mapping without re-hashing already verified base
/// segment payloads. This is intended for the immediate post-publication check after adopting a
/// `VerifiedBuiltIndex`; `verify_generation` remains the full checksum verification API.
pub fn verify_generation_structure(root: impl AsRef<Path>) -> Result<()> {
    let _ = MergedIndex::open(root, false)?;
    Ok(())
}

fn validate_logical_documents(documents: &[LogicalDocument]) -> Result<()> {
    let mut ids = HashSet::with_capacity(documents.len());
    let mut keys = HashSet::with_capacity(documents.len());
    for item in documents {
        if item.logical_id == 0 || !ids.insert(item.logical_id) {
            return Err(SearchError::InvalidArgument(
                "logical ids must be unique and non-zero".into(),
            ));
        }
        if item.document.key.is_empty() || !keys.insert(item.document.key.as_str()) {
            return Err(SearchError::InvalidArgument(
                "document keys must be unique and non-empty".into(),
            ));
        }
    }
    Ok(())
}

fn validate_logical_identities(documents: &[LogicalDocumentIdentity]) -> Result<()> {
    let mut ids = HashSet::with_capacity(documents.len());
    let mut keys = HashSet::with_capacity(documents.len());
    let mut previous = None;
    for item in documents {
        if item.logical_id == 0 || !ids.insert(item.logical_id) {
            return Err(SearchError::InvalidArgument(
                "logical ids must be unique and non-zero".into(),
            ));
        }
        if previous.is_some_and(|value| item.logical_id <= value) {
            return Err(SearchError::InvalidArgument(
                "adopted logical ids must be strictly increasing in physical document order".into(),
            ));
        }
        previous = Some(item.logical_id);
        if item.key.is_empty() || !keys.insert(item.key.as_str()) {
            return Err(SearchError::InvalidArgument(
                "document keys must be unique and non-empty".into(),
            ));
        }
    }
    Ok(())
}

fn validate_upserts(upserts: &[PlannedUpsert]) -> Result<()> {
    let documents = upserts
        .iter()
        .map(|item| LogicalDocument::new(item.logical_id, item.document.clone()))
        .collect::<Vec<_>>();
    validate_logical_documents(&documents)
}

fn generation_manifest_relative(generation: Generation, suffix: &str) -> String {
    format!("generations/gen-{generation:016}-{suffix}.txt")
}

fn write_doc_map(path: &Path, documents: &[LogicalDocument]) -> Result<()> {
    write_doc_map_entries(
        path,
        documents.len(),
        documents.iter().map(|item| {
            (
                item.logical_id,
                item.document.key.as_str(),
                item.document.display_path.as_str(),
            )
        }),
    )
}

fn write_doc_map_identities(path: &Path, documents: &[LogicalDocumentIdentity]) -> Result<()> {
    write_doc_map_entries(
        path,
        documents.len(),
        documents.iter().map(|item| {
            (
                item.logical_id,
                item.key.as_str(),
                item.display_path.as_str(),
            )
        }),
    )
}

fn write_doc_map_entries<'a, I>(path: &Path, count: usize, documents: I) -> Result<()>
where
    I: IntoIterator<Item = (LogicalDocId, &'a str, &'a str)>,
{
    let mut bytes = Vec::new();
    bytes.extend_from_slice(DOCMAP_MAGIC);
    bytes.extend_from_slice(&(count as u64).to_le_bytes());
    for (logical_id, key_text, display_text) in documents {
        let key = key_text.as_bytes();
        let display = display_text.as_bytes();
        let key_len = u32::try_from(key.len())
            .map_err(|_| SearchError::InvalidArgument("document key too long".into()))?;
        let display_len = u32::try_from(display.len())
            .map_err(|_| SearchError::InvalidArgument("display path too long".into()))?;
        bytes.extend_from_slice(&logical_id.to_le_bytes());
        bytes.extend_from_slice(&key_len.to_le_bytes());
        bytes.extend_from_slice(&display_len.to_le_bytes());
        bytes.extend_from_slice(key);
        bytes.extend_from_slice(display);
    }
    let checksum = fnv1a(&bytes);
    bytes.extend_from_slice(&checksum.to_le_bytes());
    write_durable_file(path, &bytes)
}

fn write_tombstones(path: &Path, ids: &[LogicalDocId]) -> Result<()> {
    let mut sorted = ids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    if sorted.contains(&0) {
        return Err(SearchError::InvalidArgument(
            "tombstone logical id must be non-zero".into(),
        ));
    }
    let mut bytes = Vec::with_capacity(16 + sorted.len() * 8 + SIDECAR_FOOTER_BYTES);
    bytes.extend_from_slice(TOMBSTONE_MAGIC);
    bytes.extend_from_slice(&(sorted.len() as u64).to_le_bytes());
    for id in sorted {
        bytes.extend_from_slice(&id.to_le_bytes());
    }
    let checksum = fnv1a(&bytes);
    bytes.extend_from_slice(&checksum.to_le_bytes());
    write_durable_file(path, &bytes)
}

fn read_tombstones(path: &Path) -> Result<Vec<LogicalDocId>> {
    let bytes = read_sidecar(path, TOMBSTONE_MAGIC)?;
    if bytes.len() < 16 {
        return Err(SearchError::Format("tombstone file too small".into()));
    }
    let count = u64::from_le_bytes(bytes[8..16].try_into().expect("fixed slice"));
    let count = usize::try_from(count)
        .map_err(|_| SearchError::Format("tombstone count too large".into()))?;
    let payload_end = bytes.len() - SIDECAR_FOOTER_BYTES;
    let expected = 16usize
        .checked_add(
            count
                .checked_mul(8)
                .ok_or_else(|| SearchError::Format("tombstone size overflow".into()))?,
        )
        .ok_or_else(|| SearchError::Format("tombstone size overflow".into()))?;
    if payload_end != expected {
        return Err(SearchError::Format("bad tombstone file size".into()));
    }
    let mut out = Vec::with_capacity(count);
    let mut previous = None;
    for index in 0..count {
        let off = 16 + index * 8;
        let id = u64::from_le_bytes(bytes[off..off + 8].try_into().expect("fixed slice"));
        if id == 0 || previous.is_some_and(|prev| id <= prev) {
            return Err(SearchError::Format(
                "tombstones must be sorted unique non-zero ids".into(),
            ));
        }
        previous = Some(id);
        out.push(id);
    }
    Ok(out)
}

fn read_sidecar(path: &Path, magic: &[u8; 8]) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?.read_to_end(&mut bytes)?;
    if bytes.len() < 8 + SIDECAR_FOOTER_BYTES || bytes.get(..8) != Some(magic.as_slice()) {
        return Err(SearchError::Format(format!(
            "bad sidecar magic: {}",
            path.display()
        )));
    }
    let payload_end = bytes.len() - SIDECAR_FOOTER_BYTES;
    let expected = u64::from_le_bytes(
        bytes[payload_end..]
            .try_into()
            .expect("fixed checksum slice"),
    );
    if fnv1a(&bytes[..payload_end]) != expected {
        return Err(SearchError::Format(format!(
            "sidecar checksum mismatch: {}",
            path.display()
        )));
    }
    Ok(bytes)
}

fn write_generation_manifest(path: &Path, manifest: &GenerationManifest) -> Result<()> {
    let mut text = format!(
        "{GENERATION_MAGIC}\ngeneration {}\nsources {}\n",
        manifest.generation,
        manifest.sources.len()
    );
    for source in &manifest.sources {
        let tombstone = source.tombstone_file.as_deref().unwrap_or("-");
        text.push_str(&format!(
            "source {} {} {} {} {}\n",
            source.kind.as_str(),
            source.generation,
            source.index_dir,
            source.map_file,
            tombstone
        ));
    }
    write_durable_file(path, text.as_bytes())
}

fn read_generation_manifest(path: &Path) -> Result<GenerationManifest> {
    let text = fs::read_to_string(path)?;
    let mut lines = text.lines();
    if lines.next() != Some(GENERATION_MAGIC) {
        return Err(SearchError::Format("bad generation manifest magic".into()));
    }
    let generation = parse_named_u64(lines.next(), "generation")?;
    let source_count = parse_named_usize(lines.next(), "sources")?;
    let mut sources = Vec::with_capacity(source_count);
    for _ in 0..source_count {
        let line = lines
            .next()
            .ok_or_else(|| SearchError::Format("missing generation source".into()))?;
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 6 || fields[0] != "source" {
            return Err(SearchError::Format("bad generation source line".into()));
        }
        let kind = SourceKind::parse(fields[1])?;
        let source_generation = fields[2]
            .parse::<Generation>()
            .map_err(|_| SearchError::Format("bad source generation".into()))?;
        validate_relative_path(fields[3])?;
        validate_relative_path(fields[4])?;
        let tombstone_file = if fields[5] == "-" {
            None
        } else {
            validate_relative_path(fields[5])?;
            Some(fields[5].to_owned())
        };
        sources.push(SourceDescriptor {
            kind,
            generation: source_generation,
            index_dir: fields[3].to_owned(),
            map_file: fields[4].to_owned(),
            tombstone_file,
        });
    }
    if lines.any(|line| !line.trim().is_empty()) {
        return Err(SearchError::Format(
            "generation manifest has trailing lines".into(),
        ));
    }
    if sources.len() != source_count {
        return Err(SearchError::Format(
            "generation source count mismatch".into(),
        ));
    }
    Ok(GenerationManifest {
        generation,
        sources,
    })
}

fn read_current(root: &Path) -> Result<(Generation, String)> {
    let text = fs::read_to_string(root.join("CURRENT"))?;
    let mut lines = text.lines();
    if lines.next() != Some(CURRENT_MAGIC) {
        return Err(SearchError::Format("bad CURRENT magic".into()));
    }
    let generation = parse_named_u64(lines.next(), "generation")?;
    let manifest_line = lines
        .next()
        .ok_or_else(|| SearchError::Format("CURRENT manifest missing".into()))?;
    let mut fields = manifest_line.split_whitespace();
    if fields.next() != Some("manifest") {
        return Err(SearchError::Format("bad CURRENT manifest line".into()));
    }
    let relative = fields
        .next()
        .ok_or_else(|| SearchError::Format("CURRENT manifest path missing".into()))?;
    if fields.next().is_some() {
        return Err(SearchError::Format("bad CURRENT manifest line".into()));
    }
    validate_relative_path(relative)?;
    if lines.any(|line| !line.trim().is_empty()) {
        return Err(SearchError::Format("CURRENT has trailing lines".into()));
    }
    Ok((generation, relative.to_owned()))
}

fn publish_generation_manifest(
    root: &Path,
    relative: &str,
    manifest: &GenerationManifest,
) -> Result<()> {
    validate_relative_path(relative)?;
    let final_path = root.join(relative);
    let parent = final_path
        .parent()
        .ok_or_else(|| SearchError::Format("generation manifest parent missing".into()))?;
    fs::create_dir_all(parent)?;
    let temp_path = parent.join(format!(
        ".{}.{}.tmp",
        final_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("generation"),
        std::process::id()
    ));
    write_generation_manifest(&temp_path, manifest)?;
    fs::rename(&temp_path, &final_path)?;
    sync_directory(parent)?;
    Ok(())
}

fn publish_current(root: &Path, generation: Generation, manifest_relative: &str) -> Result<()> {
    validate_relative_path(manifest_relative)?;
    let text = format!("{CURRENT_MAGIC}\ngeneration {generation}\nmanifest {manifest_relative}\n");
    let temp = root.join(format!(".CURRENT.{}.tmp", std::process::id()));
    write_durable_file(&temp, text.as_bytes())?;
    fs::rename(&temp, root.join("CURRENT"))?;
    sync_directory(root)?;
    Ok(())
}

fn write_durable_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
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

fn parse_named_u64(line: Option<&str>, name: &str) -> Result<u64> {
    let line = line.ok_or_else(|| SearchError::Format(format!("missing {name} line")))?;
    let mut fields = line.split_whitespace();
    if fields.next() != Some(name) {
        return Err(SearchError::Format(format!("bad {name} line")));
    }
    let value = fields
        .next()
        .ok_or_else(|| SearchError::Format(format!("missing {name} value")))?
        .parse()
        .map_err(|_| SearchError::Format(format!("invalid {name}")))?;
    if fields.next().is_some() {
        return Err(SearchError::Format(format!("bad {name} line")));
    }
    Ok(value)
}

fn parse_named_usize(line: Option<&str>, name: &str) -> Result<usize> {
    let value = parse_named_u64(line, name)?;
    usize::try_from(value).map_err(|_| SearchError::Format(format!("{name} too large")))
}

fn validate_relative_path(value: &str) -> Result<()> {
    let path = Path::new(value);
    if value.is_empty() || path.is_absolute() {
        return Err(SearchError::Format("unsafe generation path".into()));
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(SearchError::Format("unsafe generation path".into()));
        }
    }
    Ok(())
}

fn directory_bytes_recursive(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            total = total
                .checked_add(directory_bytes_recursive(&entry.path())?)
                .ok_or_else(|| SearchError::Format("directory byte count overflow".into()))?;
        } else if metadata.is_file() {
            total = total
                .checked_add(metadata.len())
                .ok_or_else(|| SearchError::Format("directory byte count overflow".into()))?;
        }
    }
    Ok(total)
}

fn safe_join(root: &Path, relative: &str) -> Result<PathBuf> {
    validate_relative_path(relative)?;
    Ok(root.join(relative))
}

fn unique_nonce() -> Result<u128> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .map_err(|_| SearchError::Format("system clock before UNIX epoch".into()))
}
