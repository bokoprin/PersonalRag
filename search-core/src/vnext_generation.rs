use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, mpsc};
use std::thread;
use std::time::Instant;

use crate::fold_ascii;
use crate::format::SearchError;
use crate::index::ExactMatcher;
use crate::types::{Generation, LogicalDocId};
use crate::{VNextDocumentInput, VNextSegmentReader};

const GENERATION_HIGH_HIT_BLOCKS: usize = 8_192;
const GENERATION_BLOB_SCAN_MAX_AVG_DOC_BYTES: usize = 256;
const DENSE_LOCATION_MAX_MULTIPLIER: usize = 4;
const DENSE_LOCATION_EMPTY: u64 = u64::MAX;
const GENERATION_OPEN_MAX_WORKERS: usize = 8;
const GENERATION_QUERY_MAX_WORKERS: usize = 8;
const GENERATION_HIGH_HIT_MIN_CHUNK_BLOCKS: usize = 8_192;
const FIRST_N_SHORT_MULTIPLIER: usize = 64;
const FIRST_N_LONG_MULTIPLIER: usize = 12;
const HIDDEN_LIVE_ORDINAL: u32 = u32::MAX;

type QueryJob = Box<dyn FnOnce() + Send + 'static>;

struct GenerationQueryPool {
    senders: Vec<mpsc::Sender<QueryJob>>,
}

impl GenerationQueryPool {
    fn new() -> Self {
        let workers = thread::available_parallelism()
            .map_or(1, usize::from)
            .clamp(1, GENERATION_QUERY_MAX_WORKERS);
        let mut senders = Vec::with_capacity(workers);
        for worker in 0..workers {
            let (tx, rx) = mpsc::channel::<QueryJob>();
            thread::Builder::new()
                .name(format!("pr-vnext-query-{worker}"))
                .spawn(move || {
                    while let Ok(job) = rx.recv() {
                        job();
                    }
                })
                .expect("failed to create vNext generation query worker");
            senders.push(tx);
        }
        Self { senders }
    }

    fn workers(&self) -> usize {
        self.senders.len()
    }

    fn submit_to<T, F>(&self, worker: usize, job: F) -> Result<mpsc::Receiver<T>, SearchError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let sender = self
            .senders
            .get(worker)
            .ok_or_else(|| SearchError::Format("vNext query worker index out of bounds".into()))?;
        let (tx, rx) = mpsc::sync_channel(1);
        sender
            .send(Box::new(move || {
                let _ = tx.send(job());
            }))
            .map_err(|_| SearchError::Format("vNext query worker pool stopped".into()))?;
        Ok(rx)
    }
}

fn generation_query_pool() -> &'static GenerationQueryPool {
    static POOL: OnceLock<GenerationQueryPool> = OnceLock::new();
    POOL.get_or_init(GenerationQueryPool::new)
}

fn generation_query_profile_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("PR_PROFILE_GENERATION_QUERY").is_some_and(|value| value == "1")
    })
}

/// Role of one immutable vNext generation layer.
///
/// A published snapshot consists of exactly one base layer followed by zero or more delta layers.
/// Each layer may contain multiple `.prseg2` files so the u16 local-ID bound remains local to one
/// segment instead of limiting the logical generation as a whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VNextGenerationLayerKind {
    Base,
    Delta,
}

/// Description of one immutable vNext generation layer.
///
/// `tombstones` use logical document IDs. They must be sorted, unique, and non-zero when the
/// generation is opened. Delta constructors sort/deduplicate caller-provided IDs for convenience;
/// the strict validation remains in `VNextGenerationIndex::open` so persisted/corrupted input can
/// fail closed later without changing the semantic boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VNextGenerationLayerSpec {
    pub kind: VNextGenerationLayerKind,
    pub generation: Generation,
    pub segment_paths: Vec<PathBuf>,
    pub tombstones: Vec<LogicalDocId>,
}

impl VNextGenerationLayerSpec {
    #[must_use]
    pub fn base<I, P>(generation: Generation, segment_paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        Self {
            kind: VNextGenerationLayerKind::Base,
            generation,
            segment_paths: segment_paths.into_iter().map(Into::into).collect(),
            tombstones: Vec::new(),
        }
    }

