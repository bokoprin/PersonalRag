use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::AtomicBool,
        mpsc::{self, SyncSender, TrySendError},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use personalrag_portable_search::{
    apply_update_plan, build_disk_path_inputs_index_unified,
    build_disk_path_inputs_index_unified_retained, compact_generation_unified,
    compact_vnext_generation_store, fold_ascii, gc_vnext_generation_store,
    initialize_generation_from_built_index, initialize_vnext_generation_store,
    open_vnext_published_generation, plan_incremental_update, publish_incremental_update_unified,
    publish_vnext_incremental_generation, recommend_system_build_tuning, verify_generation,
    verify_index, verify_positional2_sidecars, verify_positional3_sidecars,
    verify_positional_sidecars, verify_vnext_generation_store, AccelerationProfile, BuildMode,
    BuildOptions, ChangeBatch, ChangeKind, DiskPathBuildConfig, DiskPathInput, DocumentChange,
    DocumentInput, IncrementalPolicy, LogicalDocumentIdentity, MergedIndex, MergedSearchSession,
    PosCodec, VNextDocumentInput, VNextGenerationIndex,
};

use crate::{
    capture_usn_checkpoint,
    contract_v1::{SearchHit, SearchRequest, SnippetBatchResult, SnippetHit, SnippetRequest},
    diff_catalog, office_open_xml_eligible, scan_files, scan_usn_changes,
    search_catalog_with_generation_metadata_reader, search_catalog_with_generation_session_reader,
    search_catalog_with_vnext_generation_reader, snippets, snippets_from_text,
    DirectoryTrackingSnapshot, ExtractionBudget, ExtractorRegistry, GenerationSearchCatalog,
    IncrementalCatalogState, OfficeExtractionConfig, OfficeExtractionRequest,
    OfficeExtractionService, OfficePreparedContent, PreparedContent, ScanExclusions, ScanProgress,
    ScanReport, ScannedFile, ScannerMode, SearchOptions, UsnCheckpoint, UsnScanResult,
};

const MIB: u64 = 1024 * 1024;
const MAX_INCREMENTAL_CHANGED_FILES: usize = 100_000;
const MIN_COMPACTION_FALLBACK_CHANGES: usize = 1_024;
const VNEXT_STORE_DIR: &str = "vnext-store";
const VNEXT_SEGMENT_DOCS: usize = 5_000;
const SHADOW_QUEUE_CAPACITY: usize = 16;
const PRODUCTION_ACCELERATION_PROFILE: AccelerationProfile = AccelerationProfile::Balanced;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductionBackendMode {
    Perf12,
    Shadow,
    VNext,
}

impl ProductionBackendMode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Perf12 => "perf12",
            Self::Shadow => "shadow",
            Self::VNext => "vnext",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "perf12" => Ok(Self::Perf12),
            "shadow" => Ok(Self::Shadow),
            "vnext" => Ok(Self::VNext),
            _ => Err("search core backend must be one of: perf12, shadow, vnext".to_owned()),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProductionBackendTelemetry {
    pub searches: u64,
    pub perf12_searches: u64,
    pub vnext_searches: u64,
    pub vnext_fallbacks: u64,
    pub shadow_comparisons: u64,
    pub shadow_mismatches: u64,
    pub shadow_queued: u64,
    pub shadow_coalesced: u64,
    pub shadow_dropped: u64,
    pub shadow_failures: u64,
    pub common_result_searches: u64,
    pub common_result_total_micros: u64,
    pub common_result_max_micros: u64,
    pub last_search_micros: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ShadowCompareKey {
    index_dir: PathBuf,
    generation: u64,
    file_query: Option<String>,
    content_query: Option<String>,
}

#[derive(Debug)]
struct ShadowCompareJob {
    key: ShadowCompareKey,
}

struct ShadowCompareExecutor {
    sender: SyncSender<ShadowCompareJob>,
    pending: Arc<Mutex<HashSet<ShadowCompareKey>>>,
}

impl ShadowCompareExecutor {
    fn new(telemetry: Arc<Mutex<ProductionBackendTelemetry>>) -> Self {
        let (sender, receiver) = mpsc::sync_channel::<ShadowCompareJob>(SHADOW_QUEUE_CAPACITY);
        let pending = Arc::new(Mutex::new(HashSet::<ShadowCompareKey>::new()));
        let worker_pending = Arc::clone(&pending);
        thread::Builder::new()
            .name("personalrag-vnext-shadow".to_owned())
            .spawn(move || {
                let mut cached_dir = None::<PathBuf>;
                let mut cached_generation = None::<u64>;
                let mut perf = None::<Arc<MergedSearchSession>>;
                let mut vnext = None::<Arc<VNextGenerationIndex>>;
                while let Ok(job) = receiver.recv() {
                    let cache_matches = cached_dir.as_ref() == Some(&job.key.index_dir)
                        && cached_generation == Some(job.key.generation);
                    if !cache_matches {
                        cached_dir = Some(job.key.index_dir.clone());
                        cached_generation = Some(job.key.generation);
                        perf = MergedSearchSession::open(&job.key.index_dir, false, 4)
                            .ok()
                            .filter(|session| session.index().generation() == job.key.generation)
                            .map(Arc::new);
                        vnext =
                            open_vnext_published_generation(vnext_store_dir(&job.key.index_dir))
                                .ok()
                                .filter(|index| index.generation() == job.key.generation)
                                .map(Arc::new);
                    }

                    let result = perf.as_deref().zip(vnext.as_deref()).ok_or(()).and_then(
                        |(perf, vnext)| {
                            let perf_hits = shadow_raw_hits_perf(
                                perf,
                                job.key.file_query.as_deref(),
                                job.key.content_query.as_deref(),
                            )
                            .map_err(|_| ())?;
                            let vnext_hits = shadow_raw_hits_vnext(
                                vnext,
                                job.key.file_query.as_deref(),
                                job.key.content_query.as_deref(),
                            )
                            .map_err(|_| ())?;
                            Ok(perf_hits != vnext_hits)
                        },
                    );
                    if let Ok(mut telemetry) = telemetry.lock() {
                        match result {
                            Ok(mismatch) => {
                                telemetry.shadow_comparisons =
                                    telemetry.shadow_comparisons.saturating_add(1);
                                if mismatch {
                                    telemetry.shadow_mismatches =
                                        telemetry.shadow_mismatches.saturating_add(1);
                                }
                            }
                            Err(()) => {
                                telemetry.shadow_failures =
                                    telemetry.shadow_failures.saturating_add(1);
                            }
                        }
                    }
                    if let Ok(mut pending) = worker_pending.lock() {
                        pending.remove(&job.key);
                    }
                }
            })
            .expect("failed to create vNext shadow comparison worker");
        Self { sender, pending }
    }

    fn try_submit(&self, job: ShadowCompareJob) -> Result<bool, TrySendError<ShadowCompareJob>> {
        let key = job.key.clone();
        if let Ok(mut pending) = self.pending.lock() {
            if !pending.insert(key.clone()) {
                return Ok(false);
            }
        }
        match self.sender.try_send(job) {
            Ok(()) => Ok(true),
            Err(error) => {
                if let Ok(mut pending) = self.pending.lock() {
                    pending.remove(&key);
                }
                Err(error)
            }
        }
    }
}

fn intersect_shadow_hits(left: Vec<u64>, right: Vec<u64>) -> Vec<u64> {
    let mut out = Vec::with_capacity(left.len().min(right.len()));
    let (mut l, mut r) = (0usize, 0usize);
    while l < left.len() && r < right.len() {
        match left[l].cmp(&right[r]) {
            std::cmp::Ordering::Less => l += 1,
            std::cmp::Ordering::Greater => r += 1,
            std::cmp::Ordering::Equal => {
                out.push(left[l]);
                l += 1;
                r += 1;
            }
        }
    }
    out
}

fn shadow_raw_hits_perf(
    index: &MergedSearchSession,
    file_query: Option<&str>,
    content_query: Option<&str>,
) -> Result<Vec<u64>, String> {
    match (file_query, content_query) {
        (Some(file), Some(content)) => Ok(intersect_shadow_hits(
            index
                .search_name(file.as_bytes())
                .map_err(|error| error.to_string())?,
            index
                .search_content(content.as_bytes())
                .map_err(|error| error.to_string())?,
        )),
        (Some(file), None) => index
            .search_name(file.as_bytes())
            .map_err(|error| error.to_string()),
        (None, Some(content)) => index
            .search_content(content.as_bytes())
            .map_err(|error| error.to_string()),
        (None, None) => Ok(Vec::new()),
    }
}

fn shadow_raw_hits_vnext(
    index: &VNextGenerationIndex,
    file_query: Option<&str>,
    content_query: Option<&str>,
) -> Result<Vec<u64>, String> {
    match (file_query, content_query) {
        (Some(file), Some(content)) => Ok(intersect_shadow_hits(
            index
                .search_name(file.as_bytes())
                .map_err(|error| error.to_string())?,
            index
                .search_content(content.as_bytes())
                .map_err(|error| error.to_string())?,
        )),
        (Some(file), None) => index
            .search_name(file.as_bytes())
            .map_err(|error| error.to_string()),
        (None, Some(content)) => index
            .search_content(content.as_bytes())
            .map_err(|error| error.to_string()),
        (None, None) => Ok(Vec::new()),
    }
}

fn vnext_store_dir(index_dir: &Path) -> PathBuf {
    index_dir.join(VNEXT_STORE_DIR)
}

fn hydration_batch_budget(memory_budget_bytes: u64) -> u64 {
    (memory_budget_bytes / 8).clamp(32 * MIB, 128 * MIB)
}

fn hydration_workers_for(
    windows: bool,
    logical_cpus: usize,
    content_files: usize,
    content_bytes: u64,
    fallback: usize,
) -> usize {
    if !windows || content_files == 0 {
        return fallback.max(1);
    }
    let average = content_bytes / content_files as u64;
    let cap = if average <= 64 * 1024 {
        8
    } else if average <= MIB {
        4
    } else {
        2
    };
    logical_cpus.max(1).min(cap)
}

pub struct SearchCatalogView<'a> {
    pub root: &'a Path,
    pub paths: &'a [String],
    pub size_bytes: &'a [u64],
    pub modified_ns: &'a [u64],
    pub logical_ids: &'a [u64],
    pub logical_to_row: &'a [u32],
    pub generation: u64,
    pub max_file_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexBuildPhase {
    Preparing,
    Building,
    Q2,
    Pos1,
    Pos23,
    Verifying,
}

impl IndexBuildPhase {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Building => "building",
            Self::Q2 => "q2",
            Self::Pos1 => "pos1",
            Self::Pos23 => "pos23",
            Self::Verifying => "verifying",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct IndexBuildProgress {
    pub phase: Option<IndexBuildPhase>,
    pub source_files: usize,
    pub processed_files: usize,
    pub indexed_files: usize,
    pub skipped_files: usize,
    pub bytes_read: u64,
    pub prepared_bytes: u64,
    pub current_path: Option<PathBuf>,
}

#[derive(Debug)]
pub struct IndexBuildOutcome {
    pub paths: Vec<String>,
    pub logical_ids: Vec<u64>,
    pub generation: u64,
    pub next_logical_id: u64,
    pub size_bytes: Vec<u64>,
    pub modified_ns: Vec<u64>,
    pub source_files: usize,
    pub processed_files: usize,
    pub indexed_files: usize,
    pub skipped_files: usize,
    pub bytes_read: u64,
}

#[derive(Debug)]
pub enum IncrementalSyncResult {
    Applied(IndexBuildOutcome),
    Unchanged(IndexBuildOutcome),
    FullRebuildRequired { changed_files: usize },
}

pub struct IncrementalSyncRequest<'a> {
    pub root: &'a Path,
    pub files: &'a [ScannedFile],
    pub index_dir: &'a Path,
    pub previous: IncrementalCatalogState,
    pub max_file_bytes: u64,
}

pub struct IncrementalChangeSyncRequest<'a> {
    pub root: &'a Path,
    pub upserts: &'a [ScannedFile],
    pub deleted_paths: &'a [String],
    pub index_dir: &'a Path,
    pub previous: IncrementalCatalogState,
    pub max_file_bytes: u64,
}

