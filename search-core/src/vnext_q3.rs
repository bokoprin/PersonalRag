use crate::format::SearchError;
use crate::vnext_segment::VNextDocumentInput;
use std::thread;
use std::time::Instant;

pub(crate) const SHARD_COUNT: usize = 256;
pub(crate) const SHARD_DIR_ENTRY_SIZE: usize = 16;
pub(crate) const SHARD_DIR_SIZE: usize = SHARD_COUNT * SHARD_DIR_ENTRY_SIZE;
const PRESENCE_BITMAP_SIZE: usize = 65_536 / 8;
const RANK_BUCKET_KEYS: usize = 256;
const RANK_ENTRY_COUNT: usize = 257;
const RANK_BYTES: usize = RANK_ENTRY_COUNT * 4;
const POSTING_META_SIZE: usize = 16;
const POSTING_ENCODING_SINGLETON: u8 = 1;
const POSTING_ENCODING_RAW_U16: u8 = 2;
const POSTING_ENCODING_DENSE_BITMAP: u8 = 3;

// Exact owner-local hashing is profitable only for very repetitive content. Sampling every
// block polluted the direct hot path, so the production decision is made once per segment:
// inspect at most 16 evenly spaced documents, measure one representative block from each,
// and enable local dedup only when >= 65% of sampled q3 occurrences are duplicates.
// Ordinary code/text therefore keeps the original emitter; strongly repetitive/generated
// text avoids sending most duplicate occurrences into the global radix.
const LOCAL_Q3_DEDUP_SAMPLE_DOCS: usize = 16;
const LOCAL_Q3_DEDUP_THRESHOLD_PPM: u64 = 650_000;
const LOCAL_Q3_DEDUP_MIN_SAMPLE_OCCURRENCES: u64 = 1_024;
const LOCAL_Q3_DEDUP_MAX_TABLE_ENTRIES: usize = 1 << 18;
// Keep tiny owner blocks on the proven pre-periodic path: their q3 stream is too short to amortize
// suffix probing or generation-stamp indirection.
const LOCAL_Q3_PERIODIC_MIN_OCCURRENCES: usize = 768;
const LOCAL_Q3_GENERATION_MIN_TABLE_ENTRIES: usize = 4_096;
// Internal q3 parallelism is bounded by the segment-level CPU budget. Avoid spawning workers
// for tiny jobs: thread setup and per-worker radix scratch can cost more than the saved CPU time.
const Q3_EMIT_MIN_OCCURRENCES_PER_WORKER: u64 = 750_000;
const Q3_SHARD_MIN_OCCURRENCES_PER_WORKER: u64 = 500_000;
const Q3_RADIX8_MAX_VALUES: usize = 32_768;

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct VNextQ3BuildProfile {
    pub emit_ms: f64,
    pub radix_prepare_ms: f64,
    pub radix_count_ms: f64,
    pub radix_prefix_ms: f64,
    pub radix_scatter_ms: f64,
    pub dedup_ms: f64,
    pub encode_ms: f64,
    /// CPU time spent deriving exact q2 postings from deduplicated q3 pairs.
    pub q2_projection_ms: f64,
    /// Exact (q2 key, owner) pairs emitted by the build-only q3 projection.
    pub q2_projection_pairs: u64,
    /// All logical q3 starts before optional owner-local dedup.
    pub occurrences: u64,
    /// Packed occurrences actually handed to the global shard radix.
    pub radix_occurrences: u64,
    pub local_dedup_saved: u64,
    pub local_dedup_blocks: u64,
    /// Owner blocks that actually initialized/touched the exact LocalQ3Set.
    pub local_hash_blocks: u64,
    /// q3 occurrences skipped after an exact periodic suffix proof inside an owner block.
    pub periodic_skip_occurrences: u64,
    /// Owner blocks where the exact periodic suffix fast path skipped the remaining q3 starts.
    pub periodic_skip_blocks: u64,
    pub direct_blocks: u64,
    pub local_sample_occurrences: u64,
    pub local_sample_duplicates: u64,
    pub unique_pairs: u64,
    /// Number of workers actually used while emitting content q3 occurrences.
    pub emit_workers: u16,
    /// Number of workers actually used for shard radix/dedup/encoding.
    pub shard_workers: u16,
    /// Wall time of the complete shard radix/dedup/encode phase. The detailed radix/dedup/
    /// encode counters above are cumulative worker CPU time when shard_workers > 1.
    pub shard_wall_ms: f64,
}

#[derive(Debug)]
pub(crate) struct VNextQ3Data {
    pub shard_dir: Vec<u8>,
    pub dictionary: Vec<u8>,
    pub postings: Vec<u8>,
    pub key_count: u32,
    pub posting_ids: u64,
    pub active_shards: u16,
    pub singleton_keys: u32,
    pub raw_u16_keys: u32,
    pub dense_bitmap_keys: u32,
    /// Build-only q2 fast-path metadata. For each content owner block, this stores the local
    /// byte offset where an exact q3 periodic-suffix proof established that all remaining q2
    /// starts repeat earlier starts. u16::MAX means no proof. This is never serialized.
    pub periodic_q2_skip_from: Vec<u16>,
    /// Build-only exact q2 postings derived from deduplicated q3 pairs. Packed as
    /// `(q2_key << 16) | owner`, globally sorted and unique. Never serialized.
    pub projected_q2: Option<Vec<Vec<u32>>>,
    /// Build-only owner-major exact q2 pairs collected while q3 emission is already visiting
    /// owner-local unique q3s. Used by the fused q1/q2 path when periodic coverage is high.
    pub emitted_q2_pairs: Option<Vec<u32>>,
    /// Build-only q1 owner lists collected alongside emitted_q2_pairs. Lists are key-indexed and
    /// owners are strictly ascending.
    pub emitted_q1_lists: Option<Vec<Vec<u16>>>,
    pub build_profile: VNextQ3BuildProfile,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VNextQ3PostingEncoding {
    Empty,
    Singleton,
    RawU16,
    DenseBitmap,
}

#[derive(Clone, Copy, Debug)]
pub struct VNextQ3Posting<'a> {
    encoding: VNextQ3PostingEncoding,
    bytes: &'a [u8],
    singleton: u16,
    cardinality: usize,
    block_count: usize,
}

impl<'a> VNextQ3Posting<'a> {
    pub(crate) const fn empty(bytes: &'a [u8]) -> Self {
        Self {
            encoding: VNextQ3PostingEncoding::Empty,
            bytes,
            singleton: 0,
            cardinality: 0,
            block_count: 0,
        }
    }

    pub(crate) const fn singleton(bytes: &'a [u8], block_id: u16) -> Self {
        Self {
            encoding: VNextQ3PostingEncoding::Singleton,
            bytes,
            singleton: block_id,
            cardinality: 1,
            block_count: usize::MAX,
        }
    }

    pub(crate) const fn raw_u16(bytes: &'a [u8], cardinality: usize) -> Self {
        Self {
            encoding: VNextQ3PostingEncoding::RawU16,
            bytes,
            singleton: 0,
            cardinality,
            block_count: usize::MAX,
        }
    }

    pub(crate) const fn dense_bitmap(
        bytes: &'a [u8],
        cardinality: usize,
        block_count: usize,
    ) -> Self {
        Self {
            encoding: VNextQ3PostingEncoding::DenseBitmap,
            bytes,
            singleton: 0,
            cardinality,
            block_count,
        }
    }

    #[must_use]
    pub const fn encoding(&self) -> VNextQ3PostingEncoding {
        self.encoding
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.cardinality
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.cardinality == 0
    }

    #[must_use]
    pub fn get(&self, index: usize) -> Option<u16> {
        if index >= self.cardinality {
            return None;
        }
        match self.encoding {
            VNextQ3PostingEncoding::Empty => None,
            VNextQ3PostingEncoding::Singleton => Some(self.singleton),
            VNextQ3PostingEncoding::RawU16 => {
                let off = index.checked_mul(2)?;
                let pair = self.bytes.get(off..off + 2)?;
                Some(u16::from_le_bytes([pair[0], pair[1]]))
            }
            VNextQ3PostingEncoding::DenseBitmap => self.iter().nth(index),
        }
    }

    #[must_use]
    pub const fn encoded_bytes(&self) -> usize {
        self.bytes.len()
    }

    #[must_use]
    pub(crate) const fn raw_u16_bytes(&self) -> Option<&'a [u8]> {
        match self.encoding {
            VNextQ3PostingEncoding::RawU16 => Some(self.bytes),
            _ => None,
        }
    }

    #[must_use]
    pub(crate) const fn dense_bitmap_bytes(&self) -> Option<&'a [u8]> {
        match self.encoding {
            VNextQ3PostingEncoding::DenseBitmap => Some(self.bytes),
            _ => None,
        }
    }

    #[must_use]
    pub(crate) const fn singleton_value(&self) -> Option<u16> {
        match self.encoding {
            VNextQ3PostingEncoding::Singleton => Some(self.singleton),
            _ => None,
        }
    }

    #[must_use]
    pub(crate) const fn block_count(&self) -> usize {
        self.block_count
    }

    #[must_use]
    pub fn contains(&self, block_id: u16) -> bool {
        match self.encoding {
            VNextQ3PostingEncoding::Empty => false,
            VNextQ3PostingEncoding::Singleton => self.singleton == block_id,
            VNextQ3PostingEncoding::RawU16 => {
                let target = block_id;
                let mut low = 0usize;
                let mut high = self.cardinality;
                while low < high {
                    let mid = low + (high - low) / 2;
                    let off = mid * 2;
                    let value = u16::from_le_bytes([self.bytes[off], self.bytes[off + 1]]);
                    match value.cmp(&target) {
                        std::cmp::Ordering::Less => low = mid + 1,
                        std::cmp::Ordering::Greater => high = mid,
                        std::cmp::Ordering::Equal => return true,
                    }
                }
                false
            }
            VNextQ3PostingEncoding::DenseBitmap => {
                let index = block_id as usize;
                index < self.block_count && self.bytes[index / 8] & (1u8 << (index % 8)) != 0
            }
        }
    }

    #[must_use]
    pub fn iter(&self) -> VNextQ3PostingIter<'a> {
        VNextQ3PostingIter {
            posting: *self,
            cursor: 0,
            remaining: self.cardinality,
        }
    }
}

pub struct VNextQ3PostingIter<'a> {
    posting: VNextQ3Posting<'a>,
    cursor: usize,
    remaining: usize,
}

impl Iterator for VNextQ3PostingIter<'_> {
    type Item = u16;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let value = match self.posting.encoding {
            VNextQ3PostingEncoding::Empty => return None,
            VNextQ3PostingEncoding::Singleton => {
                self.cursor = 1;
                self.posting.singleton
            }
            VNextQ3PostingEncoding::RawU16 => {
                let off = self.cursor.checked_mul(2)?;
                let pair = self.posting.bytes.get(off..off + 2)?;
                self.cursor += 1;
                u16::from_le_bytes([pair[0], pair[1]])
            }
            VNextQ3PostingEncoding::DenseBitmap => loop {
                if self.cursor >= self.posting.block_count {
                    return None;
                }
                let block_id = self.cursor;
                self.cursor += 1;
                if self.posting.bytes[block_id / 8] & (1u8 << (block_id % 8)) != 0 {
                    break u16::try_from(block_id).ok()?;
                }
            },
        };
        self.remaining -= 1;
        Some(value)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

impl ExactSizeIterator for VNextQ3PostingIter<'_> {}

#[derive(Default)]
struct LocalQ3Set {
    // Stored value is the full 24-bit q3 key. A parallel generation stamp marks slots belonging
    // to the current owner block, so reset is normally just an epoch increment instead of a full
    // hash-table memset. u16 is sufficient because a segment has at most u16::MAX owner blocks;
    // the rare wrap path clears stamps once and restarts at generation 1.
    slots: Vec<u32>,
    generations: Vec<u16>,
    generation: u16,
    use_generation: bool,
}

impl LocalQ3Set {
    fn mask_for(expected: usize) -> Option<usize> {
        let table_len = expected.checked_mul(2)?.checked_next_power_of_two()?.max(8);
        (table_len <= LOCAL_Q3_DEDUP_MAX_TABLE_ENTRIES).then_some(table_len - 1)
    }

