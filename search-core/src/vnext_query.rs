use crate::fold_ascii;
use crate::format::SearchError;
use crate::index::ExactMatcher;
use crate::vnext_q3::{VNextQ3Posting, VNextQ3PostingEncoding};
use crate::vnext_query_simd::intersect_direct_q3_postings;
use crate::vnext_segment::VNextSegmentReader;
use std::sync::OnceLock;
use std::time::Instant;

const MULTI_ANCHOR_SAMPLE_BLOCKS: usize = 64;
const MULTI_ANCHOR_MIN_PRIMARY_BLOCKS: usize = 64;
const MULTI_ANCHOR_CORPUS_DIVISOR: usize = 8;
const MULTI_ANCHOR_MIN_REDUCTION_NUMERATOR: usize = 7;
const MULTI_ANCHOR_MIN_REDUCTION_DENOMINATOR: usize = 8;
const HIGH_HIT_PARALLEL_BLOCKS: usize = 8_192;
const HIGH_HIT_MAX_WORKERS: usize = 4;

fn query_profile_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("PR_PROFILE_QUERY").is_some_and(|value| value == "1"))
}

fn elapsed_ns(started: Option<Instant>) -> u128 {
    started.map_or(0, |started| started.elapsed().as_nanos())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VNextContentPlanMode {
    Empty,
    ShortScan,
    ShortIndex,
    Q3Anchor,
    ZeroHit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VNextContentSearchDiagnostics {
    pub mode: VNextContentPlanMode,
    pub query_len: usize,
    pub anchor: Option<[u8; 3]>,
    pub anchor_offset: usize,
    pub anchor_blocks: usize,
    pub selected_anchor_count: u8,
    pub candidate_blocks: usize,
    pub verified_blocks: usize,
    pub hits: usize,
}

impl VNextContentSearchDiagnostics {
    const fn empty(query_len: usize) -> Self {
        Self {
            mode: VNextContentPlanMode::Empty,
            query_len,
            anchor: None,
            anchor_offset: 0,
            anchor_blocks: 0,
            selected_anchor_count: 0,
            candidate_blocks: 0,
            verified_blocks: 0,
            hits: 0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Q3AnchorCandidate {
    offset: usize,
    gram: [u8; 3],
    cardinality: usize,
}

impl VNextSegmentReader {
    /// Exact content search using the vNext block q3 index.
    ///
    /// Query bytes use the same ASCII-folded semantics as the frozen Perf12 path.
    /// One- and two-byte queries use the persistent q1/q2 indexes. q3+ queries select the
    /// rarest trigram as the primary anchor. When that posting is common, the planner samples
    /// additional selective trigrams and may add a second/third anchor before exact verification.
    pub fn search_content(&self, query: impl AsRef<[u8]>) -> Result<Vec<u16>, SearchError> {
        let query = fold_ascii(query.as_ref());
        if query.is_empty() {
            return Ok(Vec::new());
        }
        if query.len() <= 2 {
            return self.search_short_content(&query).map(|(hits, _)| hits);
        }
        self.search_content_q3_impl::<false, true>(query)
            .map(|(hits, _)| hits)
    }

    pub(crate) fn search_content_generation_worker(
        &self,
        folded_query: &[u8],
    ) -> Result<Vec<u16>, SearchError> {
        if folded_query.is_empty() {
            return Ok(Vec::new());
        }
        if folded_query.len() <= 2 {
            return self
                .search_short_content(folded_query)
                .map(|(hits, _)| hits);
        }
        self.search_content_q3_impl::<false, false>(folded_query.to_vec())
            .map(|(hits, _)| hits)
    }

    pub fn search_content_with_diagnostics(
        &self,
        query: impl AsRef<[u8]>,
    ) -> Result<(Vec<u16>, VNextContentSearchDiagnostics), SearchError> {
        let query = fold_ascii(query.as_ref());
        if query.is_empty() {
            return Ok((Vec::new(), VNextContentSearchDiagnostics::empty(0)));
        }
        if query.len() <= 2 {
            return self.search_short_content(&query);
        }
        if query_profile_enabled() {
            self.search_content_q3_impl::<true, true>(query)
        } else {
            self.search_content_q3_impl::<false, true>(query)
        }
    }

    fn search_short_content(
        &self,
        query: &[u8],
    ) -> Result<(Vec<u16>, VNextContentSearchDiagnostics), SearchError> {
        debug_assert!(!query.is_empty() && query.len() <= 2);
        let posting = self.content_short_posting(query)?;
        let mut hits = Vec::new();
        let mut last_doc = None;
        for block_id in posting.iter() {
            let doc_id = self.block(block_id)?.doc_id;
            if last_doc != Some(doc_id) {
                hits.push(doc_id);
                last_doc = Some(doc_id);
            }
        }
        let diagnostics = VNextContentSearchDiagnostics {
            mode: VNextContentPlanMode::ShortIndex,
            query_len: query.len(),
            anchor: None,
            anchor_offset: 0,
            anchor_blocks: posting.len(),
            selected_anchor_count: 1,
            candidate_blocks: posting.len(),
            verified_blocks: 0,
            hits: hits.len(),
        };
        Ok((hits, diagnostics))
    }

    fn search_content_q3_impl<const PROFILE: bool, const ALLOW_PARALLEL: bool>(
        &self,
        query: Vec<u8>,
    ) -> Result<(Vec<u16>, VNextContentSearchDiagnostics), SearchError> {
        debug_assert!(query.len() >= 3);
        let total_started = PROFILE.then(Instant::now);
        let anchor_started = PROFILE.then(Instant::now);
        let anchors = self.collect_q3_anchor_candidates(&query)?;
        let anchor_ns = elapsed_ns(anchor_started);
        if let Some(absent) = anchors.iter().find(|anchor| anchor.cardinality == 0) {
            return Ok((
                Vec::new(),
                VNextContentSearchDiagnostics {
                    mode: VNextContentPlanMode::ZeroHit,
                    query_len: query.len(),
                    anchor: Some(absent.gram),
                    anchor_offset: absent.offset,
                    anchor_blocks: 0,
                    selected_anchor_count: 1,
                    candidate_blocks: 0,
                    verified_blocks: 0,
                    hits: 0,
                },
            ));
        }

        let primary = *anchors.first().ok_or_else(|| {
            SearchError::Format("vNext q3 planner failed to select anchor".into())
        })?;
        let primary_posting = self.q3_posting(primary.gram)?;
        debug_assert_eq!(primary_posting.len(), primary.cardinality);

        let extra_started = PROFILE.then(Instant::now);
        let extra_anchors = self.select_extra_q3_anchors(primary, &anchors, primary_posting)?;
        let extra_ns = elapsed_ns(extra_started);
        let intersection_started = PROFILE.then(Instant::now);
        let filtered_blocks = if extra_anchors.is_empty() {
            None
        } else if self.all_docs_single_block() {
            let postings = extra_anchors
                .iter()
                .map(|(_, posting)| *posting)
                .collect::<Vec<_>>();
            Some(intersect_direct_q3_postings(primary_posting, &postings))
        } else {
            Some(self.intersect_q3_anchor_blocks(primary, primary_posting, &extra_anchors)?)
        };
        let intersection_ns = elapsed_ns(intersection_started);
        let candidate_blocks = filtered_blocks
            .as_ref()
            .map_or(primary.cardinality, Vec::len);

        let matcher = (query.len() > 3).then(|| ExactMatcher::new(&query));
        let verify_started = PROFILE.then(Instant::now);
        let (mut hits, verified_blocks) = if ALLOW_PARALLEL
            && self.all_docs_single_block()
            && query.len() > 3
            && candidate_blocks >= HIGH_HIT_PARALLEL_BLOCKS
        {
            let blocks = filtered_blocks
                .as_ref()
                .map_or_else(|| primary_posting.iter().collect::<Vec<_>>(), Clone::clone);
            self.verify_single_block_candidates_parallel(
                &blocks,
                matcher.as_ref().expect("long query matcher"),
            )?
        } else if let Some(blocks) = filtered_blocks.as_ref() {
            self.verify_content_candidates(
                blocks.iter().copied(),
                primary.offset,
                &query,
                matcher.as_ref(),
            )?
        } else {
            self.verify_content_candidates(
                primary_posting.iter(),
                primary.offset,
                &query,
                matcher.as_ref(),
            )?
        };
        if !self.all_docs_single_block() {
            hits.sort_unstable();
            hits.dedup();
        }
        let verify_ns = elapsed_ns(verify_started);

        let diagnostics = VNextContentSearchDiagnostics {
            mode: VNextContentPlanMode::Q3Anchor,
            query_len: query.len(),
            anchor: Some(primary.gram),
            anchor_offset: primary.offset,
            anchor_blocks: primary.cardinality,
            selected_anchor_count: 1 + u8::try_from(extra_anchors.len()).unwrap_or(2),
            candidate_blocks,
            verified_blocks,
            hits: hits.len(),
        };
        if PROFILE {
            let (raw, dense, singleton) = posting_encoding_counts(primary_posting, &extra_anchors);
            eprintln!(
                "VNEXT_QUERY_PHASE kind=content query_len={} mode={:?} anchor_blocks={} selected_anchors={} candidate_blocks={} verified_blocks={} raw_postings={} dense_postings={} singleton_postings={} anchor_ms={:.6} extra_anchor_ms={:.6} intersection_ms={:.6} verify_ms={:.6} total_ms={:.6}",
                query.len(),
                diagnostics.mode,
                diagnostics.anchor_blocks,
                diagnostics.selected_anchor_count,
                diagnostics.candidate_blocks,
                diagnostics.verified_blocks,
                raw,
                dense,
                singleton,
                anchor_ns as f64 / 1_000_000.0,
                extra_ns as f64 / 1_000_000.0,
                intersection_ns as f64 / 1_000_000.0,
                verify_ns as f64 / 1_000_000.0,
                elapsed_ns(total_started) as f64 / 1_000_000.0,
            );
        }
        Ok((hits, diagnostics))
    }

    fn collect_q3_anchor_candidates(
        &self,
        query: &[u8],
    ) -> Result<Vec<Q3AnchorCandidate>, SearchError> {
        let mut anchors = Vec::<Q3AnchorCandidate>::with_capacity(query.len().saturating_sub(2));
        for (offset, gram) in query.windows(3).enumerate() {
            let gram = [gram[0], gram[1], gram[2]];
            if anchors.iter().any(|anchor| anchor.gram == gram) {
                continue;
            }
            anchors.push(Q3AnchorCandidate {
                offset,
                gram,
                cardinality: self.q3_posting(gram)?.len(),
            });
        }
        anchors.sort_by_key(|anchor| (anchor.cardinality, anchor.offset));
        Ok(anchors)
    }

    fn select_extra_q3_anchors<'a>(
        &'a self,
        primary: Q3AnchorCandidate,
        anchors: &[Q3AnchorCandidate],
        primary_posting: VNextQ3Posting<'a>,
    ) -> Result<Vec<(Q3AnchorCandidate, VNextQ3Posting<'a>)>, SearchError> {
        let trigger = MULTI_ANCHOR_MIN_PRIMARY_BLOCKS
            .max((self.block_count() as usize / MULTI_ANCHOR_CORPUS_DIVISOR).max(1));
        if primary.cardinality < trigger || anchors.len() < 2 {
            return Ok(Vec::new());
        }

        let mut sample = sample_posting_evenly(primary_posting, MULTI_ANCHOR_SAMPLE_BLOCKS);
        if sample.len() < 2 {
            return Ok(Vec::new());
        }

        let mut selected = Vec::with_capacity(2);
        for anchor in anchors.iter().skip(1).take(2) {
            if selected.len() == 2 {
                break;
            }
            let posting = self.q3_posting(anchor.gram)?;
            let before = sample.len();
            let mut survivors = Vec::with_capacity(before);
            for &block_id in &sample {
                if self.q3_anchor_can_align(block_id, primary.offset, anchor.offset, posting)? {
                    survivors.push(block_id);
                }
            }
            if survivors.len() * MULTI_ANCHOR_MIN_REDUCTION_DENOMINATOR
                <= before * MULTI_ANCHOR_MIN_REDUCTION_NUMERATOR
            {
                sample = survivors;
                selected.push((*anchor, posting));
                if sample.is_empty() {
                    break;
                }
            }
        }
        Ok(selected)
    }

    fn intersect_q3_anchor_blocks(
        &self,
        primary: Q3AnchorCandidate,
        primary_posting: VNextQ3Posting<'_>,
        extra_anchors: &[(Q3AnchorCandidate, VNextQ3Posting<'_>)],
    ) -> Result<Vec<u16>, SearchError> {
        let mut candidates = Vec::with_capacity(primary.cardinality);
        'primary: for block_id in primary_posting.iter() {
            for (anchor, posting) in extra_anchors {
                if !self.q3_anchor_can_align(block_id, primary.offset, anchor.offset, *posting)? {
                    continue 'primary;
                }
            }
            candidates.push(block_id);
        }
        Ok(candidates)
    }

    fn q3_anchor_can_align(
        &self,
        primary_block_id: u16,
        primary_offset: usize,
        secondary_offset: usize,
        secondary_posting: VNextQ3Posting<'_>,
    ) -> Result<bool, SearchError> {
        let primary_block = self.block(primary_block_id)?;
        let doc_id = primary_block.doc_id;
        let block_count = self.document_block_count(doc_id)?;
        if block_count <= 1 {
            return Ok(secondary_posting.contains(primary_block_id));
        }

        let first_block = self.first_block(doc_id)?;
        let local_block = usize::from(primary_block_id)
            .checked_sub(usize::from(first_block))
            .ok_or_else(|| SearchError::Format("vNext primary block precedes document".into()))?;
        let block_size = self.block_size() as usize;
        let block_start = local_block
            .checked_mul(block_size)
            .ok_or_else(|| SearchError::Format("vNext anchor block offset overflow".into()))?;
        let block_end_inclusive = block_start
            .checked_add(primary_block.content_len as usize)
            .and_then(|value| value.checked_sub(1))
            .ok_or_else(|| SearchError::Format("vNext anchor block range overflow".into()))?;
        let doc_len = self.normalized_content(doc_id)?.len();
        if doc_len < 3 {
            return Ok(false);
        }
        let max_q3_start = doc_len - 3;
        let delta = secondary_offset as i64 - primary_offset as i64;
        let shifted_start = block_start as i64 + delta;
        let shifted_end = block_end_inclusive as i64 + delta;
        let clipped_start = shifted_start.max(0);
        let clipped_end = shifted_end.min(max_q3_start as i64);
        if clipped_start > clipped_end {
            return Ok(false);
        }
        let clipped_start = usize::try_from(clipped_start)
            .map_err(|_| SearchError::Format("vNext aligned q3 start overflow".into()))?;
        let clipped_end = usize::try_from(clipped_end)
            .map_err(|_| SearchError::Format("vNext aligned q3 end overflow".into()))?;

        let first_local = clipped_start / block_size;
        let last_local = clipped_end / block_size;
        for local in first_local..=last_local {
            if local >= block_count as usize {
                continue;
            }
            let candidate = usize::from(first_block)
                .checked_add(local)
                .ok_or_else(|| SearchError::Format("vNext aligned block ID overflow".into()))?;
            let candidate = u16::try_from(candidate)
                .map_err(|_| SearchError::Format("vNext aligned block ID exceeds u16".into()))?;
            if secondary_posting.contains(candidate) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn verify_content_candidates<I>(
        &self,
        blocks: I,
        anchor_offset: usize,
        query: &[u8],
        matcher: Option<&ExactMatcher<'_>>,
    ) -> Result<(Vec<u16>, usize), SearchError>
    where
        I: Iterator<Item = u16>,
    {
        if self.all_docs_single_block() {
            return self.verify_single_block_candidates(blocks, query, matcher);
        }

        let mut hits = Vec::new();
        let mut hit_docs = vec![false; self.doc_count() as usize];
        let mut verified_blocks = 0usize;
        for block_id in blocks {
            let block = self.block(block_id)?;
            let doc_index = block.doc_id as usize;
            if *hit_docs
                .get(doc_index)
                .ok_or_else(|| SearchError::Format("vNext q3 posting points outside docs".into()))?
            {
                continue;
            }

            let matches = if query.len() == 3 {
                true
            } else {
                verified_blocks = verified_blocks
                    .checked_add(1)
                    .ok_or_else(|| SearchError::Format("vNext verified block overflow".into()))?;
                self.verify_query_around_block(
                    block_id,
                    anchor_offset,
                    query,
                    matcher.expect("long query matcher"),
                )?
            };
            if matches {
                hit_docs[doc_index] = true;
                hits.push(block.doc_id);
            }
        }
        Ok((hits, verified_blocks))
    }

    fn verify_single_block_candidates<I>(
        &self,
        blocks: I,
        query: &[u8],
        matcher: Option<&ExactMatcher<'_>>,
    ) -> Result<(Vec<u16>, usize), SearchError>
    where
        I: Iterator<Item = u16>,
    {
        let (table, content) = self.single_block_sections()?;
        let mut hits = Vec::new();
        let mut verified_blocks = 0usize;
        for block_id in blocks {
            let index = usize::from(block_id);
            let off = index
                .checked_mul(12)
                .ok_or_else(|| SearchError::Format("vNext block table offset overflow".into()))?;
            let row = table
                .get(off..off + 12)
                .ok_or_else(|| SearchError::Format("vNext q3 posting block out of range".into()))?;
            let doc_id = u16::from_le_bytes([row[0], row[1]]);
            if doc_id != block_id {
                return Err(SearchError::Format(
                    "vNext single-block segment block/document mismatch".into(),
                ));
            }
            if query.len() == 3 {
                hits.push(doc_id);
                continue;
            }
            let content_offset = u32::from_le_bytes([row[4], row[5], row[6], row[7]]) as usize;
            let content_len = u32::from_le_bytes([row[8], row[9], row[10], row[11]]) as usize;
            let end = content_offset
                .checked_add(content_len)
                .ok_or_else(|| SearchError::Format("vNext block range overflow".into()))?;
            let haystack = content
                .get(content_offset..end)
                .ok_or_else(|| SearchError::Format("vNext block content out of bounds".into()))?;
            verified_blocks = verified_blocks
                .checked_add(1)
                .ok_or_else(|| SearchError::Format("vNext verified block overflow".into()))?;
            if matcher.expect("long query matcher").contains(haystack) {
                hits.push(doc_id);
            }
        }
        Ok((hits, verified_blocks))
    }

    fn verify_single_block_candidates_parallel(
        &self,
        blocks: &[u16],
        matcher: &ExactMatcher<'_>,
    ) -> Result<(Vec<u16>, usize), SearchError> {
        let workers = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(HIGH_HIT_MAX_WORKERS)
            .min(blocks.len());
        if workers <= 1 {
            return self.verify_single_block_candidates(
                blocks.iter().copied(),
                matcher.needle(),
                Some(matcher),
            );
        }
        let chunk_size = blocks.len().div_ceil(workers);
        let results = std::thread::scope(|scope| {
            let handles = blocks
                .chunks(chunk_size)
                .map(|chunk| {
                    scope.spawn(move || {
                        self.verify_single_block_candidates(
                            chunk.iter().copied(),
                            matcher.needle(),
                            Some(matcher),
                        )
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|handle| {
                    handle.join().map_err(|_| {
                        SearchError::Format("vNext high-hit verify worker panicked".into())
                    })?
                })
                .collect::<Result<Vec<_>, SearchError>>()
        })?;
        let mut hits = Vec::new();
        let mut verified = 0usize;
        for (mut local_hits, local_verified) in results {
            hits.append(&mut local_hits);
            verified = verified
                .checked_add(local_verified)
                .ok_or_else(|| SearchError::Format("vNext verified block overflow".into()))?;
        }
        Ok((hits, verified))
    }

    /// Dense-query fast path for one-block-per-document segments.
    ///
    /// Instead of decoding a q3 posting and then re-entering the verifier once per candidate, walk
    /// the normalized content blob monotonically. `find_from` may observe a byte pattern that
    /// straddles two adjacent documents because the blob has no delimiter; those candidates are
    /// rejected by the block boundary check. After the first accepted hit in a document we jump to
    /// the next document, which is especially effective for 90%+ true-hit queries.
    /// Visit exact dense-query hits without first materializing a local-document vector.
    ///
    /// This is used by the multi-generation high-hit path so exact blob verification, newest-wins
    /// filtering, and generation bitmap population can happen in one pass. The callback is invoked
    /// at most once per document and only after the match is proven to stay within that document.
    /// Range form of the dense single-block scanner.
    ///
    /// `start_block..end_block` is document-aligned because one-block-per-document is required.
    /// Generation-level scheduling can therefore split one large immutable segment across the
    /// existing bounded query pool without changing exact-match or document-boundary semantics.
    pub(crate) fn scan_content_blob_single_block_range_visit<F>(
        &self,
        folded_query: &[u8],
        start_block: usize,
        end_block: usize,
        limit: Option<usize>,
        mut on_hit: F,
    ) -> Result<Option<usize>, SearchError>
    where
        F: FnMut(u16),
    {
        if !self.all_docs_single_block() || folded_query.len() <= 3 {
            return Ok(None);
        }
        let blocks = self.block_count() as usize;
        if start_block > end_block || end_block > blocks {
            return Err(SearchError::InvalidArgument(
                "vNext dense scan block range is out of bounds".into(),
            ));
        }
        if start_block == end_block || limit == Some(0) {
            return Ok(Some(0));
        }
        let (table, content) = self.single_block_sections()?;
        let matcher = ExactMatcher::new(folded_query);
        let needle_len = folded_query.len();
        let bounds = |index: usize| -> Result<(usize, usize, u16), SearchError> {
            let off = index
                .checked_mul(12)
                .ok_or_else(|| SearchError::Format("vNext block table offset overflow".into()))?;
            let row = table
                .get(off..off + 12)
                .ok_or_else(|| SearchError::Format("vNext block table row out of bounds".into()))?;
            let doc_id = u16::from_le_bytes([row[0], row[1]]);
            let start = u32::from_le_bytes([row[4], row[5], row[6], row[7]]) as usize;
            let len = u32::from_le_bytes([row[8], row[9], row[10], row[11]]) as usize;
            let end = start
                .checked_add(len)
                .ok_or_else(|| SearchError::Format("vNext block range overflow".into()))?;
            if end > content.len() || usize::from(doc_id) != index {
                return Err(SearchError::Format(
                    "vNext single-block table/content mismatch".into(),
                ));
            }
            Ok((start, end, doc_id))
        };
        let (range_start, _, _) = bounds(start_block)?;
        let (_, range_end, _) = bounds(end_block - 1)?;

        let profile_enabled = query_profile_enabled();
        let profile_started = profile_enabled.then(Instant::now);
        let mut find_from_calls = 0usize;
        let mut candidate_positions = 0usize;
        let mut hit_count = 0usize;
        let mut block_index = start_block;
        let mut cursor = range_start;
        while block_index < end_block {
            let (mut block_start, mut block_end, mut doc_id) = bounds(block_index)?;
            cursor = cursor.max(block_start);
            let (position, examined) = if profile_enabled {
                find_from_calls += 1;
                matcher.find_from_profiled(content, cursor)
            } else {
                (matcher.find_from(content, cursor), 0)
            };
            candidate_positions = candidate_positions.saturating_add(examined);
            let Some(position) = position else {
                break;
            };
            if position >= range_end {
                break;
            }

            while position >= block_end {
                block_index += 1;
                if block_index >= end_block {
                    if profile_enabled {
                        eprintln!(
                            "VNEXT_QUERY_PHASE kind=dense_blob query_len={} start_block={} end_block={} hits={} find_from_calls={} candidate_positions={} total_ms={:.6}",
                            folded_query.len(),
                            start_block,
                            end_block,
                            hit_count,
                            find_from_calls,
                            candidate_positions,
                            elapsed_ns(profile_started) as f64 / 1_000_000.0,
                        );
                    }
                    return Ok(Some(hit_count));
                }
                (block_start, block_end, doc_id) = bounds(block_index)?;
            }
            if position < block_start {
                return Err(SearchError::Format(
                    "vNext dense scan moved before current document".into(),
                ));
            }

            if position
                .checked_add(needle_len)
                .is_some_and(|end| end <= block_end)
            {
                on_hit(doc_id);
                hit_count = hit_count.saturating_add(1);
                if limit.is_some_and(|limit| hit_count >= limit) {
                    break;
                }
                block_index += 1;
                cursor = if block_index < end_block {
                    bounds(block_index)?.0
                } else {
                    range_end
                };
            } else {
                // The candidate crossed a document boundary. Resume one byte later so a valid
                // match beginning near the end of this document is not skipped.
                cursor = position.saturating_add(1);
            }
        }
        if profile_enabled {
            eprintln!(
                "VNEXT_QUERY_PHASE kind=dense_blob query_len={} start_block={} end_block={} hits={} find_from_calls={} candidate_positions={} total_ms={:.6}",
                folded_query.len(),
                start_block,
                end_block,
                hit_count,
                find_from_calls,
                candidate_positions,
                elapsed_ns(profile_started) as f64 / 1_000_000.0,
            );
        }
        Ok(Some(hit_count))
    }

    pub(crate) fn content_contains_prepared(
        &self,
        doc_id: u16,
        matcher: &ExactMatcher<'_>,
    ) -> Result<bool, SearchError> {
        if matcher.needle().is_empty() {
            return Ok(false);
        }
        if self.all_docs_single_block() {
            let (table, content) = self.single_block_sections()?;
            let index = usize::from(doc_id);
            if index >= self.doc_count() as usize {
                return Ok(false);
            }
            let off = index
                .checked_mul(12)
                .ok_or_else(|| SearchError::Format("vNext block table offset overflow".into()))?;
            let row = table
                .get(off..off + 12)
                .ok_or_else(|| SearchError::Format("vNext block table row out of bounds".into()))?;
            let row_doc = u16::from_le_bytes([row[0], row[1]]);
            if row_doc != doc_id {
                return Err(SearchError::Format(
                    "vNext single-block segment block/document mismatch".into(),
                ));
            }
            let start = u32::from_le_bytes([row[4], row[5], row[6], row[7]]) as usize;
            let len = u32::from_le_bytes([row[8], row[9], row[10], row[11]]) as usize;
            let end = start
                .checked_add(len)
                .ok_or_else(|| SearchError::Format("vNext block range overflow".into()))?;
            let haystack = content
                .get(start..end)
                .ok_or_else(|| SearchError::Format("vNext block content out of bounds".into()))?;
            return Ok(matcher.contains(haystack));
        }
        Ok(matcher.contains(self.normalized_content(doc_id)?))
    }

    pub(crate) fn path_contains_prepared(
        &self,
        doc_id: u16,
        folded_query: &[u8],
        matcher: &ExactMatcher<'_>,
    ) -> Result<bool, SearchError> {
        if folded_query.is_empty() {
            return Ok(false);
        }
        let path = self.display_path(doc_id)?.as_bytes();
        if folded_query.len() > path.len() {
            return Ok(false);
        }
        if matcher.contains(path) {
            return Ok(true);
        }
        if !path.iter().any(u8::is_ascii_uppercase) {
            return Ok(false);
        }
        Ok(contains_folded_ascii(path, folded_query))
    }

    /// Filename/path search matching Perf12's `normalized_name = fold_ascii(display_path)`.
    ///
    /// All distinct q3 grams are scored through the per-reader cardinality cache built at mmap
    /// open. Only the rarest posting is decoded from the persistent q3 sections before exact
    /// verification, avoiding repeated shard-bitmap/rank walks for long selective paths.
    pub fn search_path(&self, query: impl AsRef<[u8]>) -> Result<Vec<u16>, SearchError> {
        let query = fold_ascii(query.as_ref());
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let profile_enabled = query_profile_enabled();
        let total_started = profile_enabled.then(Instant::now);
        if query.len() <= 2 {
            return Ok(self.path_short_posting(&query)?.iter().collect());
        }

        let anchor_started = profile_enabled.then(Instant::now);
        let mut anchors = Vec::<([u8; 3], usize)>::with_capacity(query.len().saturating_sub(2));
        for gram in query.windows(3) {
            let gram = [gram[0], gram[1], gram[2]];
            if anchors.iter().any(|(seen, _)| *seen == gram) {
                continue;
            }
            let cardinality = self.path_q3_cardinality(gram)?;
            if cardinality == 0 {
                return Ok(Vec::new());
            }
            anchors.push((gram, cardinality));
        }
        anchors.sort_by_key(|(_, cardinality)| *cardinality);
        let anchor_ns = elapsed_ns(anchor_started);
        let (primary_gram, _) = *anchors
            .first()
            .ok_or_else(|| SearchError::Format("vNext path q3 planner failed".into()))?;
        let primary = self.path_q3_posting(primary_gram)?;
        if query.len() == 3 {
            return Ok(primary.iter().collect());
        }
        let extra = anchors
            .iter()
            .skip(1)
            .take(2)
            .map(|(gram, _)| self.path_q3_posting(*gram))
            .collect::<Result<Vec<_>, _>>()?;

        let matcher = ExactMatcher::new(&query);
        let intersection_started = profile_enabled.then(Instant::now);
        let candidates = if extra.is_empty() {
            primary.iter().collect::<Vec<_>>()
        } else {
            intersect_direct_q3_postings(primary, &extra)
        };
        let intersection_ns = elapsed_ns(intersection_started);
        let candidate_docs = candidates.len();
        let verify_started = profile_enabled.then(Instant::now);
        let mut hits = Vec::new();
        for doc_id in candidates {
            let path = self.display_path(doc_id)?.as_bytes();
            if matcher.contains(path)
                || (path.iter().any(u8::is_ascii_uppercase) && contains_folded_ascii(path, &query))
            {
                hits.push(doc_id);
            }
        }
        let verify_ns = elapsed_ns(verify_started);
        if profile_enabled {
            let mut raw = usize::from(primary.encoding() == VNextQ3PostingEncoding::RawU16);
            let mut dense = usize::from(primary.encoding() == VNextQ3PostingEncoding::DenseBitmap);
            let mut singleton =
                usize::from(primary.encoding() == VNextQ3PostingEncoding::Singleton);
            for posting in &extra {
                raw += usize::from(posting.encoding() == VNextQ3PostingEncoding::RawU16);
                dense += usize::from(posting.encoding() == VNextQ3PostingEncoding::DenseBitmap);
                singleton += usize::from(posting.encoding() == VNextQ3PostingEncoding::Singleton);
            }
            eprintln!(
                "VNEXT_QUERY_PHASE kind=path query_len={} primary_blocks={} selected_anchors={} candidate_docs={} hits={} raw_postings={} dense_postings={} singleton_postings={} anchor_ms={:.6} intersection_ms={:.6} verify_ms={:.6} total_ms={:.6}",
                query.len(),
                primary.len(),
                1 + extra.len(),
                candidate_docs,
                hits.len(),
                raw,
                dense,
                singleton,
                anchor_ns as f64 / 1_000_000.0,
                intersection_ns as f64 / 1_000_000.0,
                verify_ns as f64 / 1_000_000.0,
                elapsed_ns(total_started) as f64 / 1_000_000.0,
            );
        }
        Ok(hits)
    }

    /// Alias kept for direct A/B with Perf12 `search_name` terminology.
    pub fn search_name(&self, query: impl AsRef<[u8]>) -> Result<Vec<u16>, SearchError> {
        self.search_path(query)
    }

    fn verify_query_around_block(
        &self,
        block_id: u16,
        anchor_offset: usize,
        query: &[u8],
        matcher: &ExactMatcher<'_>,
    ) -> Result<bool, SearchError> {
        let block = self.block(block_id)?;
        if self.document_block_count(block.doc_id)? == 1 {
            return Ok(matcher.contains(self.block_content_from_meta(block)?));
        }
        let doc = self.normalized_content(block.doc_id)?;
        if doc.len() < query.len() {
            return Ok(false);
        }
        let first_block_id = self.first_block(block.doc_id)?;
        let first_block = self.block(first_block_id)?;
        let doc_start = first_block.content_offset as usize;
        let block_start = (block.content_offset as usize)
            .checked_sub(doc_start)
            .ok_or_else(|| SearchError::Format("vNext block precedes its document".into()))?;
        let block_end = block_start
            .checked_add(block.content_len as usize)
            .ok_or_else(|| SearchError::Format("vNext block verify range overflow".into()))?;

        let verify_start = block_start.saturating_sub(anchor_offset);
        let right_extension = query
            .len()
            .checked_sub(anchor_offset)
            .and_then(|value| value.checked_sub(1))
            .ok_or_else(|| SearchError::Format("vNext anchor offset exceeds query".into()))?;
        let verify_end = block_end.saturating_add(right_extension).min(doc.len());
        let window = doc
            .get(verify_start..verify_end)
            .ok_or_else(|| SearchError::Format("vNext verify window out of bounds".into()))?;
        Ok(matcher.contains(window))
    }
}

fn posting_encoding_counts(
    primary: VNextQ3Posting<'_>,
    extra: &[(Q3AnchorCandidate, VNextQ3Posting<'_>)],
) -> (usize, usize, usize) {
    let mut raw = 0usize;
    let mut dense = 0usize;
    let mut singleton = 0usize;
    for posting in std::iter::once(primary).chain(extra.iter().map(|(_, posting)| *posting)) {
        match posting.encoding() {
            VNextQ3PostingEncoding::RawU16 => raw += 1,
            VNextQ3PostingEncoding::DenseBitmap => dense += 1,
            VNextQ3PostingEncoding::Singleton => singleton += 1,
            VNextQ3PostingEncoding::Empty => {}
        }
    }
    (raw, dense, singleton)
}

fn sample_posting_evenly(posting: VNextQ3Posting<'_>, max_samples: usize) -> Vec<u16> {
    let sample_count = posting.len().min(max_samples);
    if sample_count == 0 {
        return Vec::new();
    }
    if sample_count == 1 {
        return posting.iter().take(1).collect();
    }
    if sample_count == posting.len() {
        return posting.iter().collect();
    }

    let last_ordinal = posting.len() - 1;
    let last_sample = sample_count - 1;
    let mut samples = Vec::with_capacity(sample_count);
    let mut sample_index = 0usize;
    let mut target = 0usize;
    for (ordinal, block_id) in posting.iter().enumerate() {
        if ordinal == target {
            samples.push(block_id);
            sample_index += 1;
            if sample_index == sample_count {
                break;
            }
            target = sample_index * last_ordinal / last_sample;
        }
    }
    samples
}

#[inline]
fn contains_folded_ascii(haystack: &[u8], folded_needle: &[u8]) -> bool {
    if folded_needle.is_empty() {
        return true;
    }
    if folded_needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(folded_needle.len()).any(|window| {
        window
            .iter()
            .zip(folded_needle)
            .all(|(left, right)| left.to_ascii_lowercase() == *right)
    })
}