pub trait SearchEngine: Send + Sync {
    fn search(
        &self,
        index_dir: &Path,
        catalog: SearchCatalogView<'_>,
        request: SearchRequest,
        backend: &str,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<SearchHit>, String>;

    fn snippets(&self, request: &SnippetRequest) -> Result<Vec<SnippetHit>, String>;

    fn snippets_batch(&self, request: &[SnippetRequest])
        -> Result<Vec<SnippetBatchResult>, String>;
}

pub trait IndexEngine: Send + Sync {
    fn scan(
        &self,
        root: &Path,
        max_file_bytes: u64,
        scanner_mode: &str,
        exclusions: &crate::contract_v1::ExclusionConfig,
        cancel: Arc<AtomicBool>,
        on_progress: Arc<dyn Fn(ScanProgress) + Send + Sync>,
    ) -> Result<ScanReport, String>;

    fn capture_change_checkpoint(&self, root: &Path) -> Result<Option<UsnCheckpoint>, String>;

    fn scan_changes(
        &self,
        root: &Path,
        checkpoint: UsnCheckpoint,
        directories: &DirectoryTrackingSnapshot,
        max_file_bytes: u64,
        exclusions: &crate::contract_v1::ExclusionConfig,
    ) -> Result<UsnScanResult, String>;

    fn build(
        &self,
        root: &Path,
        files: Vec<ScannedFile>,
        build_dir: &Path,
        max_file_bytes: u64,
        cancel: &AtomicBool,
        on_progress: &mut dyn FnMut(IndexBuildProgress),
    ) -> Result<IndexBuildOutcome, String>;

    fn sync_incremental(
        &self,
        request: IncrementalSyncRequest<'_>,
        cancel: &AtomicBool,
        on_progress: &mut dyn FnMut(IndexBuildProgress),
    ) -> Result<IncrementalSyncResult, String>;

    fn sync_incremental_changes(
        &self,
        request: IncrementalChangeSyncRequest<'_>,
        cancel: &AtomicBool,
        on_progress: &mut dyn FnMut(IndexBuildProgress),
    ) -> Result<IncrementalSyncResult, String>;
}

struct CachedMergedSearchSession {
    index_dir: PathBuf,
    generation: u64,
    session: Arc<MergedSearchSession>,
}

struct CachedVNextGeneration {
    index_dir: PathBuf,
    generation: u64,
    index: Arc<VNextGenerationIndex>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SortOrderKey {
    field: String,
    descending: bool,
}

#[derive(Debug)]
struct SearchSortOrder {
    logical_ids: Vec<u64>,
    rank_by_row: Vec<u32>,
}

#[derive(Debug, Default)]
struct CachedSortOrders {
    generation: u64,
    doc_count: usize,
    orders: HashMap<SortOrderKey, Arc<SearchSortOrder>>,
}

pub struct PortableEngine {
    merged_search: Mutex<Option<CachedMergedSearchSession>>,
    vnext_search: Mutex<Option<CachedVNextGeneration>>,
    sort_orders: Mutex<CachedSortOrders>,
    production_backend: Mutex<ProductionBackendMode>,
    telemetry: Arc<Mutex<ProductionBackendTelemetry>>,
    shadow_executor: ShadowCompareExecutor,
}

impl Default for PortableEngine {
    fn default() -> Self {
        Self::with_production_backend(ProductionBackendMode::Perf12)
    }
}

impl PortableEngine {
    #[must_use]
    pub fn with_production_backend(mode: ProductionBackendMode) -> Self {
        let telemetry = Arc::new(Mutex::new(ProductionBackendTelemetry::default()));
        Self {
            merged_search: Mutex::new(None),
            vnext_search: Mutex::new(None),
            sort_orders: Mutex::new(CachedSortOrders::default()),
            production_backend: Mutex::new(mode),
            shadow_executor: ShadowCompareExecutor::new(Arc::clone(&telemetry)),
            telemetry,
        }
    }

    pub fn set_production_backend(&self, mode: ProductionBackendMode) -> Result<(), String> {
        *self
            .production_backend
            .lock()
            .map_err(|_| "production backend lock poisoned".to_owned())? = mode;
        self.invalidate_search_cache()
    }

    pub fn production_backend(&self) -> Result<ProductionBackendMode, String> {
        self.production_backend
            .lock()
            .map(|mode| *mode)
            .map_err(|_| "production backend lock poisoned".to_owned())
    }

    pub fn production_backend_telemetry(&self) -> Result<ProductionBackendTelemetry, String> {
        self.telemetry
            .lock()
            .map(|telemetry| telemetry.clone())
            .map_err(|_| "production backend telemetry lock poisoned".to_owned())
    }

    pub fn vnext_ready(&self, index_dir: &Path, generation: u64) -> bool {
        self.vnext_generation_index(index_dir, generation).is_ok()
    }

    pub fn active_production_backend(
        &self,
        index_dir: &Path,
        generation: u64,
    ) -> Result<&'static str, String> {
        Ok(match self.production_backend()? {
            ProductionBackendMode::Perf12 | ProductionBackendMode::Shadow => "perf12",
            ProductionBackendMode::VNext if self.vnext_ready(index_dir, generation) => "vnext",
            ProductionBackendMode::VNext => "perf12",
        })
    }

    fn vnext_generation_index(
        &self,
        index_dir: &Path,
        generation: u64,
    ) -> Result<Arc<VNextGenerationIndex>, String> {
        let store = vnext_store_dir(index_dir);
        let mut cache = self
            .vnext_search
            .lock()
            .map_err(|_| "vNext search cache lock poisoned".to_owned())?;
        if let Some(cached) = cache.as_ref() {
            if cached.index_dir == store && cached.generation == generation {
                return Ok(Arc::clone(&cached.index));
            }
        }
        *cache = None;
        let index =
            Arc::new(open_vnext_published_generation(&store).map_err(|error| error.to_string())?);
        if index.generation() != generation {
            return Err("vNext generation and GUI catalog generations differ".to_owned());
        }
        *cache = Some(CachedVNextGeneration {
            index_dir: store,
            generation,
            index: Arc::clone(&index),
        });
        Ok(index)
    }

    fn merged_search_session(
        &self,
        index_dir: &Path,
        generation: u64,
    ) -> Result<Arc<MergedSearchSession>, String> {
        let mut cache = self
            .merged_search
            .lock()
            .map_err(|_| "merged search cache lock poisoned".to_owned())?;
        if let Some(cached) = cache.as_ref() {
            if cached.index_dir == index_dir && cached.generation == generation {
                return Ok(Arc::clone(&cached.session));
            }
        }
        *cache = None;
        let session = Arc::new(
            MergedSearchSession::open(index_dir, false, 4).map_err(|error| error.to_string())?,
        );
        if session.index().generation() != generation {
            return Err(
                "generation index and GUI catalog generations differ; rebuild index".to_owned(),
            );
        }
        *cache = Some(CachedMergedSearchSession {
            index_dir: index_dir.to_path_buf(),
            generation,
            session: Arc::clone(&session),
        });
        Ok(session)
    }