    fn reset_for(&mut self, expected: usize) -> Option<usize> {
        let mask = Self::mask_for(expected)?;
        let table_len = mask + 1;
        if self.slots.len() < table_len {
            self.slots.resize(table_len, 0);
        }
        self.use_generation = table_len >= LOCAL_Q3_GENERATION_MIN_TABLE_ENTRIES;
        if self.use_generation {
            if self.generations.len() < table_len {
                self.generations.resize(table_len, 0);
            }
            self.generation = self.generation.wrapping_add(1);
            if self.generation == 0 {
                self.generations.fill(0);
                self.generation = 1;
            }
        } else {
            self.slots[..table_len].fill(0);
        }
        Some(table_len - 1)
    }

    fn insert(&mut self, key: u32, mask: usize) -> bool {
        debug_assert!(key <= 0x00ff_ffff);
        let mut slot_index = q3_hash(key) as usize & mask;
        if self.use_generation {
            loop {
                if self.generations[slot_index] != self.generation {
                    self.generations[slot_index] = self.generation;
                    self.slots[slot_index] = key;
                    return true;
                }
                if self.slots[slot_index] == key {
                    return false;
                }
                slot_index = (slot_index + 1) & mask;
            }
        }
        let stored = key + 1;
        loop {
            let slot = &mut self.slots[slot_index];
            if *slot == 0 {
                *slot = stored;
                return true;
            }
            if *slot == stored {
                return false;
            }
            slot_index = (slot_index + 1) & mask;
        }
    }
}

#[inline]
fn q3_hash(key: u32) -> u32 {
    let mut hash = key.wrapping_mul(0x85eb_ca6b);
    hash ^= hash >> 16;
    hash
}

#[inline]
fn full_q3_key(bytes: &[u8], start: usize) -> u32 {
    (u32::from(bytes[start]) << 16)
        | (u32::from(bytes[start + 1]) << 8)
        | u32::from(bytes[start + 2])
}

fn emit_packed_q3(shards: &mut [Vec<u32>], key: u32, owner: u16) {
    let shard = (key >> 16) as usize;
    let suffix = key as u16;
    shards[shard].push((u32::from(suffix) << 16) | u32::from(owner));
}

#[derive(Clone, Copy, Debug, Default)]
struct LocalQ3Decision {
    enabled: bool,
    sample_occurrences: u64,
    sample_duplicates: u64,
}

fn decide_owner_local_dedup(
    docs: &[VNextDocumentInput],
    block_size: usize,
    local_set: &mut LocalQ3Set,
) -> LocalQ3Decision {
    if docs.is_empty() {
        return LocalQ3Decision::default();
    }
    let sample_count = LOCAL_Q3_DEDUP_SAMPLE_DOCS.min(docs.len());
    let mut occurrences = 0u64;
    let mut duplicates = 0u64;

    for sample in 0..sample_count {
        let doc_index = if sample_count <= 1 {
            0
        } else {
            sample * (docs.len() - 1) / (sample_count - 1)
        };
        let bytes = &docs[doc_index].normalized_content;
        if bytes.len() < 3 {
            continue;
        }
        let last_start_exclusive = bytes.len() - 2;
        let block_count = last_start_exclusive.div_ceil(block_size);
        let local_block = block_count / 2;
        let block_start = local_block * block_size;
        let block_end = block_start
            .saturating_add(block_size)
            .min(last_start_exclusive);
        let block_occurrences = block_end - block_start;
        let Some(mask) = local_set.reset_for(block_occurrences) else {
            continue;
        };
        let mut unique = 0usize;
        for start in block_start..block_end {
            if local_set.insert(full_q3_key(bytes, start), mask) {
                unique += 1;
            }
        }
        occurrences = occurrences.saturating_add(block_occurrences as u64);
        duplicates = duplicates.saturating_add((block_occurrences - unique) as u64);
    }

    let enabled = occurrences >= LOCAL_Q3_DEDUP_MIN_SAMPLE_OCCURRENCES
        && duplicates.saturating_mul(1_000_000)
            >= occurrences.saturating_mul(LOCAL_Q3_DEDUP_THRESHOLD_PPM);
    LocalQ3Decision {
        enabled,
        sample_occurrences: occurrences,
        sample_duplicates: duplicates,
    }
}

#[derive(Debug, Default)]
struct ContentQ3EmitStats {
    local_saved: u64,
    local_blocks: u64,
    local_hash_blocks: u64,
    periodic_skip_occurrences: u64,
    periodic_skip_blocks: u64,
    periodic_q2_skips: Vec<(u16, u16)>,
    projected_q2_pairs: Vec<u32>,
    projected_q1_lists: Option<Vec<Vec<u16>>>,
    direct_blocks: u64,
}

#[inline]
fn record_emitted_q2(
    stats: &mut ContentQ3EmitStats,
    q2_last_owner: &mut [u16],
    q1_last_owner: &mut [u16; 256],
    q2_key: u16,
    owner: u16,
) {
    let q2_index = usize::from(q2_key);
    if q2_last_owner[q2_index] == owner {
        return;
    }
    q2_last_owner[q2_index] = owner;
    stats
        .projected_q2_pairs
        .push((u32::from(q2_key) << 16) | u32::from(owner));
    let q1_key = usize::from((q2_key >> 8) as u8);
    if q1_last_owner[q1_key] != owner {
        q1_last_owner[q1_key] = owner;
        if let Some(lists) = stats.projected_q1_lists.as_mut() {
            lists[q1_key].push(owner);
        }
    }
}

#[inline]
fn record_emitted_q1_final(
    stats: &mut ContentQ3EmitStats,
    q1_last_owner: &mut [u16; 256],
    byte: u8,
    owner: u16,
) {
    let key = usize::from(byte);
    if q1_last_owner[key] != owner {
        q1_last_owner[key] = owner;
        if let Some(lists) = stats.projected_q1_lists.as_mut() {
            lists[key].push(owner);
        }
    }
}

fn emit_content_q3_direct(
    docs: &[VNextDocumentInput],
    first_blocks: &[u16],
    block_size: usize,
    shards: &mut [Vec<u32>],
) -> Result<ContentQ3EmitStats, SearchError> {
    let mut direct_blocks = 0u64;
    for (doc_index, doc) in docs.iter().enumerate() {
        if doc.normalized_content.len() < 3 {
            continue;
        }
        let first_block = usize::from(first_blocks[doc_index]);
        let last_start_exclusive = doc.normalized_content.len() - 2;
        let mut block_start = 0usize;
        let mut local_block = 0usize;
        while block_start < last_start_exclusive {
            let block_end = block_start
                .saturating_add(block_size)
                .min(last_start_exclusive);
            let owner = first_block
                .checked_add(local_block)
                .ok_or_else(|| SearchError::Format("vNext q3 block ID overflow".into()))?;
            let owner = u16::try_from(owner)
                .map_err(|_| SearchError::Format("vNext q3 block ID exceeds u16".into()))?;
            for start in block_start..block_end {
                let a = doc.normalized_content[start] as usize;
                let suffix = (u16::from(doc.normalized_content[start + 1]) << 8)
                    | u16::from(doc.normalized_content[start + 2]);
                shards[a].push((u32::from(suffix) << 16) | u32::from(owner));
            }
            direct_blocks = direct_blocks.saturating_add(1);
            block_start = block_end;
            local_block = local_block
                .checked_add(1)
                .ok_or_else(|| SearchError::Format("vNext q3 local block overflow".into()))?;
        }
    }
    Ok(ContentQ3EmitStats {
        direct_blocks,
        ..ContentQ3EmitStats::default()
    })
}