    #[must_use]
    pub fn delta<I, P>(
        generation: Generation,
        segment_paths: I,
        mut tombstones: Vec<LogicalDocId>,
    ) -> Self
    where
        I: IntoIterator<Item = P>,
        P: Into<PathBuf>,
    {
        tombstones.sort_unstable();
        tombstones.dedup();
        Self {
            kind: VNextGenerationLayerKind::Delta,
            generation,
            segment_paths: segment_paths.into_iter().map(Into::into).collect(),
            tombstones,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VNextVersionLocation {
    segment: usize,
    doc_id: u16,
}

struct VNextGenerationSegment {
    reader: VNextSegmentReader,
    logical_ids: Vec<LogicalDocId>,
    visible: Vec<u8>,
    live_ordinals: Vec<u32>,
}

#[derive(Debug)]
struct GenerationContentPlan {
    folded: Vec<u8>,
    high_hit: bool,
    scan_all: bool,
    zero_hit: bool,
}

#[derive(Debug)]
struct GenerationHighHitWorkerResult {
    bitmap: Vec<u64>,
    physical_hits: usize,
    visible_hits: usize,
    search_ns: u128,
    newest_filter_ns: u128,
}

struct GenerationHighHitJob {
    segment: Arc<VNextGenerationSegment>,
    block_range: Option<(usize, usize)>,
    weight: usize,
}

fn generation_elapsed_ns(started: Option<Instant>) -> u128 {
    started.map_or(0, |started| started.elapsed().as_nanos())
}

/// Read-only multi-segment vNext generation snapshot.
///
/// Visibility is resolved once at open time. Older physical versions remain searchable inside
/// their immutable `.prseg2`, but are masked by `visible` unless they are the newest live location
/// for that logical ID. Query hot paths therefore do not walk tombstone sets or compare generation
/// numbers per hit.
pub struct VNextGenerationIndex {
    generation: Generation,
    layer_count: usize,
    tombstone_events: usize,
    segments: Vec<Arc<VNextGenerationSegment>>,
    live: HashMap<LogicalDocId, VNextVersionLocation>,
    dense_locations: Option<Vec<u64>>,
    live_order: Vec<LogicalDocId>,
}

impl VNextGenerationIndex {
    pub fn open(
        generation: Generation,
        layers: &[VNextGenerationLayerSpec],
    ) -> Result<Self, SearchError> {
        Self::open_impl(generation, layers, false)
    }

    pub(crate) fn open_published(
        generation: Generation,
        layers: &[VNextGenerationLayerSpec],
    ) -> Result<Self, SearchError> {
        Self::open_impl(generation, layers, true)
    }

    fn open_impl(
        generation: Generation,
        layers: &[VNextGenerationLayerSpec],
        published_fast: bool,
    ) -> Result<Self, SearchError> {
        validate_layers(generation, layers)?;

        let mut segments = Vec::<VNextGenerationSegment>::new();
        let mut live = HashMap::<LogicalDocId, VNextVersionLocation>::new();
        let mut tombstone_events = 0usize;

        for layer in layers {
            // Same ordering as Perf12 generation semantics: delete old physical versions first,
            // then let any upsert of the same logical ID in this generation become the live one.
            for &logical_id in &layer.tombstones {
                live.remove(&logical_id);
            }
            tombstone_events = tombstone_events
                .checked_add(layer.tombstones.len())
                .ok_or_else(|| SearchError::Format("vNext tombstone count overflow".into()))?;

            let mut seen_in_layer = HashSet::<LogicalDocId>::new();
            // Segment files inside one immutable layer are independent. Full checksum/structure
            // validation is CPU-heavy, so validate them concurrently while preserving manifest
            // order for deterministic physical locations.
            let readers = open_segment_paths_parallel(&layer.segment_paths, published_fast)?;
            for reader in readers {
                let doc_count = usize::try_from(reader.doc_count())
                    .map_err(|_| SearchError::Format("vNext document count overflow".into()))?;
                let segment_index = segments.len();
                let mut logical_ids = Vec::with_capacity(doc_count);

                for physical in 0..doc_count {
                    let doc_id = u16::try_from(physical).map_err(|_| {
                        SearchError::Format("vNext local document ID overflow".into())
                    })?;
                    let logical_id = reader.logical_id(doc_id)?;
                    if logical_id == 0 {
                        return Err(SearchError::Format(
                            "vNext generation logical ID must be non-zero".into(),
                        ));
                    }
                    if !seen_in_layer.insert(logical_id) {
                        return Err(SearchError::Format(format!(
                            "duplicate logical ID {logical_id} inside vNext generation layer {}",
                            layer.generation
                        )));
                    }
                    logical_ids.push(logical_id);
                    live.insert(
                        logical_id,
                        VNextVersionLocation {
                            segment: segment_index,
                            doc_id,
                        },
                    );
                }

                segments.push(VNextGenerationSegment {
                    reader,
                    visible: vec![0; doc_count],
                    live_ordinals: vec![HIDDEN_LIVE_ORDINAL; doc_count],
                    logical_ids,
                });
            }
        }

        for location in live.values() {
            let segment = segments.get_mut(location.segment).ok_or_else(|| {
                SearchError::Format("vNext live segment location out of bounds".into())
            })?;
            let visible = segment
                .visible
                .get_mut(location.doc_id as usize)
                .ok_or_else(|| {
                    SearchError::Format("vNext live document location out of bounds".into())
                })?;
            *visible = 1;
        }

        let mut live_order = live.keys().copied().collect::<Vec<_>>();
        live_order.sort_unstable();
        for (ordinal, &logical_id) in live_order.iter().enumerate() {
            let location = live.get(&logical_id).ok_or_else(|| {
                SearchError::Format("vNext live logical ID has no physical location".into())
            })?;
            let ordinal = u32::try_from(ordinal)
                .map_err(|_| SearchError::Format("vNext live ordinal exceeds u32".into()))?;
            let segment = segments.get_mut(location.segment).ok_or_else(|| {
                SearchError::Format("vNext live segment location out of bounds".into())
            })?;
            *segment
                .live_ordinals
                .get_mut(location.doc_id as usize)
                .ok_or_else(|| SearchError::Format("vNext live ordinal out of bounds".into()))? =
                ordinal;
        }
        let dense_locations = build_dense_location_table(&live);
        let segments = segments.into_iter().map(Arc::new).collect();

        Ok(Self {
            generation,
            layer_count: layers.len(),
            tombstone_events,
            segments,
            live,
            dense_locations,
            live_order,
        })
    }

    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    #[must_use]
    pub const fn layer_count(&self) -> usize {
        self.layer_count
    }

    #[must_use]
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    #[must_use]
    pub fn live_docs(&self) -> usize {
        self.live.len()
    }

    #[must_use]
    pub const fn tombstone_events(&self) -> usize {
        self.tombstone_events
    }

    #[must_use]
    pub fn contains_logical_id(&self, logical_id: LogicalDocId) -> bool {
        self.live_location(logical_id).is_some()
    }

    #[must_use]
    pub fn live_logical_ids(&self) -> &[LogicalDocId] {
        &self.live_order
    }

    pub fn search_content(
        &self,
        query: impl AsRef<[u8]>,
    ) -> Result<Vec<LogicalDocId>, SearchError> {
        let query = query.as_ref();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let profile_enabled = generation_query_profile_enabled();
        let total_started = profile_enabled.then(Instant::now);
        let plan_started = profile_enabled.then(Instant::now);
        let plan = self.plan_content(query)?;
        let plan_ns = generation_elapsed_ns(plan_started);
        if plan.zero_hit {
            if profile_enabled {
                eprintln!(
                    "VNEXT_GENERATION_QUERY query_len={} high_hit=false scan_all=false zero_hit=true plan_ms={:.6} total_ms={:.6}",
                    query.len(),
                    plan_ns as f64 / 1_000_000.0,
                    generation_elapsed_ns(total_started) as f64 / 1_000_000.0,
                );
            }
            return Ok(Vec::new());
        }
        let hits = if plan.high_hit {
            self.search_content_generation_high_hit(&plan)?
        } else {
            self.search(query, false)?
        };
        if profile_enabled {
            eprintln!(
                "VNEXT_GENERATION_QUERY query_len={} high_hit={} scan_all={} zero_hit=false hits={} plan_ms={:.6} total_ms={:.6}",
                query.len(),
                plan.high_hit,
                plan.scan_all,
                hits.len(),
                plan_ns as f64 / 1_000_000.0,
                generation_elapsed_ns(total_started) as f64 / 1_000_000.0,
            );
        }
        Ok(hits)
    }

    pub fn search_path(&self, query: impl AsRef<[u8]>) -> Result<Vec<LogicalDocId>, SearchError> {
        self.search(query.as_ref(), true)
    }

    pub fn search_name(&self, query: impl AsRef<[u8]>) -> Result<Vec<LogicalDocId>, SearchError> {
        self.search_path(query)
    }

    /// Adaptive first-N search in caller-provided row order.
    ///
    /// For selective queries, the inverted index is still the cheapest way to discover exact hits,
    /// so we materialize that small set and project it into `ordered_logical_ids`. For dense
    /// queries, walking the requested row order and exact-verifying live documents stops as soon as
    /// `limit` hits are known. This is the production GUI path: a 98%-hit query with limit=2000 no
    /// longer materializes ~100k logical IDs just to discard almost all of them.
    pub fn first_n_in_order(
        &self,
        query: impl AsRef<[u8]>,
        names: bool,
        ordered_logical_ids: &[LogicalDocId],
        limit: usize,
    ) -> Result<Vec<LogicalDocId>, SearchError> {
        let query = fold_ascii(query.as_ref());
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        if ordered_logical_ids.len() != self.live_order.len() {
            return Err(SearchError::InvalidArgument(
                "vNext first-N order must contain every live logical document".into(),
            ));
        }

        let estimated = self.estimate_anchor_candidates(&query, names)?;
        if estimated == 0 {
            return Ok(Vec::new());
        }
        let multiplier = if query.len() <= 3 {
            FIRST_N_SHORT_MULTIPLIER
        } else {
            FIRST_N_LONG_MULTIPLIER
        };
        let base = limit.saturating_mul(multiplier);
        let threshold = if query.len() <= 3 {
            base
        } else {
            base.saturating_add(limit.saturating_mul(self.live_docs()).isqrt())
        };

        if estimated <= threshold {
            let hits = if names {
                self.search_path(&query)?
            } else {
                self.search_content(&query)?
            };
            let hit_set = hits.into_iter().collect::<HashSet<_>>();
            let mut out = Vec::with_capacity(limit.min(hit_set.len()));
            for &logical_id in ordered_logical_ids {
                if hit_set.contains(&logical_id) {
                    out.push(logical_id);
                    if out.len() == limit {
                        break;
                    }
                }
            }
            return Ok(out);
        }

        let matcher = ExactMatcher::new(&query);
        let mut out = Vec::with_capacity(limit.min(ordered_logical_ids.len()));
        for &logical_id in ordered_logical_ids {
            if self.logical_document_contains_prepared(logical_id, &query, &matcher, names)? {
                out.push(logical_id);
                if out.len() == limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Adaptive first-N for the common GUI conjunction: path/name AND content.
    ///
    /// If either side is selective, materialize only that exact candidate set and verify the other
    /// predicate in requested row order. If both are dense, verify path first and content second so
    /// the scan stops as soon as `limit` exact conjunction hits are known.
    pub fn first_n_conjunctive_in_order(
        &self,
        path_query: impl AsRef<[u8]>,
        content_query: impl AsRef<[u8]>,
        ordered_logical_ids: &[LogicalDocId],
        limit: usize,
    ) -> Result<Vec<LogicalDocId>, SearchError> {
        let path_query = fold_ascii(path_query.as_ref());
        let content_query = fold_ascii(content_query.as_ref());
        if path_query.is_empty() {
            return self.first_n_in_order(content_query, false, ordered_logical_ids, limit);
        }
        if content_query.is_empty() {
            return self.first_n_in_order(path_query, true, ordered_logical_ids, limit);
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        if ordered_logical_ids.len() != self.live_order.len() {
            return Err(SearchError::InvalidArgument(
                "vNext first-N conjunction order must contain every live logical document".into(),
            ));
        }

        let path_estimated = self.estimate_anchor_candidates(&path_query, true)?;
        let content_estimated = self.estimate_anchor_candidates(&content_query, false)?;
        if path_estimated == 0 || content_estimated == 0 {
            return Ok(Vec::new());
        }
        let threshold = limit
            .saturating_mul(FIRST_N_LONG_MULTIPLIER)
            .saturating_add(limit.saturating_mul(self.live_docs()).isqrt());
        let path_matcher = ExactMatcher::new(&path_query);
        let content_matcher = ExactMatcher::new(&content_query);

        let selective = path_estimated.min(content_estimated) <= threshold;
        if selective {
            let path_is_selective = path_estimated <= content_estimated;
            let candidates = if path_is_selective {
                self.search_path(&path_query)?
            } else {
                self.search_content(&content_query)?
            };
            let candidate_set = candidates.into_iter().collect::<HashSet<_>>();
            let mut out = Vec::with_capacity(limit.min(candidate_set.len()));
            for &logical_id in ordered_logical_ids {
                if !candidate_set.contains(&logical_id) {
                    continue;
                }
                let Some(location) = self.live_location(logical_id) else {
                    continue;
                };
                let segment = self.segments.get(location.segment).ok_or_else(|| {
                    SearchError::Format("vNext live segment location out of bounds".into())
                })?;
                let other_matches = if path_is_selective {
                    segment
                        .reader
                        .content_contains_prepared(location.doc_id, &content_matcher)?
                } else {
                    segment.reader.path_contains_prepared(
                        location.doc_id,
                        &path_query,
                        &path_matcher,
                    )?
                };
                if other_matches {
                    out.push(logical_id);
                    if out.len() == limit {
                        break;
                    }
                }
            }
            return Ok(out);
        }

        let mut out = Vec::with_capacity(limit.min(ordered_logical_ids.len()));
        for &logical_id in ordered_logical_ids {
            let Some(location) = self.live_location(logical_id) else {
                continue;
            };
            let segment = self.segments.get(location.segment).ok_or_else(|| {
                SearchError::Format("vNext live segment location out of bounds".into())
            })?;
            if !segment.reader.path_contains_prepared(
                location.doc_id,
                &path_query,
                &path_matcher,
            )? {
                continue;
            }
            if segment
                .reader
                .content_contains_prepared(location.doc_id, &content_matcher)?
            {
                out.push(logical_id);
                if out.len() == limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    fn live_location(&self, logical_id: LogicalDocId) -> Option<VNextVersionLocation> {
        if let Some(table) = &self.dense_locations {
            let index = usize::try_from(logical_id).ok()?;
            let encoded = *table.get(index)?;
            if encoded == DENSE_LOCATION_EMPTY {
                return None;
            }
            return Some(VNextVersionLocation {
                segment: usize::try_from(encoded >> 16).ok()?,
                doc_id: encoded as u16,
            });
        }
        self.live.get(&logical_id).copied()
    }

    fn logical_document_contains_prepared(
        &self,
        logical_id: LogicalDocId,
        folded_query: &[u8],
        matcher: &ExactMatcher<'_>,
        names: bool,
    ) -> Result<bool, SearchError> {
        let Some(location) = self.live_location(logical_id) else {
            return Ok(false);
        };
        let segment = self.segments.get(location.segment).ok_or_else(|| {
            SearchError::Format("vNext live segment location out of bounds".into())
        })?;
        if names {
            segment
                .reader
                .path_contains_prepared(location.doc_id, folded_query, matcher)
        } else {
            segment
                .reader
                .content_contains_prepared(location.doc_id, matcher)
        }
    }

    fn estimate_anchor_candidates(&self, folded: &[u8], names: bool) -> Result<usize, SearchError> {
        if folded.is_empty() {
            return Ok(0);
        }
        if folded.len() <= 2 {
            return self.segments.iter().try_fold(0usize, |total, segment| {
                let posting = if names {
                    segment.reader.path_short_posting(folded)?
                } else {
                    segment.reader.content_short_posting(folded)?
                };
                total
                    .checked_add(posting.len())
                    .ok_or_else(|| SearchError::Format("vNext first-N cardinality overflow".into()))
            });
        }

        let mut grams = folded
            .windows(3)
            .map(|window| [window[0], window[1], window[2]])
            .collect::<Vec<_>>();
        grams.sort_unstable();
        grams.dedup();
        let mut rarest = usize::MAX;
        for gram in grams {
            let total = self.segments.iter().try_fold(0usize, |total, segment| {
                let cardinality = if names {
                    segment.reader.path_q3_posting(gram)?.len()
                } else {
                    segment.reader.q3_posting(gram)?.len()
                };
                total.checked_add(cardinality).ok_or_else(|| {
                    SearchError::Format("vNext first-N q3 cardinality overflow".into())
                })
            })?;
            if total == 0 {
                return Ok(0);
            }
            rarest = rarest.min(total);
        }
        Ok(rarest)
    }

    fn plan_content(&self, query: &[u8]) -> Result<GenerationContentPlan, SearchError> {
        let folded = fold_ascii(query);
        if folded.len() <= 3 || self.segments.len() < 2 {
            return Ok(GenerationContentPlan {
                folded,
                high_hit: false,
                scan_all: false,
                zero_hit: false,
            });
        }
        let mut grams = folded
            .windows(3)
            .map(|window| [window[0], window[1], window[2]])
            .collect::<Vec<_>>();
        grams.sort_unstable();
        grams.dedup();
        let mut rarest_total = usize::MAX;
        for gram in grams {
            let total = self.segments.iter().try_fold(0usize, |total, segment| {
                total
                    .checked_add(segment.reader.q3_posting(gram)?.len())
                    .ok_or_else(|| {
                        SearchError::Format("vNext generation q3 cardinality overflow".into())
                    })
            })?;
            if total == 0 {
                return Ok(GenerationContentPlan {
                    folded,
                    high_hit: false,
                    scan_all: false,
                    zero_hit: true,
                });
            }
            rarest_total = rarest_total.min(total);
        }
        let total_blocks = self.segments.iter().try_fold(0usize, |total, segment| {
            total
                .checked_add(segment.reader.block_count() as usize)
                .ok_or_else(|| SearchError::Format("vNext generation block count overflow".into()))
        })?;
        let high_hit = rarest_total >= GENERATION_HIGH_HIT_BLOCKS;
        let total_content_bytes = if high_hit {
            self.segments.iter().try_fold(0usize, |total, segment| {
                total
                    .checked_add(segment.reader.content_blob_len()?)
                    .ok_or_else(|| {
                        SearchError::Format("vNext generation content size overflow".into())
                    })
            })?
        } else {
            0
        };
        let compact_documents = self.live_docs() > 0
            && total_content_bytes
                <= self
                    .live_docs()
                    .saturating_mul(GENERATION_BLOB_SCAN_MAX_AVG_DOC_BYTES);
        let scan_all = high_hit
            && compact_documents
            && total_blocks > 0
            && rarest_total.saturating_mul(4) >= total_blocks.saturating_mul(3);
        Ok(GenerationContentPlan {
            folded,
            high_hit,
            scan_all,
            zero_hit: false,
        })
    }

    fn plan_generation_high_hit_jobs(
        &self,
        scan_all: bool,
        workers: usize,
    ) -> Result<Vec<GenerationHighHitJob>, SearchError> {
        let total_blocks = self.segments.iter().try_fold(0usize, |total, segment| {
            total
                .checked_add(segment.reader.block_count() as usize)
                .ok_or_else(|| SearchError::Format("vNext generation block count overflow".into()))
        })?;
        let target_blocks = total_blocks
            .div_ceil(workers.max(1))
            .max(GENERATION_HIGH_HIT_MIN_CHUNK_BLOCKS);
        let mut jobs = Vec::new();
        for segment in &self.segments {
            let blocks = segment.reader.block_count() as usize;
            if scan_all && segment.reader.all_docs_single_block() && blocks > 0 {
                let chunks = blocks.div_ceil(target_blocks).max(1);
                let chunk_blocks = blocks.div_ceil(chunks);
                let mut start = 0usize;
                while start < blocks {
                    let end = start.saturating_add(chunk_blocks).min(blocks);
                    jobs.push(GenerationHighHitJob {
                        segment: Arc::clone(segment),
                        block_range: Some((start, end)),
                        weight: end - start,
                    });
                    start = end;
                }
            } else {
                jobs.push(GenerationHighHitJob {
                    segment: Arc::clone(segment),
                    block_range: None,
                    weight: blocks.max(1),
                });
            }
        }
        jobs.sort_unstable_by_key(|job| std::cmp::Reverse(job.weight));
        Ok(jobs)
    }

    fn search_content_generation_high_hit(
        &self,
        plan: &GenerationContentPlan,
    ) -> Result<Vec<LogicalDocId>, SearchError> {
        debug_assert!(plan.high_hit);
        let profile_enabled = generation_query_profile_enabled();
        let total_started = profile_enabled.then(Instant::now);
        let pool = generation_query_pool();
        if pool.workers() <= 1 || self.segments.len() <= 1 {
            return self.search(&plan.folded, false);
        }

        let submit_started = profile_enabled.then(Instant::now);
        let bitmap_words = self.live_order.len().div_ceil(64);
        let jobs = self.plan_generation_high_hit_jobs(plan.scan_all, pool.workers())?;
        let job_count = jobs.len();
        let mut planned_loads = vec![0usize; pool.workers()];
        let mut receivers = Vec::with_capacity(job_count);
        for job in jobs {
            let worker = planned_loads
                .iter()
                .enumerate()
                .min_by_key(|(_, load)| **load)
                .map(|(worker, _)| worker)
                .unwrap_or(0);
            planned_loads[worker] = planned_loads[worker].saturating_add(job.weight);
            let folded = plan.folded.clone();
            receivers.push(pool.submit_to(
                worker,
                move || -> Result<GenerationHighHitWorkerResult, SearchError> {
                    let segment = job.segment;
                    let search_started = profile_enabled.then(Instant::now);
                    if let Some((start_block, end_block)) = job.block_range {
                        let mut bitmap = vec![0u64; bitmap_words];
                        let mut visible_hits = 0usize;
                        let scanned = segment.reader.scan_content_blob_single_block_range_visit(
                            &folded,
                            start_block,
                            end_block,
                            None,
                            |doc_id| {
                                let ordinal = segment.live_ordinals[doc_id as usize];
                                if ordinal == HIDDEN_LIVE_ORDINAL {
                                    return;
                                }
                                let ordinal = ordinal as usize;
                                bitmap[ordinal / 64] |= 1u64 << (ordinal % 64);
                                visible_hits += 1;
                            },
                        )?;
                        if let Some(physical_hits) = scanned {
                            return Ok(GenerationHighHitWorkerResult {
                                bitmap,
                                physical_hits,
                                visible_hits,
                                search_ns: generation_elapsed_ns(search_started),
                                newest_filter_ns: 0,
                            });
                        }
                    }

                    generation_high_hit_index_worker(
                        &segment,
                        &folded,
                        bitmap_words,
                        profile_enabled,
                        search_started,
                    )
                },
            )?);
        }
        let max_planned_blocks = planned_loads.iter().copied().max().unwrap_or(0);
        let submit_ns = generation_elapsed_ns(submit_started);

        let merge_started = profile_enabled.then(Instant::now);
        let mut bitmap = vec![0u64; bitmap_words];
        let mut physical_hits = 0usize;
        let mut visible_hits = 0usize;
        let mut worker_search_ns = 0u128;
        let mut newest_filter_ns = 0u128;
        let mut wait_ns = 0u128;
        let mut bitmap_or_ns = 0u128;
        for receiver in receivers {
            let wait_started = profile_enabled.then(Instant::now);
            let worker = receiver
                .recv()
                .map_err(|_| SearchError::Format("vNext query worker result lost".into()))??;
            wait_ns = wait_ns.saturating_add(generation_elapsed_ns(wait_started));
            physical_hits = physical_hits.saturating_add(worker.physical_hits);
            visible_hits = visible_hits.saturating_add(worker.visible_hits);
            worker_search_ns = worker_search_ns.saturating_add(worker.search_ns);
            newest_filter_ns = newest_filter_ns.saturating_add(worker.newest_filter_ns);
            if worker.bitmap.len() != bitmap.len() {
                return Err(SearchError::Format(
                    "vNext generation worker bitmap length mismatch".into(),
                ));
            }
            let bitmap_or_started = profile_enabled.then(Instant::now);
            for (dst, src) in bitmap.iter_mut().zip(worker.bitmap) {
                *dst |= src;
            }
            bitmap_or_ns = bitmap_or_ns.saturating_add(generation_elapsed_ns(bitmap_or_started));
        }
        let merge_ns = generation_elapsed_ns(merge_started);
        let materialize_started = profile_enabled.then(Instant::now);
        let result_hits = bitmap
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum::<usize>();
        let out = materialize_live_hit_bitmap(&self.live_order, &bitmap, result_hits)?;
        let materialize_ns = generation_elapsed_ns(materialize_started);
        if profile_enabled {
            eprintln!(
                "VNEXT_GENERATION_HIGH_HIT segments={} workers={} jobs={} max_planned_blocks={} scan_all={} physical_hits={} visible_hits={} result_hits={} submit_ms={:.6} worker_search_sum_ms={:.6} newest_filter_sum_ms={:.6} wait_ms={:.6} bitmap_or_ms={:.6} generation_merge_ms={:.6} materialize_ms={:.6} total_ms={:.6}",
                self.segments.len(),
                pool.workers(),
                job_count,
                max_planned_blocks,
                plan.scan_all,
                physical_hits,
                visible_hits,
                out.len(),
                submit_ns as f64 / 1_000_000.0,
                worker_search_ns as f64 / 1_000_000.0,
                newest_filter_ns as f64 / 1_000_000.0,
                wait_ns as f64 / 1_000_000.0,
                bitmap_or_ns as f64 / 1_000_000.0,
                merge_ns as f64 / 1_000_000.0,
                materialize_ns as f64 / 1_000_000.0,
                generation_elapsed_ns(total_started) as f64 / 1_000_000.0,
            );
        }
        Ok(out)
    }

    /// Materialize the live logical snapshot in stable logical-ID order.
    ///
    /// This intentionally copies payload bytes. It is not a query hot path; it is the correctness
    /// bridge needed by later full-rebuild/compaction work to prove that a multi-generation
    /// snapshot has exactly the same logical documents as a freshly rebuilt base segment.
    pub fn materialize_live_documents(&self) -> Result<Vec<VNextDocumentInput>, SearchError> {
        let mut out = Vec::with_capacity(self.live_order.len());
        for &logical_id in &self.live_order {
            let location = self.live.get(&logical_id).ok_or_else(|| {
                SearchError::Format("vNext live logical ID has no physical location".into())
            })?;
            let segment = self.segments.get(location.segment).ok_or_else(|| {
                SearchError::Format("vNext live segment location out of bounds".into())
            })?;
            out.push(VNextDocumentInput::new(
                logical_id,
                segment.reader.display_path(location.doc_id)?.to_owned(),
                segment.reader.normalized_content(location.doc_id)?.to_vec(),
            ));
        }
        Ok(out)
    }

    fn search(&self, query: &[u8], paths: bool) -> Result<Vec<LogicalDocId>, SearchError> {
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for segment in &self.segments {
            let hits = if paths {
                segment.reader.search_path(query)?
            } else {
                segment.reader.search_content(query)?
            };
            append_visible_hits(segment, &hits, &mut out)?;
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }
}

fn open_segment_paths_parallel(
    paths: &[PathBuf],
    published_fast: bool,
) -> Result<Vec<VNextSegmentReader>, SearchError> {
    if paths.len() <= 1 {
        return paths
            .iter()
            .map(|path| {
                if published_fast {
                    VNextSegmentReader::open_published(path)
                } else {
                    VNextSegmentReader::open(path)
                }
            })
            .collect();
    }
    let workers = thread::available_parallelism()
        .map_or(1, usize::from)
        .min(GENERATION_OPEN_MAX_WORKERS)
        .min(paths.len());
    if workers <= 1 {
        return paths
            .iter()
            .map(|path| {
                if published_fast {
                    VNextSegmentReader::open_published(path)
                } else {
                    VNextSegmentReader::open(path)
                }
            })
            .collect();
    }
    let chunk = paths.len().div_ceil(workers);
    let partials = thread::scope(|scope| {
        paths
            .chunks(chunk)
            .enumerate()
            .map(|(chunk_index, path_chunk)| {
                scope.spawn(
                    move || -> Result<Vec<(usize, VNextSegmentReader)>, SearchError> {
                        let start = chunk_index * chunk;
                        path_chunk
                            .iter()
                            .enumerate()
                            .map(|(offset, path)| {
                                let reader = if published_fast {
                                    VNextSegmentReader::open_published(path)?
                                } else {
                                    VNextSegmentReader::open(path)?
                                };
                                Ok((start + offset, reader))
                            })
                            .collect()
                    },
                )
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| {
                handle.join().map_err(|_| {
                    SearchError::Format("vNext generation open worker panicked".into())
                })?
            })
            .collect::<Result<Vec<_>, _>>()
    })?;
    let mut opened = partials.into_iter().flatten().collect::<Vec<_>>();
    opened.sort_by_key(|(index, _)| *index);
    Ok(opened.into_iter().map(|(_, reader)| reader).collect())
}

fn append_visible_hits(
    segment: &VNextGenerationSegment,
    hits: &[u16],
    out: &mut Vec<LogicalDocId>,
) -> Result<(), SearchError> {
    for &doc_id in hits {
        let index = doc_id as usize;
        if segment.visible.get(index).copied() != Some(1) {
            continue;
        }
        let logical_id = *segment
            .logical_ids
            .get(index)
            .ok_or_else(|| SearchError::Format("vNext query hit outside logical-ID map".into()))?;
        out.push(logical_id);
    }
    Ok(())
}

#[inline(never)]
fn generation_high_hit_index_worker(
    segment: &VNextGenerationSegment,
    folded: &[u8],
    bitmap_words: usize,
    profile_enabled: bool,
    search_started: Option<Instant>,
) -> Result<GenerationHighHitWorkerResult, SearchError> {
    let hits = segment.reader.search_content_generation_worker(folded)?;
    let search_ns = generation_elapsed_ns(search_started);
    let physical_hits = hits.len();
    let filter_started = profile_enabled.then(Instant::now);
    let (bitmap, visible_hits) = visible_hit_bitmap(segment, &hits, bitmap_words)?;
    let newest_filter_ns = generation_elapsed_ns(filter_started);
    Ok(GenerationHighHitWorkerResult {
        bitmap,
        physical_hits,
        visible_hits,
        search_ns,
        newest_filter_ns,
    })
}

fn visible_hit_bitmap(
    segment: &VNextGenerationSegment,
    hits: &[u16],
    bitmap_words: usize,
) -> Result<(Vec<u64>, usize), SearchError> {
    let mut bitmap = vec![0u64; bitmap_words];
    let mut visible_hits = 0usize;
    for &doc_id in hits {
        let index = doc_id as usize;
        let ordinal = *segment
            .live_ordinals
            .get(index)
            .ok_or_else(|| SearchError::Format("vNext query hit outside ordinal map".into()))?;
        if ordinal == HIDDEN_LIVE_ORDINAL {
            continue;
        }
        let ordinal = ordinal as usize;
        let word = bitmap.get_mut(ordinal / 64).ok_or_else(|| {
            SearchError::Format("vNext live ordinal outside dense hit bitmap".into())
        })?;
        *word |= 1u64 << (ordinal % 64);
        visible_hits = visible_hits.saturating_add(1);
    }
    Ok((bitmap, visible_hits))
}

fn materialize_live_hit_bitmap(
    live_order: &[LogicalDocId],
    bitmap: &[u64],
    result_hits: usize,
) -> Result<Vec<LogicalDocId>, SearchError> {
    if bitmap.len() != live_order.len().div_ceil(64) {
        return Err(SearchError::Format(
            "vNext generation hit bitmap length mismatch".into(),
        ));
    }
    if result_hits > live_order.len() {
        return Err(SearchError::Format(
            "vNext generation hit bitmap cardinality overflow".into(),
        ));
    }
    if result_hits == live_order.len() {
        return Ok(live_order.to_vec());
    }

    let mut out = Vec::with_capacity(result_hits);
    if result_hits.saturating_mul(4) <= live_order.len() {
        for (word_index, &word) in bitmap.iter().enumerate() {
            let mut remaining = word;
            while remaining != 0 {
                let bit = remaining.trailing_zeros() as usize;
                let ordinal = word_index * 64 + bit;
                if let Some(&logical_id) = live_order.get(ordinal) {
                    out.push(logical_id);
                }
                remaining &= remaining - 1;
            }
        }
    } else {
        for (ordinal, &logical_id) in live_order.iter().enumerate() {
            if bitmap[ordinal / 64] & (1u64 << (ordinal % 64)) != 0 {
                out.push(logical_id);
            }
        }
    }
    if out.len() != result_hits {
        return Err(SearchError::Format(
            "vNext generation hit bitmap cardinality mismatch".into(),
        ));
    }
    Ok(out)
}

fn build_dense_location_table(
    live: &HashMap<LogicalDocId, VNextVersionLocation>,
) -> Option<Vec<u64>> {
    let max_id = live.keys().copied().max()?;
    let max_index = usize::try_from(max_id).ok()?;
    let dense_limit = live
        .len()
        .saturating_mul(DENSE_LOCATION_MAX_MULTIPLIER)
        .max(1024);
    if max_index > dense_limit {
        return None;
    }
    let mut table = vec![DENSE_LOCATION_EMPTY; max_index.saturating_add(1)];
    for (&logical_id, &location) in live {
        let index = usize::try_from(logical_id).ok()?;
        let segment = u64::try_from(location.segment).ok()?;
        if segment > (u64::MAX >> 16) {
            return None;
        }
        table[index] = (segment << 16) | u64::from(location.doc_id);
    }
    Some(table)
}

fn validate_layers(
    generation: Generation,
    layers: &[VNextGenerationLayerSpec],
) -> Result<(), SearchError> {
    let first = layers
        .first()
        .ok_or_else(|| SearchError::InvalidArgument("vNext generation has no base layer".into()))?;
    if first.kind != VNextGenerationLayerKind::Base {
        return Err(SearchError::Format(
            "vNext generation must start with a base layer".into(),
        ));
    }
    if !first.tombstones.is_empty() {
        return Err(SearchError::Format(
            "vNext base layer must not contain tombstones".into(),
        ));
    }

    let mut previous = None;
    for (index, layer) in layers.iter().enumerate() {
        if index > 0 && layer.kind != VNextGenerationLayerKind::Delta {
            return Err(SearchError::Format(
                "vNext generation may contain only one base layer".into(),
            ));
        }
        if previous.is_some_and(|value| layer.generation <= value) {
            return Err(SearchError::Format(
                "vNext generation layers must be strictly increasing".into(),
            ));
        }
        if layer.generation > generation {
            return Err(SearchError::Format(
                "vNext layer generation exceeds published generation".into(),
            ));
        }
        validate_tombstones(&layer.tombstones)?;
        previous = Some(layer.generation);
    }
    if previous != Some(generation) {
        return Err(SearchError::Format(
            "vNext published generation must equal newest layer generation".into(),
        ));
    }
    Ok(())
}

fn validate_tombstones(tombstones: &[LogicalDocId]) -> Result<(), SearchError> {
    let mut previous = None;
    for &logical_id in tombstones {
        if logical_id == 0 || previous.is_some_and(|value| logical_id <= value) {
            return Err(SearchError::Format(
                "vNext tombstones must be sorted unique non-zero logical IDs".into(),
            ));
        }
        previous = Some(logical_id);
    }
    Ok(())
}

#[cfg(test)]
mod high_hit_materialization_tests {
    use super::materialize_live_hit_bitmap;

    #[test]
    fn high_hit_bitmap_materialization_handles_all_dense_and_sparse_hits() {
        let live = (1u64..=130).collect::<Vec<_>>();

        let mut all = vec![u64::MAX; live.len().div_ceil(64)];
        let trailing = live.len() % 64;
        if trailing != 0 {
            *all.last_mut().unwrap() = (1u64 << trailing) - 1;
        }
        assert_eq!(
            materialize_live_hit_bitmap(&live, &all, live.len()).unwrap(),
            live
        );

        let mut dense = vec![0u64; live.len().div_ceil(64)];
        let expected_dense = live
            .iter()
            .copied()
            .filter(|id| id % 10 != 0)
            .collect::<Vec<_>>();
        for &id in &expected_dense {
            let ordinal = id as usize - 1;
            dense[ordinal / 64] |= 1u64 << (ordinal % 64);
        }
        assert_eq!(
            materialize_live_hit_bitmap(&live, &dense, expected_dense.len()).unwrap(),
            expected_dense
        );

        let mut sparse = vec![0u64; live.len().div_ceil(64)];
        let expected_sparse = vec![1u64, 65, 130];
        for &id in &expected_sparse {
            let ordinal = id as usize - 1;
            sparse[ordinal / 64] |= 1u64 << (ordinal % 64);
        }
        assert_eq!(
            materialize_live_hit_bitmap(&live, &sparse, expected_sparse.len()).unwrap(),
            expected_sparse
        );
    }
}