    pub fn invalidate_search_cache(&self) -> Result<(), String> {
        *self
            .merged_search
            .lock()
            .map_err(|_| "merged search cache lock poisoned".to_owned())? = None;
        *self
            .vnext_search
            .lock()
            .map_err(|_| "vNext search cache lock poisoned".to_owned())? = None;
        *self
            .sort_orders
            .lock()
            .map_err(|_| "sort-order cache lock poisoned".to_owned())? =
            CachedSortOrders::default();
        Ok(())
    }

    fn sort_order_for_search(
        &self,
        catalog: &SearchCatalogView<'_>,
        options: &SearchOptions,
    ) -> Result<Option<Arc<SearchSortOrder>>, String> {
        if options.sort_field == "path" && options.sort_direction != "descending" {
            return Ok(None);
        }
        if catalog.logical_ids.len() != catalog.paths.len() {
            return Ok(None);
        }
        if matches!(options.sort_field.as_str(), "size" | "modified")
            && (catalog.size_bytes.len() != catalog.paths.len()
                || catalog.modified_ns.len() != catalog.paths.len())
        {
            return Ok(None);
        }
        if !matches!(
            options.sort_field.as_str(),
            "path" | "name" | "size" | "modified" | "extension"
        ) {
            return Ok(None);
        }
        let key = SortOrderKey {
            field: options.sort_field.clone(),
            descending: options.sort_direction == "descending",
        };
        {
            let mut cache = self
                .sort_orders
                .lock()
                .map_err(|_| "sort-order cache lock poisoned".to_owned())?;
            if cache.generation != catalog.generation || cache.doc_count != catalog.paths.len() {
                cache.generation = catalog.generation;
                cache.doc_count = catalog.paths.len();
                cache.orders.clear();
            }
            if let Some(order) = cache.orders.get(&key) {
                return Ok(Some(Arc::clone(order)));
            }
        }

        let mut rows = (0..catalog.paths.len())
            .map(|row| u32::try_from(row).map_err(|_| "GUI row exceeds u32".to_owned()))
            .collect::<Result<Vec<_>, _>>()?;
        let descending = key.descending;
        rows.sort_unstable_by(|left, right| {
            let left_row = *left as usize;
            let right_row = *right as usize;
            let left_path = &catalog.paths[left_row];
            let right_path = &catalog.paths[right_row];
            let primary = match key.field.as_str() {
                "name" => Path::new(left_path)
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or(left_path)
                    .cmp(
                        Path::new(right_path)
                            .file_name()
                            .and_then(|value| value.to_str())
                            .unwrap_or(right_path),
                    ),
                "size" => catalog.size_bytes[left_row].cmp(&catalog.size_bytes[right_row]),
                "modified" => catalog.modified_ns[left_row].cmp(&catalog.modified_ns[right_row]),
                "extension" => Path::new(left_path)
                    .extension()
                    .and_then(|value| value.to_str())
                    .unwrap_or("")
                    .cmp(
                        Path::new(right_path)
                            .extension()
                            .and_then(|value| value.to_str())
                            .unwrap_or(""),
                    ),
                _ => left_path.cmp(right_path),
            };
            let primary = if descending {
                primary.reverse()
            } else {
                primary
            };
            primary.then_with(|| left_path.cmp(right_path))
        });
        let logical_ids = rows
            .iter()
            .map(|row| catalog.logical_ids[*row as usize])
            .collect::<Vec<_>>();
        let mut rank_by_row = vec![u32::MAX; rows.len()];
        for (rank, row) in rows.iter().copied().enumerate() {
            rank_by_row[row as usize] = rank as u32;
        }
        let built = Arc::new(SearchSortOrder {
            logical_ids,
            rank_by_row,
        });
        let mut cache = self
            .sort_orders
            .lock()
            .map_err(|_| "sort-order cache lock poisoned".to_owned())?;
        if cache.generation == catalog.generation && cache.doc_count == catalog.paths.len() {
            cache.orders.insert(key, Arc::clone(&built));
        }
        Ok(Some(built))
    }
}

impl SearchEngine for PortableEngine {
    fn search(
        &self,
        index_dir: &Path,
        catalog: SearchCatalogView<'_>,
        request: SearchRequest,
        backend: &str,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<SearchHit>, String> {
        let started = Instant::now();
        let options = SearchOptions {
            file_query: request.file_query,
            include_path: request.include_path,
            content_query: request.content_query,
            extensions: request.extensions,
            path_scope: request.path_scope,
            match_case: request.match_case,
            whole_words: request.whole_words,
            regex: request.regex,
            sort_field: request.sort_field,
            sort_direction: request.sort_direction,
            limit: request.limit,
            backend: backend.to_owned(),
        };
        let registry = ExtractorRegistry::new();
        let budget = ExtractionBudget::from_max_file_bytes(catalog.max_file_bytes);
        let office_service = office_extraction_service(index_dir, catalog.max_file_bytes);
        let sort_order = self.sort_order_for_search(&catalog, &options)?;
        let generation_catalog = GenerationSearchCatalog {
            root: catalog.root,
            paths: catalog.paths,
            size_bytes: catalog.size_bytes,
            modified_ns: catalog.modified_ns,
            logical_ids: catalog.logical_ids,
            logical_to_row: catalog.logical_to_row,
            generation: catalog.generation,
            first_n_logical_order: sort_order
                .as_deref()
                .map(|order| order.logical_ids.as_slice()),
            first_n_rank_by_row: sort_order
                .as_deref()
                .map(|order| order.rank_by_row.as_slice()),
        };

        let read_content = |path: &Path| {
            if office_open_xml_eligible(path) {
                office_service
                    .read_search_text(path)
                    .ok()
                    .map(|(text, _, _)| text)
            } else {
                registry.read_search_text(path, budget).ok().flatten()
            }
        };

        let run_perf12 = || {
            if index_dir.join("CURRENT").exists() {
                let session = self.merged_search_session(index_dir, catalog.generation)?;
                search_catalog_with_generation_session_reader(
                    index_dir,
                    generation_catalog,
                    &options,
                    &session,
                    read_content,
                    is_cancelled,
                )
            } else {
                search_catalog_with_generation_metadata_reader(
                    index_dir,
                    generation_catalog,
                    &options,
                    read_content,
                    is_cancelled,
                )
            }
        };
        let run_vnext = || -> Result<_, String> {
            let store = vnext_store_dir(index_dir);
            let index = self.vnext_generation_index(index_dir, catalog.generation)?;
            search_catalog_with_vnext_generation_reader(
                &store,
                generation_catalog,
                &options,
                &index,
                read_content,
                is_cancelled,
            )
        };

        let mode = self.production_backend()?;
        let (results, active_backend, fallback) = match mode {
            ProductionBackendMode::Perf12 => (run_perf12()?, "perf12", false),
            ProductionBackendMode::Shadow => {
                let perf = run_perf12()?;
                if !options.regex
                    && !options.match_case
                    && !options.whole_words
                    && (options.file_query.is_some() || options.content_query.is_some())
                {
                    let job = ShadowCompareJob {
                        key: ShadowCompareKey {
                            index_dir: index_dir.to_path_buf(),
                            generation: catalog.generation,
                            file_query: options.file_query.clone(),
                            content_query: options.content_query.clone(),
                        },
                    };
                    let submit = self.shadow_executor.try_submit(job);
                    let mut telemetry = self
                        .telemetry
                        .lock()
                        .map_err(|_| "production backend telemetry lock poisoned".to_owned())?;
                    match submit {
                        Ok(true) => {
                            telemetry.shadow_queued = telemetry.shadow_queued.saturating_add(1);
                        }
                        Ok(false) => {
                            telemetry.shadow_coalesced =
                                telemetry.shadow_coalesced.saturating_add(1);
                        }
                        Err(_) => {
                            telemetry.shadow_dropped = telemetry.shadow_dropped.saturating_add(1);
                        }
                    }
                }
                (perf, "perf12", false)
            }
            ProductionBackendMode::VNext => match run_vnext() {
                Ok(vnext) => (vnext, "vnext", false),
                Err(_) => (run_perf12()?, "perf12", true),
            },
        };

        {
            let mut telemetry = self
                .telemetry
                .lock()
                .map_err(|_| "production backend telemetry lock poisoned".to_owned())?;
            telemetry.searches = telemetry.searches.saturating_add(1);
            if active_backend == "vnext" {
                telemetry.vnext_searches = telemetry.vnext_searches.saturating_add(1);
            } else {
                telemetry.perf12_searches = telemetry.perf12_searches.saturating_add(1);
            }
            if fallback {
                telemetry.vnext_fallbacks = telemetry.vnext_fallbacks.saturating_add(1);
            }
            let elapsed_micros = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
            if results.len() >= 8_192 {
                telemetry.common_result_searches =
                    telemetry.common_result_searches.saturating_add(1);
                telemetry.common_result_total_micros = telemetry
                    .common_result_total_micros
                    .saturating_add(elapsed_micros);
                telemetry.common_result_max_micros =
                    telemetry.common_result_max_micros.max(elapsed_micros);
            }
            telemetry.last_search_micros = elapsed_micros;
        }

        Ok(results
            .into_iter()
            .map(|item| SearchHit {
                file_id: item.file_id,
                path: item.path,
                name: item.name,
                extension: item.extension,
                size_bytes: item.size_bytes,
                modified_ns: item.modified_ns,
                content_state: item.content_state,
            })
            .collect())
    }

    fn snippets(&self, request: &SnippetRequest) -> Result<Vec<SnippetHit>, String> {
        let registry = ExtractorRegistry::new();
        let source_bytes =
            std::fs::metadata(&request.path).map_or(1, |metadata| metadata.len().max(1));
        let budget = ExtractionBudget::from_max_file_bytes(source_bytes);
        let items = match registry.prepare(&request.path, budget) {
            Ok(PreparedContent::Extracted(document)) => snippets_from_text(
                &document.text,
                &request.query,
                request.context,
                request.max_hits,
                request.match_case,
                request.whole_words,
                request.regex,
            ),
            Ok(PreparedContent::NameOnly) => Ok(Vec::new()),
            Ok(PreparedContent::SourceFile) | Err(_) => snippets(
                &request.path,
                &request.query,
                request.context,
                request.max_hits,
                request.match_case,
                request.whole_words,
                request.regex,
            ),
        }?;
        Ok(items
            .into_iter()
            .map(|item| SnippetHit {
                line_number: item.line_number,
                before: item.before,
                hit_line: item.hit_line,
                after: item.after,
            })
            .collect())
    }

    fn snippets_batch(
        &self,
        requests: &[SnippetRequest],
    ) -> Result<Vec<SnippetBatchResult>, String> {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc,
        };

        if requests.is_empty() {
            return Ok(Vec::new());
        }
        let workers = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .clamp(1, 4)
            .min(requests.len());
        if workers == 1 {
            return requests
                .iter()
                .map(|request| {
                    self.snippets(request).map(|hits| SnippetBatchResult {
                        path: request.path.clone(),
                        hits,
                    })
                })
                .collect();
        }

        let next = AtomicUsize::new(0);
        let (sender, receiver) = mpsc::channel();
        std::thread::scope(|scope| {
            for _ in 0..workers {
                let sender = sender.clone();
                let next = &next;
                scope.spawn(move || loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(request) = requests.get(index) else {
                        break;
                    };
                    let result = self.snippets(request).map(|hits| SnippetBatchResult {
                        path: request.path.clone(),
                        hits,
                    });
                    if sender.send((index, result)).is_err() {
                        break;
                    }
                });
            }
            drop(sender);
            let mut ordered = (0..requests.len()).map(|_| None).collect::<Vec<_>>();
            for (index, result) in receiver {
                ordered[index] = Some(result);
            }
            ordered
                .into_iter()
                .enumerate()
                .map(|(index, result)| {
                    result.ok_or_else(|| {
                        format!("snippet worker returned no result for item {index}")
                    })?
                })
                .collect()
        })
    }
}