fn emit_content_q3_local_dedup(
    docs: &[VNextDocumentInput],
    first_blocks: &[u16],
    block_size: usize,
    shards: &mut [Vec<u32>],
    local_set: &mut LocalQ3Set,
    collect_q12_projection: bool,
) -> Result<ContentQ3EmitStats, SearchError> {
    let mut stats = ContentQ3EmitStats::default();
    if collect_q12_projection {
        stats.projected_q1_lists = Some((0..256).map(|_| Vec::<u16>::new()).collect::<Vec<_>>());
    }
    let mut q2_last_owner = collect_q12_projection.then(|| vec![u16::MAX; 65_536]);
    let mut q1_last_owner = [u16::MAX; 256];
    // High-repetition blocks often revisit the same q3s with a short period. The tiny direct-
    // mapped cache is an exact-positive filter: collisions can only become misses. For long
    // blocks, defer LocalQ3Set entirely while probing the first 256 starts for an exact periodic
    // suffix. If the proof succeeds, global radix/dedup remains the exact oracle for the small
    // speculative prefix and the large owner-local hash table is never touched. If it fails, seed
    // LocalQ3Set from that prefix and continue the original exact path.
    let mut recent_q3 = [0u32; 256];
    let mut recent_pos = [0usize; 256];
    for (doc_index, doc) in docs.iter().enumerate() {
        let first_block = usize::from(first_blocks[doc_index]);
        if doc.normalized_content.len() < 3 {
            if collect_q12_projection {
                if doc.normalized_content.len() == 2 {
                    let key = (u16::from(doc.normalized_content[0]) << 8)
                        | u16::from(doc.normalized_content[1]);
                    record_emitted_q2(
                        &mut stats,
                        q2_last_owner.as_mut().expect("q2 projection allocated"),
                        &mut q1_last_owner,
                        key,
                        first_blocks[doc_index],
                    );
                }
                if let Some(&last) = doc.normalized_content.last() {
                    record_emitted_q1_final(
                        &mut stats,
                        &mut q1_last_owner,
                        last,
                        first_blocks[doc_index],
                    );
                }
            }
            continue;
        }
        let last_start_exclusive = doc.normalized_content.len() - 2;
        let mut block_start = 0usize;
        let mut local_block = 0usize;
        while block_start < last_start_exclusive {
            let block_end = block_start
                .saturating_add(block_size)
                .min(last_start_exclusive);
            let owner = first_block
                .checked_add(local_block)
                .ok_or_else(|| SearchError::Format("vNext q3 block ID overflow".into()))?;
            let owner = u16::try_from(owner)
                .map_err(|_| SearchError::Format("vNext q3 block ID exceeds u16".into()))?;
            let occurrences = block_end - block_start;
            let mut projection_scan_end = block_end;
            if LocalQ3Set::mask_for(occurrences).is_some() {
                recent_q3.fill(0);
                let mut emitted = 0usize;
                let bytes = &doc.normalized_content;
                if occurrences < LOCAL_Q3_PERIODIC_MIN_OCCURRENCES {
                    let mask = local_set.reset_for(occurrences).ok_or_else(|| {
                        SearchError::Format(
                            "vNext q3 local hash sizing changed unexpectedly".into(),
                        )
                    })?;
                    stats.local_hash_blocks = stats.local_hash_blocks.saturating_add(1);
                    for start in block_start..block_end {
                        let key = full_q3_key(bytes, start);
                        let stored = key + 1;
                        let recent_slot = &mut recent_q3[key as usize & 0xff];
                        if *recent_slot == stored {
                            continue;
                        }
                        *recent_slot = stored;
                        if local_set.insert(key, mask) {
                            emit_packed_q3(shards, key, owner);
                            emitted += 1;
                        }
                    }
                } else {
                    let mut repeat_probes = 0u8;
                    let mut start = block_start;
                    let speculative_end = block_start.saturating_add(256).min(block_end);
                    let mut periodic_proven = false;

                    while start < speculative_end {
                        let key = full_q3_key(bytes, start);
                        let stored = key + 1;
                        let slot_index = key as usize & 0xff;
                        if recent_q3[slot_index] == stored {
                            let previous = recent_pos[slot_index];
                            let lag = start - previous;
                            let remaining = block_end - start;
                            if repeat_probes < 8 && lag <= 256 && remaining >= 256 {
                                repeat_probes += 1;
                                let byte_end = block_end + 2;
                                let previous_start = start - lag;
                                if bytes[start..byte_end] == bytes[previous_start..byte_end - lag] {
                                    stats.periodic_skip_occurrences = stats
                                        .periodic_skip_occurrences
                                        .saturating_add(remaining as u64);
                                    stats.periodic_skip_blocks =
                                        stats.periodic_skip_blocks.saturating_add(1);
                                    stats.periodic_q2_skips.push((
                                        owner,
                                        u16::try_from(start - block_start).map_err(|_| {
                                            SearchError::Format(
                                                "vNext q3 periodic skip offset exceeds u16".into(),
                                            )
                                        })?,
                                    ));
                                    projection_scan_end = start;
                                    periodic_proven = true;
                                    break;
                                }
                            }
                            recent_pos[slot_index] = start;
                            start += 1;
                            continue;
                        }
                        recent_q3[slot_index] = stored;
                        recent_pos[slot_index] = start;
                        // A speculative miss is emitted directly. A cache collision can therefore
                        // emit an owner-local duplicate, but the existing global radix dedup removes
                        // it exactly and serialized bytes remain unchanged.
                        emit_packed_q3(shards, key, owner);
                        emitted += 1;
                        start += 1;
                    }

                    if !periodic_proven {
                        let mask = local_set.reset_for(occurrences).ok_or_else(|| {
                            SearchError::Format(
                                "vNext q3 local hash sizing changed unexpectedly".into(),
                            )
                        })?;
                        stats.local_hash_blocks = stats.local_hash_blocks.saturating_add(1);
                        // Seed the exact fallback from the speculative prefix. Re-reading at most
                        // 256 q3s is cheaper than hashing every periodic-success block, and keeps
                        // later local dedup exact across the speculative/fallback boundary.
                        for seed_start in block_start..start {
                            local_set.insert(full_q3_key(bytes, seed_start), mask);
                        }

                        while start < block_end {
                            let key = full_q3_key(bytes, start);
                            let stored = key + 1;
                            let slot_index = key as usize & 0xff;
                            if recent_q3[slot_index] == stored {
                                let previous = recent_pos[slot_index];
                                let lag = start - previous;
                                let remaining = block_end - start;
                                if repeat_probes < 8 && lag <= 256 && remaining >= 256 {
                                    repeat_probes += 1;
                                    let byte_end = block_end + 2;
                                    let previous_start = start - lag;
                                    if bytes[start..byte_end]
                                        == bytes[previous_start..byte_end - lag]
                                    {
                                        stats.periodic_skip_occurrences = stats
                                            .periodic_skip_occurrences
                                            .saturating_add(remaining as u64);
                                        stats.periodic_skip_blocks =
                                            stats.periodic_skip_blocks.saturating_add(1);
                                        stats.periodic_q2_skips.push((
                                            owner,
                                            u16::try_from(start - block_start).map_err(|_| {
                                                SearchError::Format(
                                                    "vNext q3 periodic skip offset exceeds u16"
                                                        .into(),
                                                )
                                            })?,
                                        ));
                                        projection_scan_end = start;
                                        break;
                                    }
                                }
                                recent_pos[slot_index] = start;
                                start += 1;
                                continue;
                            }
                            recent_q3[slot_index] = stored;
                            recent_pos[slot_index] = start;
                            if local_set.insert(key, mask) {
                                emit_packed_q3(shards, key, owner);
                                emitted += 1;
                            }
                            start += 1;
                        }
                    }
                }
                stats.local_saved = stats
                    .local_saved
                    .saturating_add((occurrences - emitted) as u64);
                stats.local_blocks = stats.local_blocks.saturating_add(1);
            } else {
                for start in block_start..block_end {
                    let key = full_q3_key(&doc.normalized_content, start);
                    emit_packed_q3(shards, key, owner);
                }
                stats.direct_blocks = stats.direct_blocks.saturating_add(1);
            }
            if let Some(last_owner) = q2_last_owner.as_mut() {
                for q2_start in block_start..projection_scan_end {
                    let q2_key = (u16::from(doc.normalized_content[q2_start]) << 8)
                        | u16::from(doc.normalized_content[q2_start + 1]);
                    record_emitted_q2(&mut stats, last_owner, &mut q1_last_owner, q2_key, owner);
                }
            }
            block_start = block_end;
            local_block = local_block
                .checked_add(1)
                .ok_or_else(|| SearchError::Format("vNext q3 local block overflow".into()))?;
        }
        if collect_q12_projection {
            let bytes = &doc.normalized_content;
            let q2_start = bytes.len() - 2;
            let tail_owner = first_block
                .checked_add(q2_start / block_size)
                .ok_or_else(|| {
                    SearchError::Format("vNext projected q2 tail owner overflow".into())
                })?;
            let tail_owner = u16::try_from(tail_owner).map_err(|_| {
                SearchError::Format("vNext projected q2 tail owner exceeds u16".into())
            })?;
            let q2_key = (u16::from(bytes[q2_start]) << 8) | u16::from(bytes[q2_start + 1]);
            record_emitted_q2(
                &mut stats,
                q2_last_owner.as_mut().expect("q2 projection allocated"),
                &mut q1_last_owner,
                q2_key,
                tail_owner,
            );
            let final_start = bytes.len() - 1;
            let final_owner = first_block
                .checked_add(final_start / block_size)
                .ok_or_else(|| {
                    SearchError::Format("vNext projected q1 tail owner overflow".into())
                })?;
            let final_owner = u16::try_from(final_owner).map_err(|_| {
                SearchError::Format("vNext projected q1 tail owner exceeds u16".into())
            })?;
            record_emitted_q1_final(
                &mut stats,
                &mut q1_last_owner,
                bytes[final_start],
                final_owner,
            );
        }
    }
    Ok(stats)
}

fn empty_q3_shards() -> Vec<Vec<u32>> {
    (0..SHARD_COUNT).map(|_| Vec::<u32>::new()).collect()
}

fn q3_effective_worker_count(
    worker_limit: usize,
    item_count: usize,
    work_items: u64,
    min_work_per_worker: u64,
) -> usize {
    let limit = worker_limit.max(1).min(item_count.max(1));
    if limit <= 1 || work_items < min_work_per_worker.saturating_mul(2) {
        return 1;
    }
    let by_work = work_items.div_ceil(min_work_per_worker).max(1) as usize;
    limit.min(by_work.max(1))
}

fn q3_doc_ranges(doc_count: usize, workers: usize) -> Vec<std::ops::Range<usize>> {
    let workers = workers.max(1).min(doc_count.max(1));
    let mut ranges = Vec::with_capacity(workers);
    for worker in 0..workers {
        let begin = worker * doc_count / workers;
        let end = (worker + 1) * doc_count / workers;
        if begin < end {
            ranges.push(begin..end);
        }
    }
    if ranges.is_empty() {
        ranges.push(0..0);
    }
    ranges
}

fn emit_content_q3_chunk(
    docs: &[VNextDocumentInput],
    first_blocks: &[u16],
    block_size: usize,
    local_dedup: bool,
    collect_q12_projection: bool,
) -> Result<(Vec<Vec<u32>>, ContentQ3EmitStats), SearchError> {
    let mut shards = empty_q3_shards();
    let stats = if local_dedup {
        let mut local_set = LocalQ3Set::default();
        emit_content_q3_local_dedup(
            docs,
            first_blocks,
            block_size,
            &mut shards,
            &mut local_set,
            collect_q12_projection,
        )?
    } else {
        emit_content_q3_direct(docs, first_blocks, block_size, &mut shards)?
    };
    Ok((shards, stats))
}

fn merge_content_q3_partials(
    mut partials: Vec<(Vec<Vec<u32>>, ContentQ3EmitStats)>,
) -> (Vec<Vec<u32>>, ContentQ3EmitStats) {
    let mut merged = empty_q3_shards();
    let mut stats = ContentQ3EmitStats::default();
    for (_, part_stats) in &mut partials {
        stats.local_saved = stats.local_saved.saturating_add(part_stats.local_saved);
        stats.local_blocks = stats.local_blocks.saturating_add(part_stats.local_blocks);
        stats.local_hash_blocks = stats
            .local_hash_blocks
            .saturating_add(part_stats.local_hash_blocks);
        stats.periodic_skip_occurrences = stats
            .periodic_skip_occurrences
            .saturating_add(part_stats.periodic_skip_occurrences);
        stats.periodic_skip_blocks = stats
            .periodic_skip_blocks
            .saturating_add(part_stats.periodic_skip_blocks);
        stats
            .periodic_q2_skips
            .append(&mut part_stats.periodic_q2_skips);
        stats
            .projected_q2_pairs
            .append(&mut part_stats.projected_q2_pairs);
        if let Some(part_lists) = part_stats.projected_q1_lists.take() {
            let target_lists = stats
                .projected_q1_lists
                .get_or_insert_with(|| (0..256).map(|_| Vec::<u16>::new()).collect::<Vec<_>>());
            for (target, mut source) in target_lists.iter_mut().zip(part_lists) {
                target.append(&mut source);
            }
        }
        stats.direct_blocks = stats.direct_blocks.saturating_add(part_stats.direct_blocks);
    }
    if partials.is_empty() {
        return (merged, stats);
    }
    for shard_id in 0..SHARD_COUNT {
        let total = partials
            .iter()
            .map(|(shards, _)| shards[shard_id].len())
            .sum::<usize>();
        let mut shard = std::mem::take(&mut partials[0].0[shard_id]);
        shard.reserve(total.saturating_sub(shard.len()));
        for (shards, _) in partials.iter_mut().skip(1) {
            shard.append(&mut shards[shard_id]);
        }
        merged[shard_id] = shard;
    }
    (merged, stats)
}

fn emit_content_q3_bounded(
    docs: &[VNextDocumentInput],
    first_blocks: &[u16],
    block_size: usize,
    local_dedup: bool,
    collect_q12_projection: bool,
    workers: usize,
) -> Result<(Vec<Vec<u32>>, ContentQ3EmitStats), SearchError> {
    if workers <= 1 || docs.len() <= 1 {
        return emit_content_q3_chunk(
            docs,
            first_blocks,
            block_size,
            local_dedup,
            collect_q12_projection,
        );
    }
    let ranges = q3_doc_ranges(docs.len(), workers);
    if ranges.len() <= 1 {
        return emit_content_q3_chunk(
            docs,
            first_blocks,
            block_size,
            local_dedup,
            collect_q12_projection,
        );
    }

    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(ranges.len().saturating_sub(1));
        for range in ranges.iter().skip(1).cloned() {
            handles.push(scope.spawn(move || {
                emit_content_q3_chunk(
                    &docs[range.clone()],
                    &first_blocks[range],
                    block_size,
                    local_dedup,
                    collect_q12_projection,
                )
            }));
        }
        let first = &ranges[0];
        let mut partials = Vec::with_capacity(ranges.len());
        partials.push(emit_content_q3_chunk(
            &docs[first.clone()],
            &first_blocks[first.clone()],
            block_size,
            local_dedup,
            collect_q12_projection,
        )?);
        for handle in handles {
            partials.push(
                handle
                    .join()
                    .map_err(|_| SearchError::Format("vNext q3 emit worker panicked".into()))??,
            );
        }
        Ok(merge_content_q3_partials(partials))
    })
}

#[derive(Clone, Copy)]
struct Q3BuildOptions {
    local_dedup: bool,
    q2_projection: bool,
}

pub(crate) fn build_q3_index_with_workers(
    docs: &[VNextDocumentInput],
    first_blocks: &[u16],
    block_size: u32,
    block_count: usize,
    collect_profile: bool,
    worker_limit: usize,
) -> Result<VNextQ3Data, SearchError> {
    build_q3_index_impl(
        docs,
        first_blocks,
        block_size,
        block_count,
        collect_profile,
        Q3BuildOptions {
            local_dedup: true,
            q2_projection: false,
        },
        worker_limit.max(1),
    )
}

pub(crate) fn build_q3_index_with_q2_projection(
    docs: &[VNextDocumentInput],
    first_blocks: &[u16],
    block_size: u32,
    block_count: usize,
    collect_profile: bool,
) -> Result<VNextQ3Data, SearchError> {
    build_q3_index_impl(
        docs,
        first_blocks,
        block_size,
        block_count,
        collect_profile,
        Q3BuildOptions {
            local_dedup: true,
            q2_projection: true,
        },
        1,
    )
}

fn build_q3_index_impl(
    docs: &[VNextDocumentInput],
    first_blocks: &[u16],
    block_size: u32,
    block_count: usize,
    collect_profile: bool,
    options: Q3BuildOptions,
    worker_limit: usize,
) -> Result<VNextQ3Data, SearchError> {
    if docs.len() != first_blocks.len() {
        return Err(SearchError::Format(
            "vNext q3 document/block shape mismatch".into(),
        ));
    }
    if block_count > u16::MAX as usize {
        return Err(SearchError::InvalidArgument(
            "vNext q3 block count exceeds u16".into(),
        ));
    }
    let block_size = usize::try_from(block_size)
        .map_err(|_| SearchError::InvalidArgument("vNext q3 block size too large".into()))?;
    if block_size == 0 {
        return Err(SearchError::InvalidArgument(
            "vNext q3 block size must be non-zero".into(),
        ));
    }

    // Packed occurrence: upper16 = q3 suffix BC, lower16 = owner block ID.
    // First byte A chooses the shard. The start position owns the q3 even when
    // the following one/two bytes are in the next block of the same document.
    // Traversal is in logical document/block order, so each shard receives owners
    // in monotonically non-decreasing order. encode_q3_shards relies on this to
    // stable-sort only the suffix half while preserving owner order within a key.
    //
    // Repeated q3s inside one owner can be removed before this global radix without
    // changing serialized bytes: the old global shard.dedup() removed exactly the
    // same (suffix, owner) duplicates after sorting. We only pay the local hash cost
    // for blocks whose small sample predicts enough repetition to win.
    let emit_started = collect_profile.then(Instant::now);
    let mut profile = VNextQ3BuildProfile::default();
    let mut local_set = LocalQ3Set::default();
    let local_decision = if options.local_dedup {
        decide_owner_local_dedup(docs, block_size, &mut local_set)
    } else {
        LocalQ3Decision::default()
    };
    if collect_profile {
        profile.local_sample_occurrences = local_decision.sample_occurrences;
        profile.local_sample_duplicates = local_decision.sample_duplicates;
    }
    let logical_occurrences = docs
        .iter()
        .map(|doc| doc.normalized_content.len().saturating_sub(2) as u64)
        .sum::<u64>();
    let emit_workers = q3_effective_worker_count(
        worker_limit,
        docs.len(),
        logical_occurrences,
        Q3_EMIT_MIN_OCCURRENCES_PER_WORKER,
    );
    // Fastest validated variant: keep q1/q2 projection out of the q3 emit hot loop.
    // Build q2 from the already radix-sorted/deduplicated q3 shards instead.
    let collect_q12_during_emit = false;
    let (shards, mut emit_stats) = emit_content_q3_bounded(
        docs,
        first_blocks,
        block_size,
        local_decision.enabled,
        collect_q12_during_emit,
        emit_workers,
    )?;
    if collect_profile {
        profile.local_dedup_saved = emit_stats.local_saved;
        profile.local_dedup_blocks = emit_stats.local_blocks;
        profile.local_hash_blocks = emit_stats.local_hash_blocks;
        profile.periodic_skip_occurrences = emit_stats.periodic_skip_occurrences;
        profile.periodic_skip_blocks = emit_stats.periodic_skip_blocks;
        profile.direct_blocks = emit_stats.direct_blocks;
        profile.emit_workers = u16::try_from(emit_workers).unwrap_or(u16::MAX);
        let radix_occurrences = shards.iter().map(|shard| shard.len() as u64).sum::<u64>();
        profile.occurrences = radix_occurrences
            .checked_add(profile.local_dedup_saved)
            .ok_or_else(|| SearchError::Format("vNext q3 occurrence count overflow".into()))?;
    }

    profile.emit_ms = emit_started
        .map(|started| started.elapsed().as_secs_f64() * 1000.0)
        .unwrap_or(0.0);
    let mut periodic_q2_skip_from = if emit_stats.periodic_q2_skips.is_empty() {
        Vec::new()
    } else {
        vec![u16::MAX; block_count]
    };
    for (owner, offset) in std::mem::take(&mut emit_stats.periodic_q2_skips) {
        let slot = periodic_q2_skip_from
            .get_mut(usize::from(owner))
            .ok_or_else(|| SearchError::Format("vNext q3 periodic owner out of range".into()))?;
        *slot = (*slot).min(offset);
    }
    let proven_periodic_blocks = periodic_q2_skip_from
        .iter()
        .filter(|&&offset| offset != u16::MAX)
        .count();
    let use_emitted_q12 = collect_q12_during_emit
        && block_count != 0
        && proven_periodic_blocks.saturating_mul(4) >= block_count.saturating_mul(3);
    let emitted_q2_pairs =
        use_emitted_q12.then(|| std::mem::take(&mut emit_stats.projected_q2_pairs));
    let emitted_q1_lists = if use_emitted_q12 {
        emit_stats.projected_q1_lists.take()
    } else {
        None
    };
    let mut data = encode_q3_shards(
        shards,
        block_count,
        collect_profile,
        profile,
        worker_limit.max(1),
        periodic_q2_skip_from,
        options.q2_projection
            && local_decision.enabled
            && std::env::var_os("PR_DISABLE_Q3_Q2_PROJECTION").is_none(),
    )?;
    data.emitted_q2_pairs = emitted_q2_pairs;
    data.emitted_q1_lists = emitted_q1_lists;
    Ok(data)
}

pub(crate) fn build_path_q3_index(
    docs: &[VNextDocumentInput],
    collect_profile: bool,
) -> Result<VNextQ3Data, SearchError> {
    if docs.len() > u16::MAX as usize {
        return Err(SearchError::InvalidArgument(
            "vNext path q3 document count exceeds u16".into(),
        ));
    }
    let emit_started = collect_profile.then(Instant::now);
    let mut shards = (0..SHARD_COUNT)
        .map(|_| Vec::<u32>::new())
        .collect::<Vec<_>>();
    for (doc_index, doc) in docs.iter().enumerate() {
        let bytes = doc.display_path.as_bytes();
        if bytes.len() < 3 {
            continue;
        }
        let owner = u16::try_from(doc_index)
            .map_err(|_| SearchError::Format("vNext path doc ID exceeds u16".into()))?;
        for start in 0..bytes.len() - 2 {
            let a = bytes[start].to_ascii_lowercase() as usize;
            let suffix = (u16::from(bytes[start + 1].to_ascii_lowercase()) << 8)
                | u16::from(bytes[start + 2].to_ascii_lowercase());
            shards[a].push((u32::from(suffix) << 16) | u32::from(owner));
        }
    }
    let emit_ms = emit_started
        .map(|started| started.elapsed().as_secs_f64() * 1000.0)
        .unwrap_or(0.0);
    let occurrences = if collect_profile {
        shards.iter().map(|shard| shard.len() as u64).sum()
    } else {
        0
    };
    encode_q3_shards(
        shards,
        docs.len(),
        collect_profile,
        VNextQ3BuildProfile {
            emit_ms,
            occurrences,
            emit_workers: 1,
            ..VNextQ3BuildProfile::default()
        },
        1,
        Vec::new(),
        false,
    )
}

// Precondition: within every shard, lower16 owner IDs are monotonically non-decreasing.
// Both content and path emitters satisfy this by traversing documents/blocks in owner order.
// The stable upper16 radix therefore produces the same (suffix, owner) order as a full
// two-pass packed-u32 radix while avoiding the lower16 pass.
#[derive(Debug)]
struct EncodedQ3Shard {
    shard_id: usize,
    dictionary: Vec<u8>,
    postings: Vec<u8>,
    posting_offset_fields: Vec<usize>,
    key_count: u32,
    posting_ids: u64,
    singleton_keys: u32,
    raw_u16_keys: u32,
    dense_bitmap_keys: u32,
    projected_q2: Vec<u32>,
    profile: VNextQ3BuildProfile,
}

fn accumulate_q3_worker_profile(target: &mut VNextQ3BuildProfile, source: &VNextQ3BuildProfile) {
    target.radix_prepare_ms += source.radix_prepare_ms;
    target.radix_count_ms += source.radix_count_ms;
    target.radix_prefix_ms += source.radix_prefix_ms;
    target.radix_scatter_ms += source.radix_scatter_ms;
    target.dedup_ms += source.dedup_ms;
    target.encode_ms += source.encode_ms;
    target.q2_projection_ms += source.q2_projection_ms;
    target.q2_projection_pairs = target
        .q2_projection_pairs
        .saturating_add(source.q2_projection_pairs);
    target.unique_pairs = target.unique_pairs.saturating_add(source.unique_pairs);
}

#[derive(Default)]
struct Q3EncodeScratch {
    radix: Vec<u32>,
    counts: Vec<usize>,
    q2_owner_bits: Vec<u64>,
}