struct CleanupDir {
    path: PathBuf,
}

impl CleanupDir {
    fn new(path: PathBuf) -> Result<Self, String> {
        if path.exists() {
            fs::remove_dir_all(&path).map_err(|error| error.to_string())?;
        }
        Ok(Self { path })
    }
}

impl Drop for CleanupDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn sibling_temp_dir(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("personalrag-index");
    path.with_file_name(format!(".{name}.{suffix}"))
}

fn portable_build_options() -> BuildOptions {
    let tuning = recommend_system_build_tuning();
    BuildOptions {
        mode: BuildMode::Adaptive,
        segment_docs: tuning.segment_docs,
        workers: tuning.build_workers,
    }
}

struct PreparedFullBuildInputs {
    metadata: Vec<(u64, u64)>,
    inputs: Vec<DiskPathInput>,
    office_cache_root: PathBuf,
    office_live: BTreeMap<String, String>,
}

fn office_extraction_service(
    index_or_build_dir: &Path,
    max_file_bytes: u64,
) -> OfficeExtractionService {
    let tuning = recommend_system_build_tuning();
    let config = OfficeExtractionConfig {
        max_workers: tuning.logical_cpus.clamp(1, 4),
        memory_budget_bytes: (tuning.memory_budget_bytes / 8)
            .clamp(64 * 1024 * 1024, 512 * 1024 * 1024),
        ..Default::default()
    };
    OfficeExtractionService::new(
        OfficeExtractionService::cache_root_for_index_path(index_or_build_dir),
        ExtractionBudget::from_max_file_bytes(max_file_bytes),
        config,
    )
}

fn prepare_full_build_inputs(
    files: Vec<ScannedFile>,
    build_dir: &Path,
    spool_dir: &Path,
    max_file_bytes: u64,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(IndexBuildProgress),
) -> Result<PreparedFullBuildInputs, String> {
    let registry = ExtractorRegistry::new();
    let budget = ExtractionBudget::from_max_file_bytes(max_file_bytes);
    let office_service = office_extraction_service(build_dir, max_file_bytes);
    let source_files = files.len();
    let metadata = files
        .iter()
        .map(|file| (file.size_bytes, file.modified_ns))
        .collect::<Vec<_>>();
    let office_requests = files
        .iter()
        .enumerate()
        .filter(|(_, file)| file.index_content && office_open_xml_eligible(&file.path))
        .map(|(source_index, file)| OfficeExtractionRequest {
            source_index,
            path: file.path.clone(),
            source_bytes: file.size_bytes,
        })
        .collect::<Vec<_>>();
    let (office_results, _office_report) = office_service.prepare_many(&office_requests, cancel);
    let mut office_by_source = office_results
        .into_iter()
        .map(|result| (result.source_index(), result))
        .collect::<BTreeMap<_, _>>();
    let mut inputs = Vec::with_capacity(source_files);
    let mut office_live = BTreeMap::new();
    let mut spool_created = false;

    for (index, file) in files.into_iter().enumerate() {
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            return Err("cancelled".to_owned());
        }
        let mut content_path = None;
        let mut index_content = file.index_content;
        if index_content && office_open_xml_eligible(&file.path) {
            match office_by_source.remove(&index) {
                Some(OfficePreparedContent::Cached {
                    path, cache_key, ..
                }) => {
                    content_path = Some(path);
                    office_live.insert(file.display_path.clone(), cache_key);
                }
                Some(OfficePreparedContent::Extracted { text, .. }) => {
                    if !spool_created {
                        fs::create_dir_all(spool_dir).map_err(|error| error.to_string())?;
                        spool_created = true;
                    }
                    let prepared = spool_dir.join(format!("{index:08}.txt"));
                    fs::write(&prepared, text.as_bytes()).map_err(|error| error.to_string())?;
                    content_path = Some(prepared);
                }
                Some(OfficePreparedContent::Failed { .. }) | None => index_content = false,
            }
        } else if index_content {
            match registry.prepare(&file.path, budget) {
                Ok(PreparedContent::SourceFile) => {}
                Ok(PreparedContent::NameOnly) | Err(_) => index_content = false,
                Ok(PreparedContent::Extracted(document)) => {
                    if !spool_created {
                        fs::create_dir_all(spool_dir).map_err(|error| error.to_string())?;
                        spool_created = true;
                    }
                    let prepared = spool_dir.join(format!("{index:08}.txt"));
                    fs::write(&prepared, document.text.as_bytes())
                        .map_err(|error| error.to_string())?;
                    content_path = Some(prepared);
                }
            }
        }
        inputs.push(DiskPathInput {
            path: file.path.clone(),
            display_path: file.display_path,
            size_bytes: file.size_bytes,
            content_path,
            index_content,
        });
        if index % 128 == 0 || index + 1 == source_files {
            on_progress(IndexBuildProgress {
                phase: Some(IndexBuildPhase::Preparing),
                source_files,
                processed_files: index + 1,
                current_path: Some(file.path),
                ..IndexBuildProgress::default()
            });
        }
    }
    Ok(PreparedFullBuildInputs {
        metadata,
        inputs,
        office_cache_root: office_service.root().to_path_buf(),
        office_live,
    })
}

struct PreparedIncrementalDocument {
    document: DocumentInput,
    bytes_read: u64,
    office_cache_key: Option<String>,
}

fn document_for_incremental_file(
    file: &ScannedFile,
    registry: &ExtractorRegistry,
    office_service: &OfficeExtractionService,
    budget: ExtractionBudget,
) -> PreparedIncrementalDocument {
    let mut office_cache_key = None;
    let normalized_content = if !file.index_content {
        Vec::new()
    } else if office_open_xml_eligible(&file.path) {
        match office_service.read_search_text(&file.path) {
            Ok((text, key, _cache_hit)) => {
                office_cache_key = key;
                let mut bytes = text.into_bytes();
                bytes.make_ascii_lowercase();
                bytes
            }
            Err(_) => Vec::new(),
        }
    } else {
        match registry.prepare(&file.path, budget) {
            Ok(PreparedContent::Extracted(document)) => {
                let mut bytes = document.text.into_bytes();
                bytes.make_ascii_lowercase();
                bytes
            }
            Ok(PreparedContent::SourceFile) => fs::read(&file.path)
                .map(|mut bytes| {
                    bytes.make_ascii_lowercase();
                    bytes
                })
                .unwrap_or_default(),
            Ok(PreparedContent::NameOnly) | Err(_) => Vec::new(),
        }
    };
    let bytes_read = normalized_content.len() as u64;
    let display = file.display_path.clone();
    PreparedIncrementalDocument {
        document: DocumentInput::new(
            display.clone(),
            display.clone(),
            fold_ascii(display.as_bytes()),
            normalized_content,
        ),
        bytes_read,
        office_cache_key,
    }
}

fn publish_office_cache_live(
    service: &OfficeExtractionService,
    live: &BTreeMap<String, String>,
    run_gc: bool,
) {
    if service.publish_live(live).is_ok() && run_gc {
        let _ = service.gc(live);
    }
}