fn encode_one_q3_shard(
    shard_id: usize,
    mut shard: Vec<u32>,
    block_count: usize,
    collect_profile: bool,
    collect_q2_projection: bool,
    scratch: &mut Q3EncodeScratch,
) -> Result<Option<EncodedQ3Shard>, SearchError> {
    if shard.is_empty() {
        return Ok(None);
    }
    let bitmap_bytes = block_count.div_ceil(8);
    let mut profile = VNextQ3BuildProfile::default();
    radix_sort_u32_upper16_stable(
        &mut shard,
        &mut scratch.radix,
        &mut scratch.counts,
        collect_profile.then_some(&mut profile),
    );
    let dedup_started = collect_profile.then(Instant::now);
    shard.dedup();
    if let Some(started) = dedup_started {
        profile.dedup_ms += started.elapsed().as_secs_f64() * 1000.0;
    }
    profile.unique_pairs = shard.len() as u64;

    let projection_started = (collect_profile && collect_q2_projection).then(Instant::now);
    let mut projected_q2 = Vec::new();
    if collect_q2_projection {
        let owner_words = block_count.div_ceil(64);
        if scratch.q2_owner_bits.len() < owner_words {
            scratch.q2_owner_bits.resize(owner_words, 0);
        } else {
            scratch.q2_owner_bits[..owner_words].fill(0);
        }
        // The deduplicated q3 shard is ordered by (B, C, owner). q2 only needs (A, B,
        // owner), so union owner postings across all C values using one reusable bitmap and emit
        // the result directly as a globally sortable packed q2 stream. No per-key Vec allocation
        // or owner sort is required.
        projected_q2.reserve(shard.len());
        let mut group_start = 0usize;
        while group_start < shard.len() {
            let b = (shard[group_start] >> 24) as u8;
            let mut group_end = group_start + 1;
            while group_end < shard.len() && (shard[group_end] >> 24) as u8 == b {
                group_end += 1;
            }
            let q2_key = ((shard_id as u16) << 8) | u16::from(b);
            let first_suffix = (shard[group_start] >> 16) as u16;
            let last_suffix = (shard[group_end - 1] >> 16) as u16;
            if first_suffix == last_suffix {
                // With only one C value, q3 dedup already produced the exact ascending owner
                // list for q2(A,B), so avoid the owner bitmap entirely.
                for &packed in &shard[group_start..group_end] {
                    projected_q2.push((u32::from(q2_key) << 16) | u32::from(packed as u16));
                }
            } else {
                for &packed in &shard[group_start..group_end] {
                    let owner = packed as u16 as usize;
                    scratch.q2_owner_bits[owner / 64] |= 1u64 << (owner % 64);
                }
                for (word_index, word) in
                    scratch.q2_owner_bits[..owner_words].iter_mut().enumerate()
                {
                    let mut value = *word;
                    while value != 0 {
                        let bit = value.trailing_zeros() as usize;
                        let owner = word_index * 64 + bit;
                        projected_q2.push((u32::from(q2_key) << 16) | owner as u32);
                        value &= value - 1;
                    }
                    *word = 0;
                }
            }
            group_start = group_end;
        }
        profile.q2_projection_pairs = projected_q2.len() as u64;
    }
    if let Some(started) = projection_started {
        profile.q2_projection_ms += started.elapsed().as_secs_f64() * 1000.0;
    }

    let encode_started = collect_profile.then(Instant::now);
    let mut dictionary = vec![0u8; PRESENCE_BITMAP_SIZE + RANK_BYTES];
    let bitmap_off = 0usize;
    let rank_off = PRESENCE_BITMAP_SIZE;
    let meta_off = PRESENCE_BITMAP_SIZE + RANK_BYTES;
    let mut postings = Vec::new();
    let mut posting_offset_fields = Vec::new();
    let mut position = 0usize;
    let mut key_count = 0u32;
    let mut posting_ids = 0u64;
    let mut singleton_keys = 0u32;
    let mut raw_u16_keys = 0u32;
    let mut dense_bitmap_keys = 0u32;

    while position < shard.len() {
        let suffix = (shard[position] >> 16) as u16;
        let begin = position;
        while position < shard.len() && (shard[position] >> 16) as u16 == suffix {
            position += 1;
        }

        let suffix_index = usize::from(suffix);
        dictionary[bitmap_off + suffix_index / 8] |= 1u8 << (suffix_index % 8);
        key_count = key_count
            .checked_add(1)
            .ok_or_else(|| SearchError::Format("vNext q3 key count overflow".into()))?;

        let posting_len = position - begin;
        let raw_bytes = posting_len
            .checked_mul(2)
            .ok_or_else(|| SearchError::Format("vNext q3 raw posting size overflow".into()))?;
        let meta_start = dictionary.len();
        dictionary.resize(meta_start + POSTING_META_SIZE, 0);

        if posting_len == 1 {
            dictionary[meta_start] = POSTING_ENCODING_SINGLETON;
            let block_id = shard[begin] as u16;
            put_u32_at(&mut dictionary, meta_start + 4, u32::from(block_id))?;
            singleton_keys = singleton_keys
                .checked_add(1)
                .ok_or_else(|| SearchError::Format("vNext q3 singleton count overflow".into()))?;
        } else if bitmap_bytes > 0 && raw_bytes > bitmap_bytes {
            dictionary[meta_start] = POSTING_ENCODING_DENSE_BITMAP;
            put_u32_at(
                &mut dictionary,
                meta_start + 4,
                checked_u32(posting_len, "q3 bitmap cardinality")?,
            )?;
            let posting_off = checked_u32(postings.len(), "q3 bitmap offset")?;
            put_u32_at(&mut dictionary, meta_start + 8, posting_off)?;
            posting_offset_fields.push(meta_start + 8);
            put_u32_at(
                &mut dictionary,
                meta_start + 12,
                checked_u32(bitmap_bytes, "q3 bitmap bytes")?,
            )?;
            let bitmap_start = postings.len();
            postings.resize(bitmap_start + bitmap_bytes, 0);
            for packed in &shard[begin..position] {
                let block_id = *packed as u16 as usize;
                postings[bitmap_start + block_id / 8] |= 1u8 << (block_id % 8);
            }
            dense_bitmap_keys = dense_bitmap_keys
                .checked_add(1)
                .ok_or_else(|| SearchError::Format("vNext q3 bitmap count overflow".into()))?;
        } else {
            dictionary[meta_start] = POSTING_ENCODING_RAW_U16;
            put_u32_at(
                &mut dictionary,
                meta_start + 4,
                checked_u32(posting_len, "q3 raw cardinality")?,
            )?;
            let posting_off = checked_u32(postings.len(), "q3 raw posting offset")?;
            put_u32_at(&mut dictionary, meta_start + 8, posting_off)?;
            posting_offset_fields.push(meta_start + 8);
            put_u32_at(
                &mut dictionary,
                meta_start + 12,
                checked_u32(raw_bytes, "q3 raw posting bytes")?,
            )?;
            for packed in &shard[begin..position] {
                postings.extend_from_slice(&(*packed as u16).to_le_bytes());
            }
            raw_u16_keys = raw_u16_keys
                .checked_add(1)
                .ok_or_else(|| SearchError::Format("vNext q3 raw count overflow".into()))?;
        }
        posting_ids = posting_ids
            .checked_add(posting_len as u64)
            .ok_or_else(|| SearchError::Format("vNext q3 posting count overflow".into()))?;
    }

    let bitmap = dictionary[..PRESENCE_BITMAP_SIZE].to_vec();
    let mut rank = 0u32;
    for bucket in 0..RANK_ENTRY_COUNT {
        put_u32_at(&mut dictionary, rank_off + bucket * 4, rank)?;
        if bucket < 256 {
            let begin = bucket * (RANK_BUCKET_KEYS / 8);
            let end = begin + (RANK_BUCKET_KEYS / 8);
            rank = rank
                .checked_add(
                    bitmap[begin..end]
                        .iter()
                        .map(|byte| byte.count_ones())
                        .sum::<u32>(),
                )
                .ok_or_else(|| SearchError::Format("vNext q3 rank overflow".into()))?;
        }
    }
    if rank != key_count {
        return Err(SearchError::Format(
            "vNext q3 rank/key count mismatch while building".into(),
        ));
    }
    let expected_len = PRESENCE_BITMAP_SIZE
        .checked_add(RANK_BYTES)
        .and_then(|base| base.checked_add(key_count as usize * POSTING_META_SIZE))
        .ok_or_else(|| SearchError::Format("vNext q3 dictionary size overflow".into()))?;
    if dictionary.len() != expected_len || meta_off != rank_off + RANK_BYTES {
        return Err(SearchError::Format(
            "vNext q3 dictionary layout mismatch while building".into(),
        ));
    }
    if let Some(started) = encode_started {
        profile.encode_ms += started.elapsed().as_secs_f64() * 1000.0;
    }

    Ok(Some(EncodedQ3Shard {
        shard_id,
        dictionary,
        postings,
        posting_offset_fields,
        key_count,
        posting_ids,
        singleton_keys,
        raw_u16_keys,
        dense_bitmap_keys,
        projected_q2,
        profile,
    }))
}

fn partition_q3_shard_jobs(shards: Vec<Vec<u32>>, workers: usize) -> Vec<Vec<(usize, Vec<u32>)>> {
    let workers = workers.max(1);
    let mut jobs = shards
        .into_iter()
        .enumerate()
        .filter(|(_, shard)| !shard.is_empty())
        .collect::<Vec<_>>();
    // Longest-processing-time assignment gives a stable, cheap approximation of a balanced
    // bounded pool while allowing every worker to keep private radix scratch/count storage.
    jobs.sort_by(|(left_id, left), (right_id, right)| {
        right
            .len()
            .cmp(&left.len())
            .then_with(|| left_id.cmp(right_id))
    });
    let mut buckets = (0..workers)
        .map(|_| Vec::<(usize, Vec<u32>)>::new())
        .collect::<Vec<_>>();
    let mut loads = vec![0usize; workers];
    for (shard_id, shard) in jobs {
        let worker = loads
            .iter()
            .enumerate()
            .min_by_key(|(worker, load)| (**load, *worker))
            .map(|(worker, _)| worker)
            .unwrap_or(0);
        loads[worker] = loads[worker].saturating_add(shard.len());
        buckets[worker].push((shard_id, shard));
    }
    buckets
}

fn encode_q3_job_bucket(
    jobs: Vec<(usize, Vec<u32>)>,
    block_count: usize,
    collect_profile: bool,
    collect_q2_projection: bool,
) -> Result<Vec<EncodedQ3Shard>, SearchError> {
    let mut scratch = Q3EncodeScratch::default();
    let mut out = Vec::with_capacity(jobs.len());
    for (shard_id, shard) in jobs {
        if let Some(encoded) = encode_one_q3_shard(
            shard_id,
            shard,
            block_count,
            collect_profile,
            collect_q2_projection,
            &mut scratch,
        )? {
            out.push(encoded);
        }
    }
    Ok(out)
}

// Precondition: within every shard, lower16 owner IDs are monotonically non-decreasing.
// Both content and path emitters satisfy this by traversing documents/blocks in owner order.
// The stable upper16 radix therefore produces the same (suffix, owner) order as a full
// two-pass packed-u32 radix while avoiding the lower16 pass.
fn encode_q3_shards(
    shards: Vec<Vec<u32>>,
    block_count: usize,
    collect_profile: bool,
    mut profile: VNextQ3BuildProfile,
    worker_limit: usize,
    periodic_q2_skip_from: Vec<u16>,
    collect_q2_projection: bool,
) -> Result<VNextQ3Data, SearchError> {
    let radix_occurrences = shards.iter().map(|shard| shard.len() as u64).sum::<u64>();
    let active_shard_count = shards.iter().filter(|shard| !shard.is_empty()).count();
    if collect_profile {
        profile.radix_occurrences = radix_occurrences;
        debug_assert_eq!(
            profile
                .occurrences
                .saturating_sub(profile.local_dedup_saved),
            profile.radix_occurrences
        );
    }
    let shard_workers = q3_effective_worker_count(
        worker_limit,
        active_shard_count,
        radix_occurrences,
        Q3_SHARD_MIN_OCCURRENCES_PER_WORKER,
    );
    if collect_profile {
        profile.shard_workers = u16::try_from(shard_workers).unwrap_or(u16::MAX);
    }
    let shard_started = collect_profile.then(Instant::now);
    let mut buckets = partition_q3_shard_jobs(shards, shard_workers);
    let mut encoded = if shard_workers <= 1 {
        encode_q3_job_bucket(
            buckets.pop().unwrap_or_default(),
            block_count,
            collect_profile,
            collect_q2_projection,
        )?
    } else {
        thread::scope(|scope| {
            let mut handles = Vec::with_capacity(shard_workers.saturating_sub(1));
            for jobs in buckets.drain(1..) {
                handles.push(scope.spawn(move || {
                    encode_q3_job_bucket(jobs, block_count, collect_profile, collect_q2_projection)
                }));
            }
            let mut out = encode_q3_job_bucket(
                buckets.pop().unwrap_or_default(),
                block_count,
                collect_profile,
                collect_q2_projection,
            )?;
            for handle in handles {
                out.extend(
                    handle.join().map_err(|_| {
                        SearchError::Format("vNext q3 shard worker panicked".into())
                    })??,
                );
            }
            Ok::<_, SearchError>(out)
        })?
    };
    if let Some(started) = shard_started {
        profile.shard_wall_ms = started.elapsed().as_secs_f64() * 1000.0;
    }
    encoded.sort_by_key(|shard| shard.shard_id);

    let mut shard_dir = vec![0u8; SHARD_DIR_SIZE];
    let mut dictionary = Vec::new();
    let mut postings = Vec::new();
    let mut total_keys = 0u32;
    let mut total_posting_ids = 0u64;
    let mut active_shards = 0u16;
    let mut singleton_keys = 0u32;
    let mut raw_u16_keys = 0u32;
    let mut dense_bitmap_keys = 0u32;
    let projected_shards = encoded.len();
    let mut projected_q2 =
        collect_q2_projection.then(|| Vec::<Vec<u32>>::with_capacity(projected_shards));

    for mut shard in encoded {
        accumulate_q3_worker_profile(&mut profile, &shard.profile);
        if let Some(projected) = projected_q2.as_mut() {
            // Keep shard-owned q2 postings as independent sorted chunks. Moving the Vec header is
            // O(1); flattening them here copied ~1.6 MiB per 4KiB benchmark segment.
            projected.push(std::mem::take(&mut shard.projected_q2));
        }
        let dict_off = checked_u32(dictionary.len(), "q3 dictionary offset")?;
        let posting_base = checked_u32(postings.len(), "q3 posting base")?;
        for offset_field in &shard.posting_offset_fields {
            let local = rd_u32(&shard.dictionary, *offset_field)?;
            let global = posting_base
                .checked_add(local)
                .ok_or_else(|| SearchError::Format("vNext q3 posting offset overflow".into()))?;
            put_u32_at(&mut shard.dictionary, *offset_field, global)?;
        }
        let dict_len = checked_u32(shard.dictionary.len(), "q3 shard dictionary length")?;
        let dir_off = shard.shard_id * SHARD_DIR_ENTRY_SIZE;
        put_u32_at(&mut shard_dir, dir_off, dict_off)?;
        put_u32_at(&mut shard_dir, dir_off + 4, dict_len)?;
        put_u32_at(&mut shard_dir, dir_off + 8, shard.key_count)?;
        dictionary.append(&mut shard.dictionary);
        postings.append(&mut shard.postings);
        total_keys = total_keys
            .checked_add(shard.key_count)
            .ok_or_else(|| SearchError::Format("vNext q3 total key count overflow".into()))?;
        total_posting_ids = total_posting_ids
            .checked_add(shard.posting_ids)
            .ok_or_else(|| SearchError::Format("vNext q3 posting count overflow".into()))?;
        active_shards = active_shards
            .checked_add(1)
            .ok_or_else(|| SearchError::Format("vNext q3 active shard overflow".into()))?;
        singleton_keys = singleton_keys
            .checked_add(shard.singleton_keys)
            .ok_or_else(|| SearchError::Format("vNext q3 singleton count overflow".into()))?;
        raw_u16_keys = raw_u16_keys
            .checked_add(shard.raw_u16_keys)
            .ok_or_else(|| SearchError::Format("vNext q3 raw count overflow".into()))?;
        dense_bitmap_keys = dense_bitmap_keys
            .checked_add(shard.dense_bitmap_keys)
            .ok_or_else(|| SearchError::Format("vNext q3 bitmap count overflow".into()))?;
    }

    Ok(VNextQ3Data {
        shard_dir,
        dictionary,
        postings,
        key_count: total_keys,
        posting_ids: total_posting_ids,
        active_shards,
        singleton_keys,
        raw_u16_keys,
        dense_bitmap_keys,
        periodic_q2_skip_from,
        projected_q2,
        emitted_q2_pairs: None,
        emitted_q1_lists: None,
        build_profile: profile,
    })
}

pub(crate) fn lookup_q3<'a>(
    shard_dir: &[u8],
    dictionary: &[u8],
    postings: &'a [u8],
    gram: [u8; 3],
    block_count: u32,
) -> Result<VNextQ3Posting<'a>, SearchError> {
    if shard_dir.len() != SHARD_DIR_SIZE {
        return Err(SearchError::Format(
            "bad vNext q3 shard directory size".into(),
        ));
    }
    let dir_off = usize::from(gram[0]) * SHARD_DIR_ENTRY_SIZE;
    let dict_off = rd_u32(shard_dir, dir_off)? as usize;
    let dict_len = rd_u32(shard_dir, dir_off + 4)? as usize;
    let key_count = rd_u32(shard_dir, dir_off + 8)? as usize;
    if dict_len == 0 {
        if dict_off != 0 || key_count != 0 {
            return Err(SearchError::Format("bad empty vNext q3 shard entry".into()));
        }
        return Ok(VNextQ3Posting::empty(&postings[..0]));
    }
    let dict_end = dict_off
        .checked_add(dict_len)
        .ok_or_else(|| SearchError::Format("vNext q3 shard dictionary range overflow".into()))?;
    let shard = dictionary
        .get(dict_off..dict_end)
        .ok_or_else(|| SearchError::Format("vNext q3 shard dictionary out of bounds".into()))?;
    if shard.len() < PRESENCE_BITMAP_SIZE + RANK_BYTES {
        return Err(SearchError::Format(
            "vNext q3 shard dictionary is truncated".into(),
        ));
    }
    let suffix = (u16::from(gram[1]) << 8) | u16::from(gram[2]);
    let suffix_index = usize::from(suffix);
    let bitmap = &shard[..PRESENCE_BITMAP_SIZE];
    if bitmap[suffix_index / 8] & (1u8 << (suffix_index % 8)) == 0 {
        return Ok(VNextQ3Posting::empty(&postings[..0]));
    }

    let rank_off = PRESENCE_BITMAP_SIZE;
    let bucket = suffix_index / RANK_BUCKET_KEYS;
    let base_rank = rd_u32(shard, rank_off + bucket * 4)? as usize;
    let bucket_byte_start = bucket * (RANK_BUCKET_KEYS / 8);
    let suffix_byte = suffix_index / 8;
    let mut within = bitmap[bucket_byte_start..suffix_byte]
        .iter()
        .map(|byte| byte.count_ones() as usize)
        .sum::<usize>();
    let bit = suffix_index % 8;
    if bit != 0 {
        within += (bitmap[suffix_byte] & ((1u8 << bit) - 1)).count_ones() as usize;
    }
    let meta_index = base_rank
        .checked_add(within)
        .ok_or_else(|| SearchError::Format("vNext q3 meta rank overflow".into()))?;
    if meta_index >= key_count {
        return Err(SearchError::Format(
            "vNext q3 meta rank out of bounds".into(),
        ));
    }
    let meta_off = PRESENCE_BITMAP_SIZE + RANK_BYTES + meta_index * POSTING_META_SIZE;
    decode_posting(shard, meta_off, postings, block_count)
}

pub(crate) fn collect_q3_cardinalities(
    shard_dir: &[u8],
    dictionary: &[u8],
) -> Result<Vec<(u32, u32)>, SearchError> {
    if shard_dir.len() != SHARD_DIR_SIZE {
        return Err(SearchError::Format(
            "bad vNext q3 shard directory size".into(),
        ));
    }
    let mut out = Vec::new();
    for shard_id in 0..SHARD_COUNT {
        let dir_off = shard_id * SHARD_DIR_ENTRY_SIZE;
        let dict_off = rd_u32(shard_dir, dir_off)? as usize;
        let dict_len = rd_u32(shard_dir, dir_off + 4)? as usize;
        let key_count = rd_u32(shard_dir, dir_off + 8)? as usize;
        if dict_len == 0 {
            if dict_off != 0 || key_count != 0 {
                return Err(SearchError::Format("bad empty vNext q3 shard entry".into()));
            }
            continue;
        }
        let dict_end = dict_off
            .checked_add(dict_len)
            .ok_or_else(|| SearchError::Format("vNext q3 shard dictionary overflow".into()))?;
        let shard = dictionary
            .get(dict_off..dict_end)
            .ok_or_else(|| SearchError::Format("vNext q3 shard dictionary out of bounds".into()))?;
        if shard.len() < PRESENCE_BITMAP_SIZE + RANK_BYTES {
            return Err(SearchError::Format(
                "vNext q3 shard dictionary is truncated".into(),
            ));
        }
        let bitmap = &shard[..PRESENCE_BITMAP_SIZE];
        let meta_base = PRESENCE_BITMAP_SIZE + RANK_BYTES;
        let mut meta_index = 0usize;
        for (byte_index, &byte) in bitmap.iter().enumerate() {
            let mut bits = byte;
            while bits != 0 {
                let bit = bits.trailing_zeros() as usize;
                let suffix = byte_index * 8 + bit;
                let meta_off = meta_base
                    .checked_add(meta_index * POSTING_META_SIZE)
                    .ok_or_else(|| SearchError::Format("vNext q3 meta offset overflow".into()))?;
                let encoding = *shard
                    .get(meta_off)
                    .ok_or_else(|| SearchError::Format("vNext q3 meta out of bounds".into()))?;
                let cardinality = match encoding {
                    POSTING_ENCODING_SINGLETON => 1,
                    POSTING_ENCODING_RAW_U16 | POSTING_ENCODING_DENSE_BITMAP => {
                        rd_u32(shard, meta_off + 4)?
                    }
                    _ => {
                        return Err(SearchError::Format(
                            "unknown vNext q3 posting encoding".into(),
                        ));
                    }
                };
                let key = ((shard_id as u32) << 16) | suffix as u32;
                out.push((key, cardinality));
                meta_index += 1;
                bits &= bits - 1;
            }
        }
        if meta_index != key_count {
            return Err(SearchError::Format(
                "vNext q3 cardinality cache key count mismatch".into(),
            ));
        }
    }
    Ok(out)
}

pub(crate) fn validate_q3_sections(
    shard_dir: &[u8],
    dictionary: &[u8],
    postings: &[u8],
    block_count: u32,
) -> Result<(u32, u64, u16), SearchError> {
    if shard_dir.len() != SHARD_DIR_SIZE {
        return Err(SearchError::Format(
            "bad vNext q3 shard directory size".into(),
        ));
    }

    let mut expected_dict_off = 0usize;
    let mut expected_posting_off = 0usize;
    let mut total_keys = 0u32;
    let mut total_posting_ids = 0u64;
    let mut active_shards = 0u16;

    for shard_id in 0..SHARD_COUNT {
        let dir_off = shard_id * SHARD_DIR_ENTRY_SIZE;
        let dict_off = rd_u32(shard_dir, dir_off)? as usize;
        let dict_len = rd_u32(shard_dir, dir_off + 4)? as usize;
        let key_count = rd_u32(shard_dir, dir_off + 8)? as usize;
        let reserved = rd_u32(shard_dir, dir_off + 12)?;
        if reserved != 0 {
            return Err(SearchError::Format(
                "vNext q3 shard reserved field is non-zero".into(),
            ));
        }
        if dict_len == 0 {
            if dict_off != 0 || key_count != 0 {
                return Err(SearchError::Format("bad empty vNext q3 shard entry".into()));
            }
            continue;
        }
        if dict_off != expected_dict_off {
            return Err(SearchError::Format(
                "vNext q3 shard dictionaries are not contiguous".into(),
            ));
        }
        active_shards = active_shards
            .checked_add(1)
            .ok_or_else(|| SearchError::Format("vNext q3 active shard overflow".into()))?;
        let expected_len = PRESENCE_BITMAP_SIZE
            .checked_add(RANK_BYTES)
            .and_then(|base| base.checked_add(key_count * POSTING_META_SIZE))
            .ok_or_else(|| SearchError::Format("vNext q3 dictionary size overflow".into()))?;
        if dict_len != expected_len {
            return Err(SearchError::Format(
                "bad vNext q3 shard dictionary length".into(),
            ));
        }
        let dict_end = dict_off.checked_add(dict_len).ok_or_else(|| {
            SearchError::Format("vNext q3 shard dictionary range overflow".into())
        })?;
        let shard = dictionary
            .get(dict_off..dict_end)
            .ok_or_else(|| SearchError::Format("vNext q3 shard dictionary out of bounds".into()))?;
        let bitmap = &shard[..PRESENCE_BITMAP_SIZE];
        let bitmap_keys = bitmap.iter().map(|byte| byte.count_ones()).sum::<u32>() as usize;
        if bitmap_keys != key_count {
            return Err(SearchError::Format(
                "vNext q3 bitmap/key count mismatch".into(),
            ));
        }

        let rank_off = PRESENCE_BITMAP_SIZE;
        let mut running = 0u32;
        for bucket in 0..RANK_ENTRY_COUNT {
            if rd_u32(shard, rank_off + bucket * 4)? != running {
                return Err(SearchError::Format("bad vNext q3 rank directory".into()));
            }
            if bucket < 256 {
                let begin = bucket * (RANK_BUCKET_KEYS / 8);
                let end = begin + (RANK_BUCKET_KEYS / 8);
                running += bitmap[begin..end]
                    .iter()
                    .map(|byte| byte.count_ones())
                    .sum::<u32>();
            }
        }
        if running as usize != key_count {
            return Err(SearchError::Format(
                "vNext q3 rank/key count mismatch".into(),
            ));
        }

        let meta_base = PRESENCE_BITMAP_SIZE + RANK_BYTES;
        for meta_index in 0..key_count {
            let meta_off = meta_base + meta_index * POSTING_META_SIZE;
            let encoding = read_encoding(shard, meta_off)?;
            let value = rd_u32(shard, meta_off + 4)? as usize;
            let posting_off = rd_u32(shard, meta_off + 8)? as usize;
            let posting_bytes = rd_u32(shard, meta_off + 12)? as usize;
            if shard[meta_off + 1..meta_off + 4]
                .iter()
                .any(|&byte| byte != 0)
            {
                return Err(SearchError::Format(
                    "vNext q3 posting reserved metadata is non-zero".into(),
                ));
            }

            let cardinality = match encoding {
                VNextQ3PostingEncoding::Singleton => {
                    if value >= block_count as usize || posting_off != 0 || posting_bytes != 0 {
                        return Err(SearchError::Format(
                            "invalid vNext q3 singleton metadata".into(),
                        ));
                    }
                    1usize
                }
                VNextQ3PostingEncoding::RawU16 => {
                    if value < 2
                        || posting_off != expected_posting_off
                        || posting_bytes != value * 2
                    {
                        return Err(SearchError::Format(
                            "invalid vNext q3 raw posting metadata".into(),
                        ));
                    }
                    let posting_end = posting_off.checked_add(posting_bytes).ok_or_else(|| {
                        SearchError::Format("vNext q3 raw posting range overflow".into())
                    })?;
                    let data = postings.get(posting_off..posting_end).ok_or_else(|| {
                        SearchError::Format("vNext q3 raw posting out of bounds".into())
                    })?;
                    let mut previous = None;
                    for pair in data.chunks_exact(2) {
                        let block_id = u16::from_le_bytes([pair[0], pair[1]]);
                        if u32::from(block_id) >= block_count {
                            return Err(SearchError::Format(
                                "vNext q3 raw posting block out of range".into(),
                            ));
                        }
                        if previous.is_some_and(|old| block_id <= old) {
                            return Err(SearchError::Format(
                                "vNext q3 raw posting block IDs are not strictly increasing".into(),
                            ));
                        }
                        previous = Some(block_id);
                    }
                    expected_posting_off = posting_end;
                    value
                }
                VNextQ3PostingEncoding::DenseBitmap => {
                    let expected_bytes = (block_count as usize).div_ceil(8);
                    if value < 2
                        || posting_off != expected_posting_off
                        || posting_bytes != expected_bytes
                        || value * 2 <= expected_bytes
                    {
                        return Err(SearchError::Format(
                            "invalid vNext q3 dense bitmap metadata".into(),
                        ));
                    }
                    let posting_end = posting_off.checked_add(posting_bytes).ok_or_else(|| {
                        SearchError::Format("vNext q3 bitmap range overflow".into())
                    })?;
                    let data = postings.get(posting_off..posting_end).ok_or_else(|| {
                        SearchError::Format("vNext q3 bitmap out of bounds".into())
                    })?;
                    if !block_count.is_multiple_of(8) && !data.is_empty() {
                        let valid_bits = block_count as usize % 8;
                        let invalid_mask = !((1u8 << valid_bits) - 1);
                        if data[data.len() - 1] & invalid_mask != 0 {
                            return Err(SearchError::Format(
                                "vNext q3 bitmap has bits past block count".into(),
                            ));
                        }
                    }
                    let popcount = data
                        .iter()
                        .map(|byte| byte.count_ones() as usize)
                        .sum::<usize>();
                    if popcount != value {
                        return Err(SearchError::Format(
                            "vNext q3 bitmap cardinality mismatch".into(),
                        ));
                    }
                    expected_posting_off = posting_end;
                    value
                }
                VNextQ3PostingEncoding::Empty => {
                    return Err(SearchError::Format(
                        "empty encoding is not valid for a present vNext q3 key".into(),
                    ));
                }
            };
            total_posting_ids = total_posting_ids
                .checked_add(cardinality as u64)
                .ok_or_else(|| SearchError::Format("vNext q3 posting count overflow".into()))?;
        }
        expected_dict_off += dict_len;
        total_keys = total_keys
            .checked_add(key_count as u32)
            .ok_or_else(|| SearchError::Format("vNext q3 key count overflow".into()))?;
    }
    if expected_dict_off != dictionary.len() || expected_posting_off != postings.len() {
        return Err(SearchError::Format(
            "vNext q3 section coverage mismatch".into(),
        ));
    }
    Ok((total_keys, total_posting_ids, active_shards))
}