fn initialize_vnext_shadow_from_perf12(build_dir: &Path) -> Result<(), String> {
    let store = vnext_store_dir(build_dir);
    if store.exists() {
        fs::remove_dir_all(&store).map_err(|error| error.to_string())?;
    }
    let perf12 = MergedIndex::open(build_dir, true).map_err(|error| error.to_string())?;
    let documents = perf12
        .live_documents()
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|document| {
            VNextDocumentInput::new(
                document.logical_id,
                document.document.display_path,
                document.document.normalized_content,
            )
        })
        .collect::<Vec<_>>();
    initialize_vnext_generation_store(&store, &documents, VNEXT_SEGMENT_DOCS)
        .map_err(|error| error.to_string())?;
    verify_vnext_generation_store(&store)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn publish_vnext_shadow_incremental(
    index_dir: &Path,
    plan: &personalrag_portable_search::UpdatePlan,
    compact: bool,
) -> Result<(), String> {
    let store = vnext_store_dir(index_dir);
    if !store.join("CURRENT").exists() {
        return Err("vNext shadow store is not initialized".to_owned());
    }
    publish_vnext_incremental_generation(&store, plan, VNEXT_SEGMENT_DOCS)
        .map_err(|error| error.to_string())?;
    if compact {
        compact_vnext_generation_store(&store, VNEXT_SEGMENT_DOCS)
            .map_err(|error| error.to_string())?;
        let _ = gc_vnext_generation_store(&store, Duration::ZERO);
    }
    verify_vnext_generation_store(&store)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn outcome_from_snapshot(
    files: &[ScannedFile],
    snapshot: &personalrag_portable_search::CatalogSnapshot,
    processed_files: usize,
    bytes_read: u64,
) -> Result<IndexBuildOutcome, String> {
    let mut ordered = files.iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| left.display_path.cmp(&right.display_path));
    let mut paths = Vec::with_capacity(ordered.len());
    let mut logical_ids = Vec::with_capacity(files.len());
    let mut size_bytes = Vec::with_capacity(files.len());
    let mut modified_ns = Vec::with_capacity(files.len());
    for file in ordered {
        let Some(entry) = snapshot.live.get(&file.display_path) else {
            return Err(format!(
                "published incremental catalog is missing {}",
                file.display_path
            ));
        };
        paths.push(file.display_path.clone());
        logical_ids.push(entry.logical_id);
        size_bytes.push(file.size_bytes);
        modified_ns.push(file.modified_ns);
    }
    Ok(IndexBuildOutcome {
        source_files: paths.len(),
        processed_files,
        indexed_files: paths.len(),
        skipped_files: 0,
        bytes_read,
        paths,
        logical_ids,
        generation: snapshot.generation,
        next_logical_id: snapshot.next_logical_id,
        size_bytes,
        modified_ns,
    })
}

fn scan_exclusions(config: &crate::contract_v1::ExclusionConfig) -> ScanExclusions {
    ScanExclusions {
        dev_caches: config.dev_caches,
        virtual_envs: config.virtual_envs,
        node_modules: config.node_modules,
        build_artifacts: config.build_artifacts,
        vcs: config.vcs,
        use_gitignore: config.use_gitignore,
        custom_directory_names: config.custom_directory_names.clone(),
        custom_relative_paths: config.custom_relative_paths.clone(),
        custom_globs: config.custom_globs.clone(),
    }
}

fn sparse_outcome_from_snapshot(
    root: &Path,
    previous: &IncrementalCatalogState,
    upserts: &[ScannedFile],
    deleted_paths: &[String],
    snapshot: &personalrag_portable_search::CatalogSnapshot,
    processed_files: usize,
    bytes_read: u64,
) -> Result<IndexBuildOutcome, String> {
    let deleted = deleted_paths
        .iter()
        .map(String::as_str)
        .collect::<HashSet<_>>();
    let replacements = upserts
        .iter()
        .map(|file| (file.display_path.as_str(), file))
        .collect::<HashMap<_, _>>();
    let mut existing = Vec::with_capacity(previous.paths.len());
    for row in 0..previous.paths.len() {
        let path = &previous.paths[row];
        if deleted.contains(path.as_str()) {
            continue;
        }
        if let Some(file) = replacements.get(path.as_str()) {
            existing.push((path.clone(), file.size_bytes, file.modified_ns));
        } else {
            existing.push((
                path.clone(),
                previous.size_bytes[row],
                previous.modified_ns[row],
            ));
        }
    }
    let mut added = upserts
        .iter()
        .filter(|file| previous.paths.binary_search(&file.display_path).is_err())
        .map(|file| (file.display_path.clone(), file.size_bytes, file.modified_ns))
        .collect::<Vec<_>>();
    added.sort_by(|left, right| left.0.cmp(&right.0));

    let mut merged = Vec::with_capacity(existing.len() + added.len());
    let (mut left, mut right) = (0usize, 0usize);
    while left < existing.len() || right < added.len() {
        let take_existing =
            right >= added.len() || (left < existing.len() && existing[left].0 < added[right].0);
        if take_existing {
            merged.push(existing[left].clone());
            left += 1;
        } else {
            merged.push(added[right].clone());
            right += 1;
        }
    }

    let mut paths = Vec::with_capacity(merged.len());
    let mut logical_ids = Vec::with_capacity(merged.len());
    let mut size_bytes = Vec::with_capacity(merged.len());
    let mut modified_ns = Vec::with_capacity(merged.len());
    for (path, size, modified) in merged {
        let entry = snapshot.live.get(&path).ok_or_else(|| {
            format!(
                "published incremental catalog is missing {}",
                root.join(&path).display()
            )
        })?;
        paths.push(path);
        logical_ids.push(entry.logical_id);
        size_bytes.push(size);
        modified_ns.push(modified);
    }
    Ok(IndexBuildOutcome {
        source_files: paths.len(),
        processed_files,
        indexed_files: paths.len(),
        skipped_files: 0,
        bytes_read,
        paths,
        logical_ids,
        generation: snapshot.generation,
        next_logical_id: snapshot.next_logical_id,
        size_bytes,
        modified_ns,
    })
}

impl IndexEngine for PortableEngine {
    fn scan(
        &self,
        root: &Path,
        max_file_bytes: u64,
        scanner_mode: &str,
        exclusions: &crate::contract_v1::ExclusionConfig,
        cancel: Arc<AtomicBool>,
        on_progress: Arc<dyn Fn(ScanProgress) + Send + Sync>,
    ) -> Result<ScanReport, String> {
        let scan_exclusions = scan_exclusions(exclusions);
        scan_files(
            root,
            max_file_bytes,
            ScannerMode::parse(scanner_mode),
            &scan_exclusions,
            cancel,
            on_progress,
        )
    }

    fn capture_change_checkpoint(&self, root: &Path) -> Result<Option<UsnCheckpoint>, String> {
        capture_usn_checkpoint(root)
    }

    fn scan_changes(
        &self,
        root: &Path,
        checkpoint: UsnCheckpoint,
        directories: &DirectoryTrackingSnapshot,
        max_file_bytes: u64,
        exclusions: &crate::contract_v1::ExclusionConfig,
    ) -> Result<UsnScanResult, String> {
        let scan_exclusions = scan_exclusions(exclusions);
        scan_usn_changes(
            root,
            checkpoint,
            directories,
            max_file_bytes,
            &scan_exclusions,
        )
    }

    fn build(
        &self,
        root: &Path,
        mut files: Vec<ScannedFile>,
        build_dir: &Path,
        max_file_bytes: u64,
        cancel: &AtomicBool,
        on_progress: &mut dyn FnMut(IndexBuildProgress),
    ) -> Result<IndexBuildOutcome, String> {
        self.invalidate_search_cache()?;
        on_progress(IndexBuildProgress {
            phase: Some(IndexBuildPhase::Preparing),
            source_files: files.len(),
            current_path: Some(PathBuf::from(format!("{} paths", files.len()))),
            ..IndexBuildProgress::default()
        });
        let sort_started = Instant::now();
        let sort_workers = crate::build_order::sort_scanned_files(&mut files)?;
        if std::env::var_os("PR_PROFILE_BUILD_ORDER").is_some() {
            eprintln!(
                "BUILD_ORDER_SORT files={} workers={} elapsed_ms={:.3}",
                files.len(),
                sort_workers,
                sort_started.elapsed().as_secs_f64() * 1000.0,
            );
        }
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            return Err("cancelled".to_owned());
        }

        let spool_path = sibling_temp_dir(build_dir, "extract");
        let _spool_cleanup = CleanupDir::new(spool_path.clone())?;
        let PreparedFullBuildInputs {
            metadata,
            inputs,
            office_cache_root,
            office_live,
        } = prepare_full_build_inputs(
            files,
            build_dir,
            &spool_path,
            max_file_bytes,
            cancel,
            on_progress,
        )?;
        let content_files = inputs.iter().filter(|file| file.index_content).count();
        let content_bytes = inputs
            .iter()
            .filter(|file| file.index_content)
            .map(|file| file.size_bytes)
            .sum::<u64>();

        let tuning = recommend_system_build_tuning();
        let hydration_workers = hydration_workers_for(
            cfg!(windows),
            tuning.logical_cpus,
            content_files,
            content_bytes,
            tuning.scan_workers,
        );
        let build_options = portable_build_options();
        let base_index_path = sibling_temp_dir(build_dir, "base-index");
        let _base_cleanup = CleanupDir::new(base_index_path.clone())?;
        let production_mode = self.production_backend()?;
        let capture_budget = (tuning.memory_budget_bytes / 8).clamp(64 * MIB, 512 * MIB);
        // Retaining normalized documents removes the post-Perf12 re-materialization pass, but
        // keeping a large corpus alive while PRPOS frontiers build adds memory/cache pressure.
        // A/B profiling showed <=~10 MiB improves wall time while ~80 MiB regresses, so keep the
        // no-copy fast path deliberately small and fall back to the snapshot path for large roots.
        let retained_hydration_budget = capture_budget.min(32 * MIB);
        let estimated_capture_bytes = content_bytes.saturating_add(
            inputs
                .iter()
                .map(|input| input.display_path.len() as u64)
                .sum::<u64>(),
        );
        let retain_for_vnext = production_mode != ProductionBackendMode::Perf12
            && estimated_capture_bytes <= retained_hydration_budget;
        let mut shared_vnext_capture = None::<Vec<VNextDocumentInput>>;
        let build_config = DiskPathBuildConfig {
            max_docs: None,
            max_file_bytes,
            build: &build_options,
            scan_workers: hydration_workers,
            hydration_batch_bytes: hydration_batch_budget(tuning.memory_budget_bytes),
            cancel: Some(cancel),
        };
        let mut progress_adapter =
            |progress: &personalrag_portable_search::DiskPathBuildProgress| {
                on_progress(IndexBuildProgress {
                    phase: Some(IndexBuildPhase::Building),
                    source_files: progress.source_files,
                    processed_files: progress.processed_files,
                    indexed_files: progress.indexed_files,
                    skipped_files: progress.skipped_files,
                    bytes_read: progress.bytes_read,
                    prepared_bytes: progress.prepared_bytes,
                    current_path: progress.current_path.clone(),
                });
            };
        let map_build_error = |error: personalrag_portable_search::SearchError| {
            if error.to_string().contains("build cancelled") {
                "cancelled".to_owned()
            } else {
                error.to_string()
            }
        };
        let report = if production_mode == ProductionBackendMode::Perf12 {
            build_disk_path_inputs_index_unified(
                root,
                inputs,
                &base_index_path,
                build_config,
                PRODUCTION_ACCELERATION_PROFILE,
                &mut progress_adapter,
            )
            .map_err(map_build_error)?
        } else if retain_for_vnext {
            let (report, retained) = build_disk_path_inputs_index_unified_retained(
                root,
                inputs,
                &base_index_path,
                build_config,
                PRODUCTION_ACCELERATION_PROFILE,
                &mut progress_adapter,
            )
            .map_err(map_build_error)?;
            if retained.len() == report.build.docs {
                shared_vnext_capture = Some(
                    retained
                        .into_iter()
                        .enumerate()
                        .map(|(index, document)| {
                            VNextDocumentInput::new(
                                index as u64 + 1,
                                document.display_path,
                                document.normalized_content,
                            )
                        })
                        .collect(),
                );
            }
            report
        } else {
            build_disk_path_inputs_index_unified(
                root,
                inputs,
                &base_index_path,
                build_config,
                PRODUCTION_ACCELERATION_PROFILE,
                &mut progress_adapter,
            )
            .map_err(map_build_error)?
        };
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            return Err("cancelled".to_owned());
        }

        let terminal_progress = |phase: IndexBuildPhase, label: &str| IndexBuildProgress {
            phase: Some(phase),
            source_files: report.source_files,
            processed_files: report.processed_files,
            indexed_files: report.build.docs,
            skipped_files: report.skipped_files,
            bytes_read: report.bytes_read,
            prepared_bytes: 0,
            current_path: Some(PathBuf::from(label)),
        };

        on_progress(terminal_progress(IndexBuildPhase::Verifying, "verify"));
        verify_index(&base_index_path).map_err(|error| error.to_string())?;
        if PRODUCTION_ACCELERATION_PROFILE == AccelerationProfile::Full {
            verify_positional_sidecars(&base_index_path, PosCodec::production())
                .map_err(|error| error.to_string())?;
            verify_positional2_sidecars(&base_index_path).map_err(|error| error.to_string())?;
            verify_positional3_sidecars(&base_index_path).map_err(|error| error.to_string())?;
        }

        let identities = report
            .display_paths
            .iter()
            .enumerate()
            .map(|(index, path)| {
                LogicalDocumentIdentity::new(index as u64 + 1, path.clone(), path.clone())
            })
            .collect::<Vec<_>>();
        initialize_generation_from_built_index(build_dir, &base_index_path, &identities)
            .map_err(|error| error.to_string())?;
        verify_generation(build_dir).map_err(|error| error.to_string())?;

        if production_mode != ProductionBackendMode::Perf12 {
            let store = vnext_store_dir(build_dir);
            if store.exists() {
                fs::remove_dir_all(&store).map_err(|error| error.to_string())?;
            }
            if let Some(captured) = shared_vnext_capture.take() {
                initialize_vnext_generation_store(&store, &captured, VNEXT_SEGMENT_DOCS)
                    .map_err(|error| error.to_string())?;
            } else {
                initialize_vnext_shadow_from_perf12(build_dir)?;
            }
            verify_vnext_generation_store(&store).map_err(|error| error.to_string())?;
        }

        let cache_service = OfficeExtractionService::new(
            office_cache_root,
            ExtractionBudget::from_max_file_bytes(max_file_bytes),
            OfficeExtractionConfig::default(),
        );
        publish_office_cache_live(&cache_service, &office_live, true);

        let mut sizes = Vec::with_capacity(report.source_indices.len());
        let mut modified = Vec::with_capacity(report.source_indices.len());
        for source_index in &report.source_indices {
            let (size_bytes, modified_ns) = metadata
                .get(*source_index as usize)
                .copied()
                .unwrap_or((0, 0));
            sizes.push(size_bytes);
            modified.push(modified_ns);
        }
        let logical_ids = (1..=report.display_paths.len() as u64).collect::<Vec<_>>();
        Ok(IndexBuildOutcome {
            paths: report.display_paths,
            generation: 0,
            next_logical_id: logical_ids.last().copied().unwrap_or(0) + 1,
            logical_ids,
            size_bytes: sizes,
            modified_ns: modified,
            source_files: report.source_files,
            processed_files: report.processed_files,
            indexed_files: report.build.docs,
            skipped_files: report.skipped_files,
            bytes_read: report.bytes_read,
        })
    }

    fn sync_incremental(
        &self,
        request: IncrementalSyncRequest<'_>,
        cancel: &AtomicBool,
        on_progress: &mut dyn FnMut(IndexBuildProgress),
    ) -> Result<IncrementalSyncResult, String> {
        self.invalidate_search_cache()?;
        let IncrementalSyncRequest {
            root: _root,
            files,
            index_dir,
            previous,
            max_file_bytes,
        } = request;
        previous.validate()?;
        let current = MergedIndex::open(index_dir, true).map_err(|error| error.to_string())?;
        if current.generation() != previous.generation
            || current.live_docs() != previous.paths.len()
        {
            return Ok(IncrementalSyncResult::FullRebuildRequired {
                changed_files: files.len(),
            });
        }
        let diff = diff_catalog(&previous, files)?;
        let changed_files = diff.changed_files();
        if changed_files == 0 {
            let snapshot = previous.snapshot()?;
            return outcome_from_snapshot(files, &snapshot, 0, 0)
                .map(IncrementalSyncResult::Unchanged);
        }
        if changed_files >= MAX_INCREMENTAL_CHANGED_FILES {
            return Ok(IncrementalSyncResult::FullRebuildRequired { changed_files });
        }
        on_progress(IndexBuildProgress {
            phase: Some(IndexBuildPhase::Preparing),
            source_files: changed_files,
            current_path: Some(PathBuf::from(format!("{changed_files} changed paths"))),
            ..IndexBuildProgress::default()
        });

        let registry = ExtractorRegistry::new();
        let budget = ExtractionBudget::from_max_file_bytes(max_file_bytes);
        let office_service = office_extraction_service(index_dir, max_file_bytes);
        let mut office_live = office_service.load_live();
        let mut changes = Vec::with_capacity(changed_files);
        for deleted in &diff.deleted {
            changes.push(DocumentChange {
                kind: ChangeKind::Delete,
                key: deleted.path.clone(),
                document: None,
            });
            office_live.remove(&deleted.path);
        }
        let mut processed = diff.deleted.len();
        let mut bytes_read = 0u64;
        for changed in &diff.modified {
            if cancel.load(std::sync::atomic::Ordering::Acquire) {
                return Err("cancelled".to_owned());
            }
            let prepared =
                document_for_incremental_file(&changed.file, &registry, &office_service, budget);
            bytes_read = bytes_read.saturating_add(prepared.bytes_read);
            match prepared.office_cache_key {
                Some(key) => {
                    office_live.insert(changed.file.display_path.clone(), key);
                }
                None => {
                    office_live.remove(&changed.file.display_path);
                }
            }
            changes.push(DocumentChange {
                kind: ChangeKind::Upsert,
                key: changed.file.display_path.clone(),
                document: Some(prepared.document),
            });
            processed += 1;
            on_progress(IndexBuildProgress {
                phase: Some(IndexBuildPhase::Preparing),
                source_files: changed_files,
                processed_files: processed,
                bytes_read,
                current_path: Some(changed.file.path.clone()),
                ..IndexBuildProgress::default()
            });
        }
        for added in &diff.added {
            if cancel.load(std::sync::atomic::Ordering::Acquire) {
                return Err("cancelled".to_owned());
            }
            let prepared = document_for_incremental_file(added, &registry, &office_service, budget);
            bytes_read = bytes_read.saturating_add(prepared.bytes_read);
            match prepared.office_cache_key {
                Some(key) => {
                    office_live.insert(added.display_path.clone(), key);
                }
                None => {
                    office_live.remove(&added.display_path);
                }
            }
            changes.push(DocumentChange {
                kind: ChangeKind::Upsert,
                key: added.display_path.clone(),
                document: Some(prepared.document),
            });
            processed += 1;
            on_progress(IndexBuildProgress {
                phase: Some(IndexBuildPhase::Preparing),
                source_files: changed_files,
                processed_files: processed,
                bytes_read,
                current_path: Some(added.path.clone()),
                ..IndexBuildProgress::default()
            });
        }

        let base = previous.snapshot()?;
        let batch = ChangeBatch {
            expected_base_generation: previous.generation,
            changes,
        };
        let plan = plan_incremental_update(&base, &batch, IncrementalPolicy::default())
            .map_err(|error| error.to_string())?;
        let compact_after_publish =
            changed_files >= MIN_COMPACTION_FALLBACK_CHANGES && plan.compaction_recommended;
        let next = apply_update_plan(&base, &plan).map_err(|error| error.to_string())?;
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            return Err("cancelled".to_owned());
        }
        on_progress(IndexBuildProgress {
            phase: Some(IndexBuildPhase::Building),
            source_files: changed_files,
            processed_files: changed_files,
            indexed_files: plan.upserts.len(),
            bytes_read,
            current_path: Some(PathBuf::from(format!(
                "generation {}",
                plan.next_generation
            ))),
            ..IndexBuildProgress::default()
        });
        publish_incremental_update_unified(index_dir, &plan, &portable_build_options())
            .map_err(|error| error.to_string())?;
        if compact_after_publish {
            if cancel.load(std::sync::atomic::Ordering::Acquire) {
                return Err("cancelled".to_owned());
            }
            on_progress(IndexBuildProgress {
                phase: Some(IndexBuildPhase::Building),
                source_files: changed_files,
                processed_files: changed_files,
                indexed_files: plan.upserts.len(),
                bytes_read,
                current_path: Some(PathBuf::from(format!(
                    "compact generation {}",
                    plan.next_generation
                ))),
                ..IndexBuildProgress::default()
            });
            compact_generation_unified(index_dir, &portable_build_options())
                .map_err(|error| error.to_string())?;
        }
        if vnext_store_dir(index_dir).join("CURRENT").exists() {
            let _ = publish_vnext_shadow_incremental(index_dir, &plan, compact_after_publish);
        }
        on_progress(IndexBuildProgress {
            phase: Some(IndexBuildPhase::Verifying),
            source_files: changed_files,
            processed_files: changed_files,
            indexed_files: plan.upserts.len(),
            bytes_read,
            current_path: Some(PathBuf::from("verify generation")),
            ..IndexBuildProgress::default()
        });
        verify_generation(index_dir).map_err(|error| error.to_string())?;
        publish_office_cache_live(&office_service, &office_live, false);
        outcome_from_snapshot(files, &next, changed_files, bytes_read)
            .map(IncrementalSyncResult::Applied)
    }

    fn sync_incremental_changes(
        &self,
        request: IncrementalChangeSyncRequest<'_>,
        cancel: &AtomicBool,
        on_progress: &mut dyn FnMut(IndexBuildProgress),
    ) -> Result<IncrementalSyncResult, String> {
        self.invalidate_search_cache()?;
        let IncrementalChangeSyncRequest {
            root,
            upserts,
            deleted_paths,
            index_dir,
            previous,
            max_file_bytes,
        } = request;
        previous.validate()?;
        if previous.paths.windows(2).any(|pair| pair[0] >= pair[1]) {
            return Err("incremental catalog paths must be strictly sorted".to_owned());
        }
        let current = MergedIndex::open(index_dir, true).map_err(|error| error.to_string())?;
        if current.generation() != previous.generation
            || current.live_docs() != previous.paths.len()
        {
            return Ok(IncrementalSyncResult::FullRebuildRequired {
                changed_files: upserts.len() + deleted_paths.len(),
            });
        }

        let mut unique_upserts = HashMap::<&str, &ScannedFile>::with_capacity(upserts.len());
        for file in upserts {
            if file.display_path.is_empty() {
                return Err("USN change contains an empty display path".to_owned());
            }
            unique_upserts.insert(file.display_path.as_str(), file);
        }
        let unique_deleted = deleted_paths
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        // A USN event is stronger evidence than size/mtime. Re-index every reported upsert so
        // same-size edits with preserved timestamps are still observed by the journal fast path.
        let mut actual_upserts = unique_upserts.into_values().cloned().collect::<Vec<_>>();
        actual_upserts.sort_by(|left, right| left.display_path.cmp(&right.display_path));
        let upsert_paths = actual_upserts
            .iter()
            .map(|file| file.display_path.as_str())
            .collect::<HashSet<_>>();
        let mut actual_deleted = unique_deleted
            .into_iter()
            .filter(|path| !upsert_paths.contains(path))
            .filter(|path| {
                previous
                    .paths
                    .binary_search_by(|probe| probe.as_str().cmp(path))
                    .is_ok()
            })
            .map(str::to_owned)
            .collect::<Vec<_>>();
        actual_deleted.sort();
        let changed_files = actual_upserts.len() + actual_deleted.len();
        if changed_files == 0 {
            let snapshot = previous.snapshot()?;
            return sparse_outcome_from_snapshot(root, &previous, &[], &[], &snapshot, 0, 0)
                .map(IncrementalSyncResult::Unchanged);
        }
        if changed_files >= MAX_INCREMENTAL_CHANGED_FILES {
            return Ok(IncrementalSyncResult::FullRebuildRequired { changed_files });
        }
        on_progress(IndexBuildProgress {
            phase: Some(IndexBuildPhase::Preparing),
            source_files: changed_files,
            current_path: Some(PathBuf::from(format!("{changed_files} journal changes"))),
            ..IndexBuildProgress::default()
        });

        let registry = ExtractorRegistry::new();
        let budget = ExtractionBudget::from_max_file_bytes(max_file_bytes);
        let office_service = office_extraction_service(index_dir, max_file_bytes);
        let mut office_live = office_service.load_live();
        let mut changes = Vec::with_capacity(changed_files);
        for path in &actual_deleted {
            changes.push(DocumentChange {
                kind: ChangeKind::Delete,
                key: path.clone(),
                document: None,
            });
            office_live.remove(path);
        }
        let mut bytes_read = 0u64;
        for (offset, file) in actual_upserts.iter().enumerate() {
            if cancel.load(std::sync::atomic::Ordering::Acquire) {
                return Err("cancelled".to_owned());
            }
            let prepared = document_for_incremental_file(file, &registry, &office_service, budget);
            bytes_read = bytes_read.saturating_add(prepared.bytes_read);
            match prepared.office_cache_key {
                Some(key) => {
                    office_live.insert(file.display_path.clone(), key);
                }
                None => {
                    office_live.remove(&file.display_path);
                }
            }
            changes.push(DocumentChange {
                kind: ChangeKind::Upsert,
                key: file.display_path.clone(),
                document: Some(prepared.document),
            });
            let processed = actual_deleted.len() + offset + 1;
            on_progress(IndexBuildProgress {
                phase: Some(IndexBuildPhase::Preparing),
                source_files: changed_files,
                processed_files: processed,
                bytes_read,
                current_path: Some(file.path.clone()),
                ..IndexBuildProgress::default()
            });
        }

        let base = previous.snapshot()?;
        let batch = ChangeBatch {
            expected_base_generation: previous.generation,
            changes,
        };
        let plan = plan_incremental_update(&base, &batch, IncrementalPolicy::default())
            .map_err(|error| error.to_string())?;
        let compact_after_publish =
            changed_files >= MIN_COMPACTION_FALLBACK_CHANGES && plan.compaction_recommended;
        let next = apply_update_plan(&base, &plan).map_err(|error| error.to_string())?;
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            return Err("cancelled".to_owned());
        }
        on_progress(IndexBuildProgress {
            phase: Some(IndexBuildPhase::Building),
            source_files: changed_files,
            processed_files: changed_files,
            indexed_files: plan.upserts.len(),
            bytes_read,
            current_path: Some(PathBuf::from(format!(
                "generation {}",
                plan.next_generation
            ))),
            ..IndexBuildProgress::default()
        });
        publish_incremental_update_unified(index_dir, &plan, &portable_build_options())
            .map_err(|error| error.to_string())?;
        if compact_after_publish {
            if cancel.load(std::sync::atomic::Ordering::Acquire) {
                return Err("cancelled".to_owned());
            }
            on_progress(IndexBuildProgress {
                phase: Some(IndexBuildPhase::Building),
                source_files: changed_files,
                processed_files: changed_files,
                indexed_files: plan.upserts.len(),
                bytes_read,
                current_path: Some(PathBuf::from(format!(
                    "compact generation {}",
                    plan.next_generation
                ))),
                ..IndexBuildProgress::default()
            });
            compact_generation_unified(index_dir, &portable_build_options())
                .map_err(|error| error.to_string())?;
        }
        if vnext_store_dir(index_dir).join("CURRENT").exists() {
            let _ = publish_vnext_shadow_incremental(index_dir, &plan, compact_after_publish);
        }
        verify_generation(index_dir).map_err(|error| error.to_string())?;
        publish_office_cache_live(&office_service, &office_live, false);
        sparse_outcome_from_snapshot(
            root,
            &previous,
            &actual_upserts,
            &actual_deleted,
            &next,
            changed_files,
            bytes_read,
        )
        .map(IncrementalSyncResult::Applied)
    }
}