fn decode_posting<'a>(
    shard: &[u8],
    meta_off: usize,
    postings: &'a [u8],
    block_count: u32,
) -> Result<VNextQ3Posting<'a>, SearchError> {
    let encoding = read_encoding(shard, meta_off)?;
    let value = rd_u32(shard, meta_off + 4)? as usize;
    let posting_off = rd_u32(shard, meta_off + 8)? as usize;
    let posting_bytes = rd_u32(shard, meta_off + 12)? as usize;
    match encoding {
        VNextQ3PostingEncoding::Empty => Err(SearchError::Format(
            "empty encoding is not valid for a present vNext q3 key".into(),
        )),
        VNextQ3PostingEncoding::Singleton => {
            let block_id = u16::try_from(value)
                .map_err(|_| SearchError::Format("vNext q3 singleton exceeds u16".into()))?;
            if u32::from(block_id) >= block_count || posting_off != 0 || posting_bytes != 0 {
                return Err(SearchError::Format(
                    "invalid vNext q3 singleton metadata".into(),
                ));
            }
            Ok(VNextQ3Posting::singleton(&postings[..0], block_id))
        }
        VNextQ3PostingEncoding::RawU16 => {
            let end = posting_off
                .checked_add(posting_bytes)
                .ok_or_else(|| SearchError::Format("vNext q3 raw range overflow".into()))?;
            if value < 2 || posting_bytes != value * 2 {
                return Err(SearchError::Format(
                    "invalid vNext q3 raw posting metadata".into(),
                ));
            }
            let data = postings
                .get(posting_off..end)
                .ok_or_else(|| SearchError::Format("vNext q3 raw posting out of bounds".into()))?;
            Ok(VNextQ3Posting::raw_u16(data, value))
        }
        VNextQ3PostingEncoding::DenseBitmap => {
            let expected_bytes = (block_count as usize).div_ceil(8);
            let end = posting_off
                .checked_add(posting_bytes)
                .ok_or_else(|| SearchError::Format("vNext q3 bitmap range overflow".into()))?;
            if value < 2 || posting_bytes != expected_bytes || value * 2 <= expected_bytes {
                return Err(SearchError::Format(
                    "invalid vNext q3 dense bitmap metadata".into(),
                ));
            }
            let data = postings
                .get(posting_off..end)
                .ok_or_else(|| SearchError::Format("vNext q3 bitmap out of bounds".into()))?;
            Ok(VNextQ3Posting::dense_bitmap(
                data,
                value,
                block_count as usize,
            ))
        }
    }
}

fn read_encoding(bytes: &[u8], off: usize) -> Result<VNextQ3PostingEncoding, SearchError> {
    match *bytes
        .get(off)
        .ok_or_else(|| SearchError::Format("vNext q3 encoding read out of bounds".into()))?
    {
        POSTING_ENCODING_SINGLETON => Ok(VNextQ3PostingEncoding::Singleton),
        POSTING_ENCODING_RAW_U16 => Ok(VNextQ3PostingEncoding::RawU16),
        POSTING_ENCODING_DENSE_BITMAP => Ok(VNextQ3PostingEncoding::DenseBitmap),
        _ => Err(SearchError::Format(
            "unknown vNext q3 posting encoding".into(),
        )),
    }
}

fn radix_sort_u32_upper16_stable(
    values: &mut Vec<u32>,
    scratch: &mut Vec<u32>,
    counts: &mut Vec<usize>,
    profile: Option<&mut VNextQ3BuildProfile>,
) {
    if values.len() < 2 {
        return;
    }
    let mut profile = profile;
    let prepare_started = profile.as_ref().map(|_| Instant::now());
    scratch.clear();
    scratch.resize(values.len(), 0);
    if let (Some(profile), Some(started)) = (profile.as_mut(), prepare_started) {
        profile.radix_prepare_ms += started.elapsed().as_secs_f64() * 1000.0;
    }

    if values.len() <= Q3_RADIX8_MAX_VALUES {
        // After periodic/local dedup, content q3 shards are typically only a few thousand
        // pairs. A full 65,536-entry count clear+prefix walk then costs more than another
        // linear pass over the actual pairs. Stable LSD radix over the two suffix bytes keeps
        // owner order byte-identical while using only 256 counters on the stack.
        let mut small_counts = [0usize; 256];

        let count_started = profile.as_ref().map(|_| Instant::now());
        for &value in values.iter() {
            small_counts[((value >> 16) & 0xff) as usize] += 1;
        }
        if let (Some(profile), Some(started)) = (profile.as_mut(), count_started) {
            profile.radix_count_ms += started.elapsed().as_secs_f64() * 1000.0;
        }

        let prefix_started = profile.as_ref().map(|_| Instant::now());
        let mut sum = 0usize;
        for count in &mut small_counts {
            let current = *count;
            *count = sum;
            sum += current;
        }
        if let (Some(profile), Some(started)) = (profile.as_mut(), prefix_started) {
            profile.radix_prefix_ms += started.elapsed().as_secs_f64() * 1000.0;
        }

        let scatter_started = profile.as_ref().map(|_| Instant::now());
        for &value in values.iter() {
            let bucket = ((value >> 16) & 0xff) as usize;
            scratch[small_counts[bucket]] = value;
            small_counts[bucket] += 1;
        }
        small_counts.fill(0);
        for &value in scratch.iter() {
            small_counts[(value >> 24) as usize] += 1;
        }
        let mut sum = 0usize;
        for count in &mut small_counts {
            let current = *count;
            *count = sum;
            sum += current;
        }
        for &value in scratch.iter() {
            let bucket = (value >> 24) as usize;
            values[small_counts[bucket]] = value;
            small_counts[bucket] += 1;
        }
        if let (Some(profile), Some(started)) = (profile.as_mut(), scatter_started) {
            profile.radix_scatter_ms += started.elapsed().as_secs_f64() * 1000.0;
        }
        return;
    }

    if counts.len() != 65_536 {
        counts.resize(65_536, 0);
    }
    let count_started = profile.as_ref().map(|_| Instant::now());
    counts.fill(0);
    for &value in values.iter() {
        counts[(value >> 16) as usize] += 1;
    }
    if let (Some(profile), Some(started)) = (profile.as_mut(), count_started) {
        profile.radix_count_ms += started.elapsed().as_secs_f64() * 1000.0;
    }

    let prefix_started = profile.as_ref().map(|_| Instant::now());
    let mut sum = 0usize;
    for count in counts.iter_mut() {
        let current = *count;
        *count = sum;
        sum += current;
    }
    if let (Some(profile), Some(started)) = (profile.as_mut(), prefix_started) {
        profile.radix_prefix_ms += started.elapsed().as_secs_f64() * 1000.0;
    }

    let scatter_started = profile.as_ref().map(|_| Instant::now());
    for &value in values.iter() {
        let bucket = (value >> 16) as usize;
        scratch[counts[bucket]] = value;
        counts[bucket] += 1;
    }
    core::mem::swap(values, scratch);
    if let (Some(profile), Some(started)) = (profile.as_mut(), scatter_started) {
        profile.radix_scatter_ms += started.elapsed().as_secs_f64() * 1000.0;
    }
}

fn checked_u32(value: usize, label: &str) -> Result<u32, SearchError> {
    u32::try_from(value)
        .map_err(|_| SearchError::InvalidArgument(format!("vNext {label} exceeds u32")))
}

fn rd_u32(bytes: &[u8], off: usize) -> Result<u32, SearchError> {
    let data = bytes
        .get(off..off + 4)
        .ok_or_else(|| SearchError::Format("vNext q3 u32 read out of bounds".into()))?;
    Ok(u32::from_le_bytes(
        data.try_into().expect("fixed vNext q3 u32 slice"),
    ))
}