#[cfg(test)]
mod policy_tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        sync::{atomic::AtomicBool, Arc, Mutex},
        thread,
        time::{Duration, SystemTime},
    };

    use personalrag_portable_search::{
        initialize_generation, initialize_vnext_generation_store, BuildMode, BuildOptions,
        DocumentInput, LogicalDocument, MergedIndex, VNextDocumentInput,
    };

    use crate::{IncrementalCatalogState, SearchOptions};

    use super::{
        hydration_batch_budget, hydration_workers_for, vnext_store_dir,
        IncrementalChangeSyncRequest, IncrementalSyncResult, IndexEngine, PortableEngine,
        ProductionBackendMode, ProductionBackendTelemetry, SearchCatalogView,
        ShadowCompareExecutor, ShadowCompareJob, ShadowCompareKey, MIB,
    };

    #[test]
    fn production_backend_mode_is_strict_and_perf12_is_default() {
        assert_eq!(
            ProductionBackendMode::parse("perf12").unwrap(),
            ProductionBackendMode::Perf12
        );
        assert_eq!(
            ProductionBackendMode::parse(" SHADOW ").unwrap(),
            ProductionBackendMode::Shadow
        );
        assert_eq!(
            ProductionBackendMode::parse("vnext").unwrap(),
            ProductionBackendMode::VNext
        );
        assert!(ProductionBackendMode::parse("auto").is_err());
        assert_eq!(
            PortableEngine::default().production_backend().unwrap(),
            ProductionBackendMode::Perf12
        );
    }

    #[test]
    fn sort_order_cache_matches_gui_sort_semantics() {
        let engine = PortableEngine::default();
        let paths = vec![
            "z/a.log".to_owned(),
            "a/c.txt".to_owned(),
            "b/b.txt".to_owned(),
            "a/a.txt".to_owned(),
        ];
        let sizes = vec![10, 30, 30, 20];
        let modified = vec![4, 1, 3, 2];
        let logical_ids = vec![1, 2, 3, 4];
        let logical_to_row = vec![u32::MAX, 0, 1, 2, 3];
        let catalog = SearchCatalogView {
            root: Path::new("."),
            paths: &paths,
            size_bytes: &sizes,
            modified_ns: &modified,
            logical_ids: &logical_ids,
            logical_to_row: &logical_to_row,
            generation: 7,
            max_file_bytes: 1024,
        };
        let options = SearchOptions {
            file_query: None,
            include_path: true,
            content_query: Some("timeout".to_owned()),
            extensions: Vec::new(),
            path_scope: None,
            match_case: false,
            whole_words: false,
            regex: false,
            sort_field: "size".to_owned(),
            sort_direction: "descending".to_owned(),
            limit: 2,
            backend: "v2".to_owned(),
        };
        let order = engine
            .sort_order_for_search(&catalog, &options)
            .unwrap()
            .unwrap();
        // size desc, ties preserve ascending path: a/c.txt before b/b.txt.
        assert_eq!(order.logical_ids, vec![2, 3, 4, 1]);
        assert_eq!(order.rank_by_row, vec![3, 0, 1, 2]);
        let cached = engine
            .sort_order_for_search(&catalog, &options)
            .unwrap()
            .unwrap();
        assert!(Arc::ptr_eq(&order, &cached));
    }

    #[test]
    fn vnext_mode_falls_back_to_perf12_when_shadow_is_not_ready() {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("personalrag-vnext-fallback-{unique}"));
        fs::create_dir_all(&root).unwrap();
        let engine = PortableEngine::with_production_backend(ProductionBackendMode::VNext);
        assert_eq!(
            engine.active_production_backend(&root, 0).unwrap(),
            "perf12"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shadow_compare_executor_runs_off_response_thread() {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("personalrag-shadow-async-{unique}"));
        fs::create_dir_all(&root).unwrap();
        let docs = vec![
            LogicalDocument::new(
                1,
                DocumentInput::new(
                    "alpha.txt",
                    "alpha.txt",
                    b"alpha.txt".to_vec(),
                    b"alpha timeout marker".to_vec(),
                ),
            ),
            LogicalDocument::new(
                2,
                DocumentInput::new(
                    "beta.txt",
                    "beta.txt",
                    b"beta.txt".to_vec(),
                    b"beta timeout marker".to_vec(),
                ),
            ),
        ];
        initialize_generation(
            &root,
            &docs,
            &BuildOptions {
                mode: BuildMode::Adaptive,
                segment_docs: 5_000,
                workers: 2,
            },
        )
        .unwrap();
        initialize_vnext_generation_store(
            vnext_store_dir(&root),
            &[
                VNextDocumentInput::new(1, "alpha.txt", b"alpha timeout marker".to_vec()),
                VNextDocumentInput::new(2, "beta.txt", b"beta timeout marker".to_vec()),
            ],
            5_000,
        )
        .unwrap();

        let telemetry = Arc::new(Mutex::new(ProductionBackendTelemetry::default()));
        let executor = ShadowCompareExecutor::new(Arc::clone(&telemetry));
        assert!(executor
            .try_submit(ShadowCompareJob {
                key: ShadowCompareKey {
                    index_dir: root.clone(),
                    generation: 0,
                    file_query: None,
                    content_query: Some("timeout".to_owned()),
                },
            })
            .unwrap());

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let snapshot = telemetry.lock().unwrap().clone();
            if snapshot.shadow_comparisons == 1 || snapshot.shadow_failures != 0 {
                assert_eq!(snapshot.shadow_comparisons, 1);
                assert_eq!(snapshot.shadow_mismatches, 0);
                assert_eq!(snapshot.shadow_failures, 0);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "shadow worker timed out"
            );
            thread::sleep(Duration::from_millis(5));
        }
        drop(executor);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shadow_compare_executor_coalesces_duplicate_pending_job() {
        let telemetry = Arc::new(Mutex::new(ProductionBackendTelemetry::default()));
        let executor = ShadowCompareExecutor::new(telemetry);
        let key = ShadowCompareKey {
            index_dir: PathBuf::from("coalesce-test"),
            generation: 7,
            file_query: Some("alpha".to_owned()),
            content_query: Some("timeout".to_owned()),
        };
        executor.pending.lock().unwrap().insert(key.clone());
        let queued = executor
            .try_submit(ShadowCompareJob { key: key.clone() })
            .unwrap();
        assert!(!queued);
        assert_eq!(executor.pending.lock().unwrap().len(), 1);
        executor.pending.lock().unwrap().remove(&key);
    }

    #[test]
    fn hydration_budget_keeps_headroom() {
        assert_eq!(hydration_batch_budget(128 * MIB), 32 * MIB);
        assert_eq!(hydration_batch_budget(1024 * MIB), 128 * MIB);
        assert_eq!(hydration_batch_budget(16 * 1024 * MIB), 128 * MIB);
    }

    #[test]
    fn windows_small_file_hydration_uses_more_latency_hiding_workers() {
        assert_eq!(hydration_workers_for(true, 16, 10_000, 320 * MIB, 2), 8);
        assert_eq!(hydration_workers_for(true, 16, 100, 50 * MIB, 2), 4);
        assert_eq!(hydration_workers_for(true, 16, 10, 50 * MIB, 2), 2);
        assert_eq!(hydration_workers_for(false, 16, 10_000, 320 * MIB, 2), 2);
    }

    #[test]
    fn tombstone_heavy_incremental_sync_compacts_without_full_filesystem_rebuild() {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("personalrag-stream-compact-{unique}"));
        let count = 5_000usize;
        let documents = (0..count)
            .map(|row| {
                let logical_id = row as u64 + 1;
                let path = format!("p-{row:05}.txt");
                LogicalDocument::new(
                    logical_id,
                    DocumentInput::new(
                        path.clone(),
                        path.clone(),
                        path.as_bytes().to_vec(),
                        b"common shared content".to_vec(),
                    ),
                )
            })
            .collect::<Vec<_>>();
        initialize_generation(
            &root,
            &documents,
            &BuildOptions {
                mode: BuildMode::Adaptive,
                segment_docs: 5_000,
                workers: 2,
            },
        )
        .unwrap();
        let paths = (0..count)
            .map(|row| format!("p-{row:05}.txt"))
            .collect::<Vec<_>>();
        let previous = IncrementalCatalogState {
            generation: 0,
            next_logical_id: count as u64 + 1,
            logical_ids: (1..=count as u64).collect(),
            size_bytes: vec![21; count],
            modified_ns: vec![1; count],
            paths: paths.clone(),
        };
        let deleted_paths = paths[..1_024].to_vec();
        let engine = PortableEngine::default();
        let result = engine
            .sync_incremental_changes(
                IncrementalChangeSyncRequest {
                    root: &root,
                    upserts: &[],
                    deleted_paths: &deleted_paths,
                    index_dir: &root,
                    previous,
                    max_file_bytes: 1024 * 1024,
                },
                &AtomicBool::new(false),
                &mut |_| {},
            )
            .unwrap();
        let IncrementalSyncResult::Applied(outcome) = result else {
            panic!("tombstone-heavy sync unexpectedly requested a full rebuild");
        };
        assert_eq!(outcome.paths.len(), count - deleted_paths.len());
        let merged = MergedIndex::open(&root, true).unwrap();
        assert_eq!(merged.generation(), 1);
        assert_eq!(merged.live_docs(), count - deleted_paths.len());
        assert_eq!(merged.delta_count(), 0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn merged_search_session_is_reused_within_a_generation() {
        let unique = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("personalrag-session-cache-{unique}"));
        let documents = vec![
            LogicalDocument::new(
                1,
                DocumentInput::new("a.txt", "a.txt", b"a.txt".to_vec(), b"alpha".to_vec()),
            ),
            LogicalDocument::new(
                2,
                DocumentInput::new("b.txt", "b.txt", b"b.txt".to_vec(), b"beta".to_vec()),
            ),
        ];
        initialize_generation(
            &root,
            &documents,
            &BuildOptions {
                mode: BuildMode::Adaptive,
                segment_docs: 10,
                workers: 1,
            },
        )
        .unwrap();

        let engine = PortableEngine::default();
        let first = engine.merged_search_session(&root, 0).unwrap();
        let second = engine.merged_search_session(&root, 0).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        drop(first);
        drop(second);
        engine.invalidate_search_cache().unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