fn put_u32_at(bytes: &mut [u8], off: usize, value: u32) -> Result<(), SearchError> {
    let out = bytes
        .get_mut(off..off + 4)
        .ok_or_else(|| SearchError::Format("vNext q3 u32 write out of bounds".into()))?;
    out.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_pair_sort(mut values: Vec<u32>) -> Vec<u32> {
        values.sort_unstable();
        values
    }

    #[test]
    fn monotonic_owner_upper16_radix_matches_full_pair_sort() {
        let mut values = Vec::new();
        for owner in 0u16..=511 {
            let suffixes = [
                owner.wrapping_mul(257),
                0x0001,
                0xfffe,
                owner.rotate_left(5),
                owner.wrapping_mul(257),
            ];
            for suffix in suffixes {
                values.push((u32::from(suffix) << 16) | u32::from(owner));
            }
        }
        let expected = full_pair_sort(values.clone());
        let mut scratch = Vec::new();
        let mut counts = vec![0usize; 65_536];
        radix_sort_u32_upper16_stable(&mut values, &mut scratch, &mut counts, None);
        assert_eq!(values, expected);
    }

    #[test]
    fn large_monotonic_owner_radix_fallback_matches_full_pair_sort() {
        let mut values = Vec::new();
        for owner in 0u16..4_096 {
            for salt in 0u16..9 {
                let suffix = owner
                    .wrapping_mul(257)
                    .wrapping_add(salt.wrapping_mul(7_919));
                values.push((u32::from(suffix) << 16) | u32::from(owner));
            }
        }
        assert!(values.len() > Q3_RADIX8_MAX_VALUES);
        let expected = full_pair_sort(values.clone());
        let mut scratch = Vec::new();
        let mut counts = Vec::new();
        radix_sort_u32_upper16_stable(&mut values, &mut scratch, &mut counts, None);
        assert_eq!(values, expected);
        assert_eq!(counts.len(), 65_536);
    }

    #[test]
    fn q3_sub_profile_counts_emitted_and_unique_pairs() {
        let docs = vec![VNextDocumentInput::new(1, "a.txt", b"aaaa".to_vec())];
        let data = build_q3_index_with_workers(&docs, &[0], 8, 1, true, 1).unwrap();
        assert_eq!(data.build_profile.occurrences, 2);
        assert_eq!(data.build_profile.radix_occurrences, 2);
        assert_eq!(data.build_profile.local_dedup_saved, 0);
        assert_eq!(data.build_profile.unique_pairs, 1);
        assert_eq!(data.posting_ids, 1);
    }

    fn assert_q3_data_equivalent(left: &VNextQ3Data, right: &VNextQ3Data) {
        assert_eq!(left.shard_dir, right.shard_dir);
        assert_eq!(left.dictionary, right.dictionary);
        assert_eq!(left.postings, right.postings);
        assert_eq!(left.key_count, right.key_count);
        assert_eq!(left.posting_ids, right.posting_ids);
        assert_eq!(left.active_shards, right.active_shards);
        assert_eq!(left.singleton_keys, right.singleton_keys);
        assert_eq!(left.raw_u16_keys, right.raw_u16_keys);
        assert_eq!(left.dense_bitmap_keys, right.dense_bitmap_keys);
    }

    #[test]
    fn adaptive_owner_local_dedup_is_byte_equivalent_to_global_only_dedup() {
        let mut content = Vec::new();
        for _ in 0..300 {
            content.extend_from_slice(b"abcabc");
        }
        let docs = vec![VNextDocumentInput::new(1, "repeated.txt", content)];
        let adaptive = build_q3_index_impl(
            &docs,
            &[0],
            8_192,
            1,
            true,
            Q3BuildOptions {
                local_dedup: true,
                q2_projection: false,
            },
            1,
        )
        .unwrap();
        let direct = build_q3_index_impl(
            &docs,
            &[0],
            8_192,
            1,
            true,
            Q3BuildOptions {
                local_dedup: false,
                q2_projection: false,
            },
            1,
        )
        .unwrap();

        assert_q3_data_equivalent(&adaptive, &direct);
        assert_eq!(adaptive.build_profile.local_dedup_blocks, 1);
        assert_eq!(adaptive.build_profile.direct_blocks, 0);
        assert!(adaptive.build_profile.local_dedup_saved > 1_000);
        assert!(adaptive.build_profile.radix_occurrences < adaptive.build_profile.occurrences);
        assert_eq!(
            adaptive.build_profile.radix_occurrences,
            adaptive
                .build_profile
                .occurrences
                .saturating_sub(adaptive.build_profile.local_dedup_saved)
        );
    }

    #[test]
    fn periodic_suffix_skip_is_byte_equivalent_to_global_only_dedup() {
        let mut content = b"header: exact periodic suffix\n".to_vec();
        while content.len() < 7_500 {
            content.extend_from_slice(b"abcdefghijklmnopqrstuvwxyz");
        }
        content.truncate(7_500);
        let docs = vec![VNextDocumentInput::new(1, "periodic.txt", content)];
        let adaptive = build_q3_index_impl(
            &docs,
            &[0],
            8_192,
            1,
            true,
            Q3BuildOptions {
                local_dedup: true,
                q2_projection: false,
            },
            1,
        )
        .unwrap();
        let direct = build_q3_index_impl(
            &docs,
            &[0],
            8_192,
            1,
            true,
            Q3BuildOptions {
                local_dedup: false,
                q2_projection: false,
            },
            1,
        )
        .unwrap();

        assert_q3_data_equivalent(&adaptive, &direct);
        assert!(adaptive.build_profile.periodic_skip_blocks >= 1);
        assert!(adaptive.build_profile.periodic_skip_occurrences > 4_000);
    }

    #[test]
    fn periodic_suffix_probe_fails_closed_on_late_mutation() {
        let mut content = Vec::new();
        while content.len() < 7_500 {
            content.extend_from_slice(b"abcdefghijklmnopqrstuvwxyz");
        }
        content.truncate(7_500);
        content[7_123] ^= 0x5a;
        let docs = vec![VNextDocumentInput::new(1, "near-periodic.bin", content)];
        let adaptive = build_q3_index_impl(
            &docs,
            &[0],
            8_192,
            1,
            true,
            Q3BuildOptions {
                local_dedup: true,
                q2_projection: false,
            },
            1,
        )
        .unwrap();
        let direct = build_q3_index_impl(
            &docs,
            &[0],
            8_192,
            1,
            true,
            Q3BuildOptions {
                local_dedup: false,
                q2_projection: false,
            },
            1,
        )
        .unwrap();

        assert_q3_data_equivalent(&adaptive, &direct);
        assert!(adaptive.build_profile.local_dedup_saved > 1_000);
    }

    #[test]
    fn recent_q3_front_cache_collisions_preserve_exact_bytes() {
        let mut content = Vec::new();
        for _ in 0..400 {
            // Several distinct q3s end in the same byte and therefore collide in the direct-mapped
            // recent cache. Those collisions must only become cache misses; LocalQ3Set remains the
            // exact oracle for owner-local membership.
            content.extend_from_slice(b"abXcdXefXghX");
        }
        let docs = vec![VNextDocumentInput::new(1, "recent-collision.txt", content)];
        let adaptive = build_q3_index_impl(
            &docs,
            &[0],
            8_192,
            1,
            true,
            Q3BuildOptions {
                local_dedup: true,
                q2_projection: false,
            },
            1,
        )
        .unwrap();
        let direct = build_q3_index_impl(
            &docs,
            &[0],
            8_192,
            1,
            true,
            Q3BuildOptions {
                local_dedup: false,
                q2_projection: false,
            },
            1,
        )
        .unwrap();

        assert_q3_data_equivalent(&adaptive, &direct);
        assert_eq!(adaptive.build_profile.local_dedup_blocks, 1);
        assert!(adaptive.build_profile.local_dedup_saved > 1_000);
    }

    #[test]
    fn deferred_local_hash_fallback_can_still_prove_a_late_periodic_suffix() {
        let mut content = Vec::with_capacity(7_500);
        let mut state = 0x1234_5678_9abc_def0u64;
        for _ in 0..600 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            content.push((state >> 24) as u8);
        }
        while content.len() < 7_500 {
            content.extend_from_slice(b"abcdefghijklmnopqrstuvwxyz");
        }
        content.truncate(7_500);
        let docs = vec![VNextDocumentInput::new(1, "late-periodic.bin", content)];
        let adaptive = build_q3_index_impl(
            &docs,
            &[0],
            8_192,
            1,
            true,
            Q3BuildOptions {
                local_dedup: true,
                q2_projection: false,
            },
            1,
        )
        .unwrap();
        let direct = build_q3_index_impl(
            &docs,
            &[0],
            8_192,
            1,
            true,
            Q3BuildOptions {
                local_dedup: false,
                q2_projection: false,
            },
            1,
        )
        .unwrap();

        assert_q3_data_equivalent(&adaptive, &direct);
        assert_eq!(adaptive.build_profile.local_dedup_blocks, 1);
        assert_eq!(adaptive.build_profile.local_hash_blocks, 1);
        assert!(adaptive.build_profile.periodic_skip_blocks >= 1);
        assert!(adaptive.build_profile.periodic_skip_occurrences > 4_000);
    }

    #[test]
    fn adaptive_owner_local_dedup_keeps_low_repetition_block_on_direct_path() {
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut content = Vec::with_capacity(1_024);
        for _ in 0..1_024 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            content.push((state >> 24) as u8);
        }
        let docs = vec![VNextDocumentInput::new(1, "random.bin", content)];
        let data = build_q3_index_with_workers(&docs, &[0], 8_192, 1, true, 1).unwrap();
        assert_eq!(data.build_profile.local_dedup_blocks, 0);
        assert_eq!(data.build_profile.direct_blocks, 1);
        assert_eq!(data.build_profile.local_dedup_saved, 0);
        assert_eq!(
            data.build_profile.radix_occurrences,
            data.build_profile.occurrences
        );
    }

    #[test]
    fn segment_sampling_keeps_sufficient_high_entropy_sample_on_direct_path() {
        let mut docs = Vec::new();
        let mut state = 0x6a09_e667_f3bc_c909u64;
        for doc_id in 0..16u64 {
            let mut content = Vec::with_capacity(1_024);
            for _ in 0..1_024 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                content.push((state >> 24) as u8);
            }
            docs.push(VNextDocumentInput::new(
                doc_id + 1,
                format!("random-{doc_id}.bin"),
                content,
            ));
        }
        let first_blocks = (0u16..16).collect::<Vec<_>>();
        let adaptive = build_q3_index_impl(
            &docs,
            &first_blocks,
            8_192,
            16,
            true,
            Q3BuildOptions {
                local_dedup: true,
                q2_projection: false,
            },
            1,
        )
        .unwrap();
        let direct = build_q3_index_impl(
            &docs,
            &first_blocks,
            8_192,
            16,
            true,
            Q3BuildOptions {
                local_dedup: false,
                q2_projection: false,
            },
            1,
        )
        .unwrap();

        assert_q3_data_equivalent(&adaptive, &direct);
        assert!(
            adaptive.build_profile.local_sample_occurrences
                >= LOCAL_Q3_DEDUP_MIN_SAMPLE_OCCURRENCES
        );
        assert!(
            adaptive
                .build_profile
                .local_sample_duplicates
                .saturating_mul(1_000_000)
                < adaptive
                    .build_profile
                    .local_sample_occurrences
                    .saturating_mul(LOCAL_Q3_DEDUP_THRESHOLD_PPM)
        );
        assert_eq!(adaptive.build_profile.local_dedup_blocks, 0);
        assert_eq!(adaptive.build_profile.direct_blocks, 16);
        assert_eq!(adaptive.build_profile.local_dedup_saved, 0);
        assert_eq!(
            adaptive.build_profile.radix_occurrences,
            adaptive.build_profile.occurrences
        );
    }

    #[test]
    fn local_q3_set_handles_zero_max_key_and_generation_wrap() {
        let mut set = LocalQ3Set::default();
        let mask = set.reset_for(8).unwrap();
        assert!(set.insert(0, mask));
        assert!(!set.insert(0, mask));
        assert!(set.insert(0x00ff_ffff, mask));
        assert!(!set.insert(0x00ff_ffff, mask));

        set.generation = u16::MAX;
        let mask = set.reset_for(2_048).unwrap();
        assert!(set.use_generation);
        assert_eq!(set.generation, 1);
        assert!(set.insert(0, mask));
        assert!(!set.insert(0, mask));
        assert!(set.insert(0x00ff_ffff, mask));
    }

    #[test]
    fn monotonic_owner_upper16_radix_keeps_duplicates_adjacent_for_dedup() {
        let mut values = Vec::new();
        for owner in 0u16..64 {
            for suffix in [7u16, 3, 7, 9, 3, 7] {
                values.push((u32::from(suffix) << 16) | u32::from(owner));
            }
        }
        let mut expected = full_pair_sort(values.clone());
        expected.dedup();
        let mut scratch = Vec::new();
        let mut counts = vec![0usize; 65_536];
        radix_sort_u32_upper16_stable(&mut values, &mut scratch, &mut counts, None);
        values.dedup();
        assert_eq!(values, expected);
    }

    #[test]
    fn bounded_parallel_q3_is_byte_equivalent_to_single_worker() {
        let mut docs = Vec::new();
        let mut first_blocks = Vec::new();
        let mut next_block = 0u16;
        let mut state = 0x243f_6a88_85a3_08d3u64;
        for doc_id in 0..96u64 {
            let mut content = Vec::with_capacity(16_384);
            for _ in 0..16_384 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                content.push((state >> 24) as u8);
            }
            first_blocks.push(next_block);
            next_block = next_block.checked_add(2).unwrap();
            docs.push(VNextDocumentInput::new(
                doc_id + 1,
                format!("parallel-{doc_id}.bin"),
                content,
            ));
        }
        let single = build_q3_index_with_workers(
            &docs,
            &first_blocks,
            8_192,
            usize::from(next_block),
            true,
            1,
        )
        .unwrap();
        let parallel = build_q3_index_with_workers(
            &docs,
            &first_blocks,
            8_192,
            usize::from(next_block),
            true,
            4,
        )
        .unwrap();

        assert_q3_data_equivalent(&single, &parallel);
        assert!(parallel.build_profile.emit_workers >= 2);
        assert!(parallel.build_profile.shard_workers >= 2);
        assert_eq!(
            parallel.build_profile.occurrences,
            single.build_profile.occurrences
        );
        assert_eq!(
            parallel.build_profile.unique_pairs,
            single.build_profile.unique_pairs
        );
    }

    #[test]
    fn q3_worker_count_is_bounded_by_work_and_items() {
        assert_eq!(q3_effective_worker_count(4, 100, 100, 1_000), 1);
        assert_eq!(q3_effective_worker_count(4, 1, 10_000, 1_000), 1);
        assert_eq!(q3_effective_worker_count(4, 100, 2_100, 1_000), 3);
        assert_eq!(q3_effective_worker_count(8, 100, 20_000, 1_000), 8);
    }
}
