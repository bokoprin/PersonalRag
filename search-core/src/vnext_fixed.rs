use crate::format::SearchError;
use crate::vnext_segment::VNextDocumentInput;

const HEADER_SIZE: usize = 32;
const META_SIZE: usize = 16;
const MAGIC_DENSE: &[u8; 8] = b"PRFIX001";
const MAGIC_SPARSE: &[u8; 8] = b"PRFIX002";
const ENCODING_EMPTY: u8 = 0;
const ENCODING_SINGLETON: u8 = 1;
const ENCODING_RAW_U16: u8 = 2;
const ENCODING_DENSE_BITMAP: u8 = 3;
// Sharding wins while the q2 stream is small enough to stay cache-friendly, but on large
// segments its scattered writes compete with q3 for memory bandwidth. A conservative 6M-start
// gate keeps the measured 512B/1KiB wins and falls back before the 2KiB crossover.
const Q2_SHARDED_MAX_LOGICAL_OCCURRENCES: u64 = 6_000_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FixedIndexStats {
    pub keys: u32,
    pub posting_ids: u64,
    pub singleton_keys: u32,
    pub raw_u16_keys: u32,
    pub dense_bitmap_keys: u32,
    pub posting_bytes: u64,
}

#[derive(Debug)]
pub(crate) struct FixedIndexData {
    pub bytes: Vec<u8>,
    pub stats: FixedIndexStats,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FixedPostingEncoding {
    Empty,
    Singleton,
    RawU16,
    DenseBitmap,
}

#[derive(Clone, Copy)]
pub(crate) struct FixedPosting<'a> {
    encoding: FixedPostingEncoding,
    cardinality: usize,
    singleton: u16,
    bytes: &'a [u8],
    universe: usize,
}

impl<'a> FixedPosting<'a> {
    #[must_use]
    pub const fn len(&self) -> usize {
        self.cardinality
    }
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.cardinality == 0
    }
    #[must_use]
    pub const fn encoding(&self) -> FixedPostingEncoding {
        self.encoding
    }
    #[must_use]
    pub fn iter(&self) -> FixedPostingIter<'a> {
        FixedPostingIter {
            posting: *self,
            cursor: 0,
            remaining: self.cardinality,
        }
    }
}

pub(crate) struct FixedPostingIter<'a> {
    posting: FixedPosting<'a>,
    cursor: usize,
    remaining: usize,
}

impl Iterator for FixedPostingIter<'_> {
    type Item = u16;
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let value = match self.posting.encoding {
            FixedPostingEncoding::Empty => return None,
            FixedPostingEncoding::Singleton => {
                self.cursor = 1;
                self.posting.singleton
            }
            FixedPostingEncoding::RawU16 => {
                let off = self.cursor.checked_mul(2)?;
                let pair = self.posting.bytes.get(off..off + 2)?;
                self.cursor += 1;
                u16::from_le_bytes([pair[0], pair[1]])
            }
            FixedPostingEncoding::DenseBitmap => loop {
                if self.cursor >= self.posting.universe {
                    return None;
                }
                let id = self.cursor;
                self.cursor += 1;
                if self.posting.bytes[id / 8] & (1u8 << (id % 8)) != 0 {
                    break u16::try_from(id).ok()?;
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
impl ExactSizeIterator for FixedPostingIter<'_> {}

pub(crate) fn build_content_fixed_index(
    docs: &[VNextDocumentInput],
    first_blocks: &[u16],
    block_size: u32,
    block_count: usize,
    gram_len: usize,
) -> Result<FixedIndexData, SearchError> {
    if !matches!(gram_len, 1 | 2) {
        return Err(SearchError::InvalidArgument(
            "fixed content gram length must be 1 or 2".into(),
        ));
    }
    if block_size == 0 {
        return Err(SearchError::InvalidArgument(
            "block size must be non-zero".into(),
        ));
    }
    if gram_len == 2 {
        return build_content_q2_sparse(docs, first_blocks, block_size, block_count);
    }

    build_content_q1_dense(docs, first_blocks, block_size, block_count)
}

fn build_content_q1_dense(
    docs: &[VNextDocumentInput],
    first_blocks: &[u16],
    block_size: u32,
    block_count: usize,
) -> Result<FixedIndexData, SearchError> {
    let mut lists = (0..256).map(|_| Vec::<u16>::new()).collect::<Vec<_>>();
    let block_size = usize::try_from(block_size)
        .map_err(|_| SearchError::InvalidArgument("block size too large".into()))?;

    for (doc_index, doc) in docs.iter().enumerate() {
        let first = usize::from(first_blocks[doc_index]);
        for (local_block, chunk) in doc.normalized_content.chunks(block_size).enumerate() {
            let owner = first
                .checked_add(local_block)
                .ok_or_else(|| SearchError::Format("fixed content block overflow".into()))?;
            if owner >= block_count {
                return Err(SearchError::Format(
                    "fixed content block out of range".into(),
                ));
            }
            let owner = u16::try_from(owner)
                .map_err(|_| SearchError::Format("block ID exceeds u16".into()))?;

            // q1 only needs one posting per byte value and block. Build a tiny block-local
            // 256-bit set, then append the owner once per present key. This removes the hot-loop
            // division and per-byte stamp comparison while preserving the exact ascending-owner
            // posting order produced by the previous implementation.
            let mut seen = [0u64; 4];
            for &byte in chunk {
                let key = usize::from(byte);
                seen[key / 64] |= 1u64 << (key % 64);
            }
            for (word_index, mut word) in seen.into_iter().enumerate() {
                while word != 0 {
                    let bit = word.trailing_zeros() as usize;
                    lists[word_index * 64 + bit].push(owner);
                    word &= word - 1;
                }
            }
        }
    }

    encode_dense_fixed_index(1, block_count, lists)
}

#[inline]
fn use_sharded_content_q2(logical_occurrences: u64) -> bool {
    logical_occurrences <= Q2_SHARDED_MAX_LOGICAL_OCCURRENCES
}

fn build_content_q2_sparse(
    docs: &[VNextDocumentInput],
    first_blocks: &[u16],
    block_size: u32,
    block_count: usize,
) -> Result<FixedIndexData, SearchError> {
    let logical_occurrences = docs
        .iter()
        .map(|doc| doc.normalized_content.len().saturating_sub(1) as u64)
        .sum::<u64>();
    let sharded = use_sharded_content_q2(logical_occurrences);
    if std::env::var_os("PR_PROFILE_BUILD").is_some() {
        eprintln!(
            "VNEXT_Q2_LAYOUT logical_occurrences={} layout={}",
            logical_occurrences,
            if sharded { "sharded8x8" } else { "flat16" }
        );
    }
    if sharded {
        build_content_q2_sparse_sharded(docs, first_blocks, block_size, block_count)
    } else {
        build_content_q2_sparse_flat(docs, first_blocks, block_size, block_count)
    }
}

fn build_content_q2_sparse_sharded(
    docs: &[VNextDocumentInput],
    first_blocks: &[u16],
    block_size: u32,
    block_count: usize,
) -> Result<FixedIndexData, SearchError> {
    let block_size = usize::try_from(block_size)
        .map_err(|_| SearchError::InvalidArgument("block size too large".into()))?;
    let mut shards = (0..256).map(|_| Vec::<u32>::new()).collect::<Vec<_>>();
    let mut seen = vec![0u64; 65_536 / 64];
    let mut global_keys = vec![0u64; 65_536 / 64];
    let mut touched_words = Vec::<usize>::new();
    for (doc_index, doc) in docs.iter().enumerate() {
        if doc.normalized_content.len() < 2 {
            continue;
        }
        let first = usize::from(first_blocks[doc_index]);
        let last_start_exclusive = doc.normalized_content.len() - 1;
        let mut block_start = 0usize;
        let mut local_block = 0usize;
        while block_start < last_start_exclusive {
            let block_end = block_start
                .saturating_add(block_size)
                .min(last_start_exclusive);
            let owner = first
                .checked_add(local_block)
                .ok_or_else(|| SearchError::Format("fixed content block overflow".into()))?;
            if owner >= block_count {
                return Err(SearchError::Format(
                    "fixed content block out of range".into(),
                ));
            }
            let owner = u16::try_from(owner)
                .map_err(|_| SearchError::Format("block ID exceeds u16".into()))?;
            touched_words.clear();
            for start in block_start..block_end {
                let key = (u16::from(doc.normalized_content[start]) << 8)
                    | u16::from(doc.normalized_content[start + 1]);
                let key_index = usize::from(key);
                let word = key_index / 64;
                let mask = 1u64 << (key_index % 64);
                if seen[word] & mask == 0 {
                    if seen[word] == 0 {
                        touched_words.push(word);
                    }
                    seen[word] |= mask;
                    global_keys[word] |= mask;
                    let shard = usize::from((key >> 8) as u8);
                    shards[shard].push((u32::from(key) << 16) | u32::from(owner));
                }
            }
            for &word in &touched_words {
                seen[word] = 0;
            }
            block_start = block_end;
            local_block += 1;
        }
    }
    let key_count = global_keys
        .iter()
        .map(|word| word.count_ones() as usize)
        .sum::<usize>();
    encode_sparse_q2_shards(block_count, shards, key_count)
}

fn build_content_q2_sparse_flat(
    docs: &[VNextDocumentInput],
    first_blocks: &[u16],
    block_size: u32,
    block_count: usize,
) -> Result<FixedIndexData, SearchError> {
    let block_size = usize::try_from(block_size)
        .map_err(|_| SearchError::InvalidArgument("block size too large".into()))?;
    let mut pairs = Vec::<u32>::new();
    // Owner block IDs are unique and bounded below u16::MAX inside one segment. Record the
    // last owner that emitted each q2 key instead of clearing an 8 KiB membership bitmap for
    // every block. This removes word/mask math and touched-word cleanup from the q2 hot loop
    // while preserving the exact key/owner pairs and their monotonic owner order.
    let mut last_owner = vec![u16::MAX; 65_536];
    for (doc_index, doc) in docs.iter().enumerate() {
        if doc.normalized_content.len() < 2 {
            continue;
        }
        let first = usize::from(first_blocks[doc_index]);
        let last_start_exclusive = doc.normalized_content.len() - 1;
        let mut block_start = 0usize;
        let mut local_block = 0usize;
        while block_start < last_start_exclusive {
            let block_end = block_start
                .saturating_add(block_size)
                .min(last_start_exclusive);
            let owner = first
                .checked_add(local_block)
                .ok_or_else(|| SearchError::Format("fixed content block overflow".into()))?;
            if owner >= block_count {
                return Err(SearchError::Format(
                    "fixed content block out of range".into(),
                ));
            }
            let owner = u16::try_from(owner)
                .map_err(|_| SearchError::Format("block ID exceeds u16".into()))?;
            for start in block_start..block_end {
                let key = (u16::from(doc.normalized_content[start]) << 8)
                    | u16::from(doc.normalized_content[start + 1]);
                let key_index = usize::from(key);
                if last_owner[key_index] != owner {
                    last_owner[key_index] = owner;
                    pairs.push((u32::from(key) << 16) | u32::from(owner));
                }
            }
            block_start = block_end;
            local_block += 1;
        }
    }
    encode_sparse_q2_pairs(block_count, pairs)
}

pub(crate) fn build_content_q1_q2_from_q3_emission(
    block_count: usize,
    q1_lists: Vec<Vec<u16>>,
    q2_pairs: Vec<u32>,
) -> Result<(FixedIndexData, FixedIndexData), SearchError> {
    if q1_lists.len() != 256 {
        return Err(SearchError::Format(
            "q3-emitted q1 list domain mismatch".into(),
        ));
    }
    debug_assert!(
        q1_lists
            .iter()
            .all(|owners| owners.windows(2).all(|pair| pair[0] < pair[1]))
    );
    debug_assert!(
        q2_pairs
            .windows(2)
            .all(|pair| (pair[0] as u16) <= (pair[1] as u16))
    );
    let q1 = encode_dense_fixed_index(1, block_count, q1_lists)?;
    let q2 = encode_sparse_q2_pairs(block_count, q2_pairs)?;
    Ok((q1, q2))
}

fn flush_projected_q2_group(
    key: u16,
    owners: &[u16],
    universe: usize,
    metadata: &mut Vec<u8>,
    postings: &mut Vec<u8>,
    stats: &mut FixedIndexStats,
) -> Result<(), SearchError> {
    if owners.is_empty() {
        return Ok(());
    }
    debug_assert!(owners.windows(2).all(|pair| pair[0] < pair[1]));
    let meta = metadata.len();
    metadata.resize(meta + META_SIZE, 0);
    put_u16(metadata, meta, key)?;
    put_u32(metadata, meta + 8, owners.len() as u32)?;
    stats.keys += 1;
    stats.posting_ids += owners.len() as u64;
    if owners.len() == 1 {
        metadata[meta + 2] = ENCODING_SINGLETON;
        put_u16(metadata, meta + 4, owners[0])?;
        stats.singleton_keys += 1;
        return Ok(());
    }

    let raw_bytes = owners
        .len()
        .checked_mul(2)
        .ok_or_else(|| SearchError::Format("projected q2 raw size overflow".into()))?;
    let bitmap_bytes = universe.div_ceil(8);
    put_u32(metadata, meta + 12, postings.len() as u32)?;
    if bitmap_bytes < raw_bytes {
        metadata[meta + 2] = ENCODING_DENSE_BITMAP;
        let start = postings.len();
        postings.resize(start + bitmap_bytes, 0);
        for &owner in owners {
            let owner = usize::from(owner);
            if owner >= universe {
                return Err(SearchError::Format(
                    "projected q2 posting ID out of range".into(),
                ));
            }
            postings[start + owner / 8] |= 1u8 << (owner % 8);
        }
        stats.dense_bitmap_keys += 1;
        stats.posting_bytes += bitmap_bytes as u64;
    } else {
        metadata[meta + 2] = ENCODING_RAW_U16;
        for &owner in owners {
            if usize::from(owner) >= universe {
                return Err(SearchError::Format(
                    "projected q2 posting ID out of range".into(),
                ));
            }
            postings.extend_from_slice(&owner.to_le_bytes());
        }
        stats.raw_u16_keys += 1;
        stats.posting_bytes += raw_bytes as u64;
    }
    Ok(())
}

pub(crate) fn build_content_q1_q2_from_q3_projection(
    docs: &[VNextDocumentInput],
    first_blocks: &[u16],
    block_size: u32,
    block_count: usize,
    projected_q2: Vec<Vec<u32>>,
) -> Result<(FixedIndexData, FixedIndexData), SearchError> {
    let profile = std::env::var_os("PR_PROFILE_BUILD").is_some();
    if docs.len() != first_blocks.len() {
        return Err(SearchError::Format(
            "q3 projection document/block shape mismatch".into(),
        ));
    }
    if block_size == 0 {
        return Err(SearchError::InvalidArgument(
            "block size must be non-zero".into(),
        ));
    }
    let block_size = usize::try_from(block_size)
        .map_err(|_| SearchError::InvalidArgument("block size too large".into()))?;
    debug_assert!(
        projected_q2
            .iter()
            .all(|chunk| chunk.windows(2).all(|pair| pair[0] < pair[1]))
    );
    debug_assert!(projected_q2.windows(2).all(|chunks| {
        chunks[0]
            .last()
            .zip(chunks[1].first())
            .is_none_or(|(left, right)| left < right)
    }));

    let stream_started = profile.then(std::time::Instant::now);
    let mut tails = Vec::<u32>::with_capacity(docs.len());
    for (doc_index, doc) in docs.iter().enumerate() {
        let bytes = &doc.normalized_content;
        if bytes.len() < 2 {
            continue;
        }
        let start = bytes.len() - 2;
        let owner = usize::from(first_blocks[doc_index])
            .checked_add(start / block_size)
            .ok_or_else(|| SearchError::Format("q2 tail owner overflow".into()))?;
        if owner >= block_count {
            return Err(SearchError::Format("q2 tail owner out of range".into()));
        }
        let key = (u16::from(bytes[start]) << 8) | u16::from(bytes[start + 1]);
        tails.push((u32::from(key) << 16) | owner as u32);
    }
    tails.sort_unstable();
    tails.dedup();

    // Consume the projected q2 stream and the small tail stream directly into fixed-index
    // metadata/postings. This fuses merge, q1 derivation and q2 encoding into one pass and avoids
    // copying ~400k projected pairs into a second temporary Vec.
    let owner_words = block_count.div_ceil(64);
    let mut q1_presence = vec![0u64; 256usize.saturating_mul(owner_words)];
    let mut q2_metadata = Vec::<u8>::new();
    let mut q2_postings = Vec::<u8>::new();
    let mut q2_stats = FixedIndexStats::default();
    let mut owners = Vec::<u16>::new();
    let mut current_key = None::<u16>;
    let mut projected = projected_q2
        .iter()
        .flat_map(|chunk| chunk.iter())
        .copied()
        .peekable();
    let mut tail_stream = tails.iter().copied().peekable();
    while projected.peek().is_some() || tail_stream.peek().is_some() {
        let packed = match (projected.peek().copied(), tail_stream.peek().copied()) {
            (Some(a), Some(b)) if a < b => projected.next().expect("peeked projected q2"),
            (Some(_), Some(b)) if b < projected.peek().copied().unwrap_or(u32::MAX) => {
                tail_stream.next().expect("peeked q2 tail")
            }
            (Some(a), Some(_)) => {
                projected.next();
                tail_stream.next();
                a
            }
            (Some(_), None) => projected.next().expect("peeked projected q2"),
            (None, Some(_)) => tail_stream.next().expect("peeked q2 tail"),
            (None, None) => break,
        };
        let key = (packed >> 16) as u16;
        let owner = packed as u16;
        if usize::from(owner) >= block_count {
            return Err(SearchError::Format(
                "q3 projected q2 owner out of range".into(),
            ));
        }
        if current_key != Some(key) {
            if let Some(previous_key) = current_key {
                flush_projected_q2_group(
                    previous_key,
                    &owners,
                    block_count,
                    &mut q2_metadata,
                    &mut q2_postings,
                    &mut q2_stats,
                )?;
                owners.clear();
            }
            current_key = Some(key);
        }
        owners.push(owner);
        if owner_words != 0 {
            let q1_key = usize::from((key >> 8) as u8);
            let owner = usize::from(owner);
            q1_presence[q1_key * owner_words + owner / 64] |= 1u64 << (owner % 64);
        }
    }
    if let Some(key) = current_key {
        flush_projected_q2_group(
            key,
            &owners,
            block_count,
            &mut q2_metadata,
            &mut q2_postings,
            &mut q2_stats,
        )?;
    }

    let postings_start = HEADER_SIZE
        .checked_add(q2_metadata.len())
        .ok_or_else(|| SearchError::Format("projected q2 size overflow".into()))?;
    let mut q2_bytes = vec![0u8; HEADER_SIZE];
    q2_bytes[..8].copy_from_slice(MAGIC_SPARSE);
    put_u32(&mut q2_bytes, 8, 2)?;
    put_u32(&mut q2_bytes, 12, block_count as u32)?;
    put_u32(&mut q2_bytes, 16, q2_stats.keys)?;
    put_u32(&mut q2_bytes, 20, META_SIZE as u32)?;
    put_u32(&mut q2_bytes, 24, postings_start as u32)?;
    q2_bytes.extend_from_slice(&q2_metadata);
    q2_bytes.extend_from_slice(&q2_postings);
    debug_assert!(validate_sparse_fixed_index(&q2_bytes, 2, block_count).is_ok());
    let q2 = FixedIndexData {
        bytes: q2_bytes,
        stats: q2_stats,
    };
    let stream_ms = stream_started
        .map(|started| started.elapsed().as_secs_f64() * 1000.0)
        .unwrap_or(0.0);

    let q1_started = profile.then(std::time::Instant::now);
    // Every byte except the document-final byte is represented by a q2 start. Add final bytes
    // explicitly, preserving ownership across multi-block documents.
    for (doc_index, doc) in docs.iter().enumerate() {
        let Some(&last_byte) = doc.normalized_content.last() else {
            continue;
        };
        let start = doc.normalized_content.len() - 1;
        let owner = usize::from(first_blocks[doc_index])
            .checked_add(start / block_size)
            .ok_or_else(|| SearchError::Format("q1 final-byte owner overflow".into()))?;
        if owner >= block_count {
            return Err(SearchError::Format(
                "q1 final-byte owner out of range".into(),
            ));
        }
        if owner_words != 0 {
            let key = usize::from(last_byte);
            q1_presence[key * owner_words + owner / 64] |= 1u64 << (owner % 64);
        }
    }
    let mut q1_lists = (0..256).map(|_| Vec::<u16>::new()).collect::<Vec<_>>();
    if owner_words != 0 {
        for (key, owner_list) in q1_lists.iter_mut().enumerate() {
            for word_index in 0..owner_words {
                let mut word = q1_presence[key * owner_words + word_index];
                while word != 0 {
                    let bit = word.trailing_zeros() as usize;
                    owner_list.push((word_index * 64 + bit) as u16);
                    word &= word - 1;
                }
            }
        }
    }
    let q1 = encode_dense_fixed_index(1, block_count, q1_lists)?;
    if profile {
        let q1_ms = q1_started
            .map(|started| started.elapsed().as_secs_f64() * 1000.0)
            .unwrap_or(0.0);
        eprintln!("VNEXT_Q3_Q2_PROJECTION stream_q2_ms={stream_ms:.3} q1_finalize_ms={q1_ms:.3}");
    }
    Ok((q1, q2))
}

pub(crate) fn build_content_q1_q2_fused_if_flat(
    docs: &[VNextDocumentInput],
    first_blocks: &[u16],
    block_size: u32,
    block_count: usize,
    periodic_q2_skip_from: Option<&[u16]>,
) -> Result<Option<(FixedIndexData, FixedIndexData)>, SearchError> {
    let logical_occurrences = docs
        .iter()
        .map(|doc| doc.normalized_content.len().saturating_sub(1) as u64)
        .sum::<u64>();
    if use_sharded_content_q2(logical_occurrences) {
        return Ok(None);
    }
    build_content_q1_q2_flat_fused(
        docs,
        first_blocks,
        block_size,
        block_count,
        periodic_q2_skip_from,
    )
    .map(Some)
}

fn build_content_q1_q2_flat_fused(
    docs: &[VNextDocumentInput],
    first_blocks: &[u16],
    block_size: u32,
    block_count: usize,
    periodic_q2_skip_from: Option<&[u16]>,
) -> Result<(FixedIndexData, FixedIndexData), SearchError> {
    if periodic_q2_skip_from
        .is_some_and(|offsets| use_periodic_q2_active_lists(offsets, block_count))
    {
        return build_content_q1_q2_flat_fused_active_lists(
            docs,
            first_blocks,
            block_size,
            block_count,
            periodic_q2_skip_from.expect("periodic offsets checked above"),
        );
    }
    build_content_q1_q2_flat_fused_radix(
        docs,
        first_blocks,
        block_size,
        block_count,
        periodic_q2_skip_from,
    )
}

fn use_periodic_q2_active_lists(offsets: &[u16], block_count: usize) -> bool {
    if block_count == 0 || offsets.len() < block_count {
        return false;
    }
    let proven = offsets[..block_count]
        .iter()
        .filter(|&&offset| offset != u16::MAX)
        .count();
    proven.saturating_mul(4) >= block_count.saturating_mul(3)
}

fn build_content_q1_q2_flat_fused_radix(
    docs: &[VNextDocumentInput],
    first_blocks: &[u16],
    block_size: u32,
    block_count: usize,
    periodic_q2_skip_from: Option<&[u16]>,
) -> Result<(FixedIndexData, FixedIndexData), SearchError> {
    if block_size == 0 {
        return Err(SearchError::InvalidArgument(
            "block size must be non-zero".into(),
        ));
    }
    let block_size = usize::try_from(block_size)
        .map_err(|_| SearchError::InvalidArgument("block size too large".into()))?;

    let mut q1_lists = (0..256).map(|_| Vec::<u16>::new()).collect::<Vec<_>>();
    let mut q2_pairs = Vec::<u32>::new();
    let mut q2_last_owner = vec![u16::MAX; 65_536];
    for (doc_index, doc) in docs.iter().enumerate() {
        let first = usize::from(first_blocks[doc_index]);
        let bytes = &doc.normalized_content;
        for (local_block, chunk) in bytes.chunks(block_size).enumerate() {
            let owner = first
                .checked_add(local_block)
                .ok_or_else(|| SearchError::Format("fixed content block overflow".into()))?;
            if owner >= block_count {
                return Err(SearchError::Format(
                    "fixed content block out of range".into(),
                ));
            }
            let owner_index = owner;
            let owner = u16::try_from(owner_index)
                .map_err(|_| SearchError::Format("block ID exceeds u16".into()))?;
            let block_start = local_block * block_size;
            let mut q1_seen = [0u64; 4];
            let q2_process_len = periodic_q2_skip_from
                .and_then(|offsets| offsets.get(owner_index))
                .copied()
                .filter(|&offset| offset != u16::MAX)
                .map_or(chunk.len(), |offset| usize::from(offset).min(chunk.len()));

            for (offset, &byte) in chunk[..q2_process_len].iter().enumerate() {
                let start = block_start + offset;
                if start + 1 < bytes.len() {
                    let q2_key = (u16::from(byte) << 8) | u16::from(bytes[start + 1]);
                    let q2_index = usize::from(q2_key);
                    if q2_last_owner[q2_index] != owner {
                        q2_last_owner[q2_index] = owner;
                        q2_pairs.push((u32::from(q2_key) << 16) | u32::from(owner));

                        // Every byte except the document's final byte is the high byte of a q2
                        // start owned by the same block. Therefore q1 presence can be derived from
                        // the first occurrence of each owner-local q2 instead of updating the q1
                        // bitset for every content byte. Repeated q2s reuse the same high byte, so
                        // skipping them cannot remove a q1 key. The document-final byte is added
                        // once below because it has no q2 start.
                        let q1_key = usize::from(byte);
                        q1_seen[q1_key / 64] |= 1u64 << (q1_key % 64);
                    }
                }
            }
            if block_start + chunk.len() == bytes.len()
                && let Some(&last_byte) = chunk.last()
            {
                let q1_key = usize::from(last_byte);
                q1_seen[q1_key / 64] |= 1u64 << (q1_key % 64);
            }

            for (word_index, mut word) in q1_seen.into_iter().enumerate() {
                while word != 0 {
                    let bit = word.trailing_zeros() as usize;
                    q1_lists[word_index * 64 + bit].push(owner);
                    word &= word - 1;
                }
            }
        }
    }

    let q1 = encode_dense_fixed_index(1, block_count, q1_lists)?;
    let q2 = encode_sparse_q2_pairs(block_count, q2_pairs)?;
    Ok((q1, q2))
}

fn build_content_q1_q2_flat_fused_active_lists(
    docs: &[VNextDocumentInput],
    first_blocks: &[u16],
    block_size: u32,
    block_count: usize,
    periodic_q2_skip_from: &[u16],
) -> Result<(FixedIndexData, FixedIndexData), SearchError> {
    if block_size == 0 {
        return Err(SearchError::InvalidArgument(
            "block size must be non-zero".into(),
        ));
    }
    let block_size = usize::try_from(block_size)
        .map_err(|_| SearchError::InvalidArgument("block size too large".into()))?;

    let mut q1_lists = (0..256).map(|_| Vec::<u16>::new()).collect::<Vec<_>>();
    let mut q2_lists = (0..65_536).map(|_| Vec::<u16>::new()).collect::<Vec<_>>();
    let mut active_q2_keys = Vec::<u16>::with_capacity(256);
    let mut q2_last_owner = vec![u16::MAX; 65_536];

    for (doc_index, doc) in docs.iter().enumerate() {
        let first = usize::from(first_blocks[doc_index]);
        let bytes = &doc.normalized_content;
        for (local_block, chunk) in bytes.chunks(block_size).enumerate() {
            let owner = first
                .checked_add(local_block)
                .ok_or_else(|| SearchError::Format("fixed content block overflow".into()))?;
            if owner >= block_count {
                return Err(SearchError::Format(
                    "fixed content block out of range".into(),
                ));
            }
            let owner_index = owner;
            let owner = u16::try_from(owner_index)
                .map_err(|_| SearchError::Format("block ID exceeds u16".into()))?;
            let block_start = local_block * block_size;
            let mut q1_seen = [0u64; 4];
            let q2_process_len = periodic_q2_skip_from
                .get(owner_index)
                .copied()
                .filter(|&offset| offset != u16::MAX)
                .map_or(chunk.len(), |offset| usize::from(offset).min(chunk.len()));

            for (offset, &byte) in chunk[..q2_process_len].iter().enumerate() {
                let start = block_start + offset;
                if start + 1 < bytes.len() {
                    let q2_key = (u16::from(byte) << 8) | u16::from(bytes[start + 1]);
                    let q2_index = usize::from(q2_key);
                    if q2_last_owner[q2_index] != owner {
                        q2_last_owner[q2_index] = owner;
                        let owners = &mut q2_lists[q2_index];
                        if owners.is_empty() {
                            owners.reserve(2_048);
                            active_q2_keys.push(q2_key);
                        }
                        owners.push(owner);

                        let q1_key = usize::from(byte);
                        q1_seen[q1_key / 64] |= 1u64 << (q1_key % 64);
                    }
                }
            }
            if block_start + chunk.len() == bytes.len()
                && let Some(&last_byte) = chunk.last()
            {
                let q1_key = usize::from(last_byte);
                q1_seen[q1_key / 64] |= 1u64 << (q1_key % 64);
            }

            for (word_index, mut word) in q1_seen.into_iter().enumerate() {
                while word != 0 {
                    let bit = word.trailing_zeros() as usize;
                    q1_lists[word_index * 64 + bit].push(owner);
                    word &= word - 1;
                }
            }
        }
    }

    active_q2_keys.sort_unstable();
    let q1 = encode_dense_fixed_index(1, block_count, q1_lists)?;
    let q2 = encode_sparse_q2_active_lists(block_count, &q2_lists, &active_q2_keys)?;
    Ok((q1, q2))
}

pub(crate) fn build_path_fixed_index(
    docs: &[VNextDocumentInput],
    gram_len: usize,
) -> Result<FixedIndexData, SearchError> {
    if !matches!(gram_len, 1 | 2) {
        return Err(SearchError::InvalidArgument(
            "fixed path gram length must be 1 or 2".into(),
        ));
    }
    if gram_len == 2 {
        let mut lists = (0..65_536).map(|_| Vec::<u16>::new()).collect::<Vec<_>>();
        let mut stamps = vec![u32::MAX; 65_536];
        for (doc_index, doc) in docs.iter().enumerate() {
            let doc_id = u16::try_from(doc_index)
                .map_err(|_| SearchError::Format("doc ID exceeds u16".into()))?;
            let stamp = u32::from(doc_id);
            let mut iter = doc
                .display_path
                .bytes()
                .map(|byte| byte.to_ascii_lowercase());
            let Some(mut previous) = iter.next() else {
                continue;
            };
            for current in iter {
                let key = (usize::from(previous) << 8) | usize::from(current);
                if stamps[key] != stamp {
                    stamps[key] = stamp;
                    lists[key].push(doc_id);
                }
                previous = current;
            }
        }
        return encode_sparse_q2_lists(docs.len(), lists);
    }

    let mut lists = (0..256).map(|_| Vec::<u16>::new()).collect::<Vec<_>>();
    let mut stamps = vec![u32::MAX; 256];
    for (doc_index, doc) in docs.iter().enumerate() {
        let doc_id = u16::try_from(doc_index)
            .map_err(|_| SearchError::Format("doc ID exceeds u16".into()))?;
        let stamp = u32::from(doc_id);
        for byte in doc.display_path.bytes().map(|b| b.to_ascii_lowercase()) {
            let key = usize::from(byte);
            if stamps[key] != stamp {
                stamps[key] = stamp;
                lists[key].push(doc_id);
            }
        }
    }
    encode_dense_fixed_index(1, docs.len(), lists)
}

fn encode_sparse_q2_lists(
    universe: usize,
    lists: Vec<Vec<u16>>,
) -> Result<FixedIndexData, SearchError> {
    if lists.len() != 65_536 {
        return Err(SearchError::Format(
            "sparse q2 builder requires 65536-key domain".into(),
        ));
    }
    let key_count = lists.iter().filter(|ids| !ids.is_empty()).count();
    let entries_bytes = key_count
        .checked_mul(META_SIZE)
        .ok_or_else(|| SearchError::Format("sparse fixed metadata overflow".into()))?;
    let postings_start = HEADER_SIZE
        .checked_add(entries_bytes)
        .ok_or_else(|| SearchError::Format("sparse fixed size overflow".into()))?;
    let mut out = vec![0u8; postings_start];
    out[..8].copy_from_slice(MAGIC_SPARSE);
    put_u32(&mut out, 8, 2)?;
    put_u32(&mut out, 12, universe as u32)?;
    put_u32(&mut out, 16, key_count as u32)?;
    put_u32(&mut out, 20, META_SIZE as u32)?;
    put_u32(&mut out, 24, postings_start as u32)?;
    let bitmap_bytes = universe.div_ceil(8);
    let mut stats = FixedIndexStats::default();
    let mut entry_index = 0usize;
    for (key, ids) in lists.iter().enumerate() {
        if ids.is_empty() {
            continue;
        }
        debug_assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
        let cardinality = ids.len();
        let meta = HEADER_SIZE + entry_index * META_SIZE;
        put_u16(&mut out, meta, key as u16)?;
        put_u32(&mut out, meta + 8, cardinality as u32)?;
        stats.keys += 1;
        stats.posting_ids += cardinality as u64;
        if cardinality == 1 {
            out[meta + 2] = ENCODING_SINGLETON;
            put_u16(&mut out, meta + 4, ids[0])?;
            stats.singleton_keys += 1;
        } else {
            let raw_bytes = cardinality
                .checked_mul(2)
                .ok_or_else(|| SearchError::Format("sparse raw size overflow".into()))?;
            let posting_off = out.len() - postings_start;
            put_u32(&mut out, meta + 12, posting_off as u32)?;
            if bitmap_bytes < raw_bytes {
                out[meta + 2] = ENCODING_DENSE_BITMAP;
                let start = out.len();
                out.resize(start + bitmap_bytes, 0);
                for &id in ids {
                    let id = usize::from(id);
                    out[start + id / 8] |= 1u8 << (id % 8);
                }
                stats.dense_bitmap_keys += 1;
                stats.posting_bytes += bitmap_bytes as u64;
            } else {
                out[meta + 2] = ENCODING_RAW_U16;
                for &id in ids {
                    out.extend_from_slice(&id.to_le_bytes());
                }
                stats.raw_u16_keys += 1;
                stats.posting_bytes += raw_bytes as u64;
            }
        }
        entry_index += 1;
    }
    debug_assert!(validate_sparse_fixed_index(&out, 2, universe).is_ok());
    Ok(FixedIndexData { bytes: out, stats })
}

fn encode_sparse_q2_active_lists(
    universe: usize,
    lists: &[Vec<u16>],
    active_keys: &[u16],
) -> Result<FixedIndexData, SearchError> {
    let entries_bytes = active_keys
        .len()
        .checked_mul(META_SIZE)
        .ok_or_else(|| SearchError::Format("sparse fixed metadata overflow".into()))?;
    let postings_start = HEADER_SIZE
        .checked_add(entries_bytes)
        .ok_or_else(|| SearchError::Format("sparse fixed size overflow".into()))?;
    let mut out = vec![0u8; postings_start];
    out[..8].copy_from_slice(MAGIC_SPARSE);
    put_u32(&mut out, 8, 2)?;
    put_u32(&mut out, 12, universe as u32)?;
    put_u32(&mut out, 16, active_keys.len() as u32)?;
    put_u32(&mut out, 20, META_SIZE as u32)?;
    put_u32(&mut out, 24, postings_start as u32)?;
    let bitmap_bytes = universe.div_ceil(8);
    let mut stats = FixedIndexStats::default();

    for (entry_index, &key) in active_keys.iter().enumerate() {
        let ids = &lists[usize::from(key)];
        debug_assert!(!ids.is_empty());
        debug_assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
        let cardinality = ids.len();
        let meta = HEADER_SIZE + entry_index * META_SIZE;
        put_u16(&mut out, meta, key)?;
        put_u32(&mut out, meta + 8, cardinality as u32)?;
        stats.keys += 1;
        stats.posting_ids += cardinality as u64;
        if cardinality == 1 {
            out[meta + 2] = ENCODING_SINGLETON;
            put_u16(&mut out, meta + 4, ids[0])?;
            stats.singleton_keys += 1;
        } else {
            let raw_bytes = cardinality
                .checked_mul(2)
                .ok_or_else(|| SearchError::Format("sparse raw size overflow".into()))?;
            let posting_off = out.len() - postings_start;
            put_u32(&mut out, meta + 12, posting_off as u32)?;
            if bitmap_bytes < raw_bytes {
                out[meta + 2] = ENCODING_DENSE_BITMAP;
                let start = out.len();
                out.resize(start + bitmap_bytes, 0);
                for &id in ids {
                    let id = usize::from(id);
                    if id >= universe {
                        return Err(SearchError::Format(
                            "sparse q2 posting ID out of range".into(),
                        ));
                    }
                    out[start + id / 8] |= 1u8 << (id % 8);
                }
                stats.dense_bitmap_keys += 1;
                stats.posting_bytes += bitmap_bytes as u64;
            } else {
                out[meta + 2] = ENCODING_RAW_U16;
                for &id in ids {
                    if usize::from(id) >= universe {
                        return Err(SearchError::Format(
                            "sparse q2 posting ID out of range".into(),
                        ));
                    }
                    out.extend_from_slice(&id.to_le_bytes());
                }
                stats.raw_u16_keys += 1;
                stats.posting_bytes += raw_bytes as u64;
            }
        }
    }
    debug_assert!(validate_sparse_fixed_index(&out, 2, universe).is_ok());
    Ok(FixedIndexData { bytes: out, stats })
}

struct SparseQ2Encoder {
    out: Vec<u8>,
    postings_start: usize,
    bitmap_bytes: usize,
    universe: usize,
    stats: FixedIndexStats,
    entry_index: usize,
}

impl SparseQ2Encoder {
    fn new(universe: usize, key_count: usize) -> Result<Self, SearchError> {
        let entries_bytes = key_count
            .checked_mul(META_SIZE)
            .ok_or_else(|| SearchError::Format("sparse fixed metadata overflow".into()))?;
        let postings_start = HEADER_SIZE
            .checked_add(entries_bytes)
            .ok_or_else(|| SearchError::Format("sparse fixed size overflow".into()))?;
        let mut out = vec![0u8; postings_start];
        out[..8].copy_from_slice(MAGIC_SPARSE);
        put_u32(&mut out, 8, 2)?;
        put_u32(&mut out, 12, universe as u32)?;
        put_u32(&mut out, 16, key_count as u32)?;
        put_u32(&mut out, 20, META_SIZE as u32)?;
        put_u32(&mut out, 24, postings_start as u32)?;
        Ok(Self {
            out,
            postings_start,
            bitmap_bytes: universe.div_ceil(8),
            universe,
            stats: FixedIndexStats::default(),
            entry_index: 0,
        })
    }

    fn push_group(&mut self, key: u16, ids: &[u32]) -> Result<(), SearchError> {
        if ids.is_empty() {
            return Ok(());
        }
        debug_assert!(
            ids.windows(2)
                .all(|pair| (pair[0] as u16) < (pair[1] as u16))
        );
        let cardinality = ids.len();
        let meta = HEADER_SIZE + self.entry_index * META_SIZE;
        put_u16(&mut self.out, meta, key)?;
        put_u32(&mut self.out, meta + 8, cardinality as u32)?;
        self.stats.keys += 1;
        self.stats.posting_ids += cardinality as u64;
        if cardinality == 1 {
            self.out[meta + 2] = ENCODING_SINGLETON;
            put_u16(&mut self.out, meta + 4, ids[0] as u16)?;
            self.stats.singleton_keys += 1;
        } else {
            let raw_bytes = cardinality
                .checked_mul(2)
                .ok_or_else(|| SearchError::Format("sparse raw size overflow".into()))?;
            let posting_off = self.out.len() - self.postings_start;
            put_u32(&mut self.out, meta + 12, posting_off as u32)?;
            if self.bitmap_bytes < raw_bytes {
                self.out[meta + 2] = ENCODING_DENSE_BITMAP;
                let start = self.out.len();
                self.out.resize(start + self.bitmap_bytes, 0);
                for packed in ids {
                    let id = (*packed as u16) as usize;
                    if id >= self.universe {
                        return Err(SearchError::Format(
                            "sparse q2 posting ID out of range".into(),
                        ));
                    }
                    self.out[start + id / 8] |= 1u8 << (id % 8);
                }
                self.stats.dense_bitmap_keys += 1;
                self.stats.posting_bytes += self.bitmap_bytes as u64;
            } else {
                self.out[meta + 2] = ENCODING_RAW_U16;
                for packed in ids {
                    let id = *packed as u16;
                    if usize::from(id) >= self.universe {
                        return Err(SearchError::Format(
                            "sparse q2 posting ID out of range".into(),
                        ));
                    }
                    self.out.extend_from_slice(&id.to_le_bytes());
                }
                self.stats.raw_u16_keys += 1;
                self.stats.posting_bytes += raw_bytes as u64;
            }
        }
        self.entry_index += 1;
        Ok(())
    }

    fn finish(self) -> Result<FixedIndexData, SearchError> {
        debug_assert!(validate_sparse_fixed_index(&self.out, 2, self.universe).is_ok());
        Ok(FixedIndexData {
            bytes: self.out,
            stats: self.stats,
        })
    }
}

// q2 pairs are emitted in document/block order, so the packed low16 owner ID is already
// monotonically non-decreasing. A stable sort on the upper16 q2 key therefore produces the same
// (key, owner) order as the old low16+upper16 LSD radix. The q2 builder also emits each key at
// most once per owner, bounding the total pair count below u32::MAX for the u16 owner universe.
fn radix_sort_q2_monotonic_owner_upper16(values: &mut Vec<u32>) -> usize {
    if values.is_empty() {
        return 0;
    }
    if values.len() == 1 {
        return 1;
    }
    debug_assert!(
        values
            .windows(2)
            .all(|pair| (pair[0] as u16) <= (pair[1] as u16))
    );
    let mut scratch = vec![0u32; values.len()];
    let mut counts = vec![0u32; 65_536];
    for &value in values.iter() {
        counts[(value >> 16) as usize] += 1;
    }
    let mut sum = 0u32;
    let mut key_count = 0usize;
    for count in &mut counts {
        let current = *count;
        key_count += usize::from(current != 0);
        *count = sum;
        sum = sum
            .checked_add(current)
            .expect("q2 pair count is bounded below u32::MAX");
    }
    for &value in values.iter() {
        let bucket = (value >> 16) as usize;
        scratch[counts[bucket] as usize] = value;
        counts[bucket] += 1;
    }
    std::mem::swap(values, &mut scratch);
    key_count
}

fn encode_sparse_q2_pairs(
    universe: usize,
    mut pairs: Vec<u32>,
) -> Result<FixedIndexData, SearchError> {
    // build_content_q2_sparse already deduplicates every q2 key within each owner block.
    // Owner IDs are globally unique and monotonic, so duplicate packed (key, owner) pairs cannot
    // exist. Let the radix histogram return the number of populated keys and avoid two additional
    // full-vector passes (global dedup + key recount) before encoding.
    let key_count = radix_sort_q2_monotonic_owner_upper16(&mut pairs);
    debug_assert!(pairs.windows(2).all(|pair| pair[0] < pair[1]));
    encode_sparse_q2_sorted_pairs(universe, pairs, key_count)
}

fn encode_sparse_q2_sorted_pairs(
    universe: usize,
    pairs: Vec<u32>,
    key_count: usize,
) -> Result<FixedIndexData, SearchError> {
    debug_assert!(pairs.windows(2).all(|pair| pair[0] < pair[1]));
    let entries_bytes = key_count
        .checked_mul(META_SIZE)
        .ok_or_else(|| SearchError::Format("sparse fixed metadata overflow".into()))?;
    let postings_start = HEADER_SIZE
        .checked_add(entries_bytes)
        .ok_or_else(|| SearchError::Format("sparse fixed size overflow".into()))?;
    let mut out = vec![0u8; postings_start];
    out[..8].copy_from_slice(MAGIC_SPARSE);
    put_u32(&mut out, 8, 2)?;
    put_u32(&mut out, 12, universe as u32)?;
    put_u32(&mut out, 16, key_count as u32)?;
    put_u32(&mut out, 20, META_SIZE as u32)?;
    put_u32(&mut out, 24, postings_start as u32)?;
    let bitmap_bytes = universe.div_ceil(8);
    let mut stats = FixedIndexStats::default();
    let mut position = 0usize;
    let mut entry_index = 0usize;
    while position < pairs.len() {
        let key = (pairs[position] >> 16) as u16;
        let begin = position;
        while position < pairs.len() && (pairs[position] >> 16) as u16 == key {
            position += 1;
        }
        let ids = &pairs[begin..position];
        let cardinality = ids.len();
        let meta = HEADER_SIZE + entry_index * META_SIZE;
        put_u16(&mut out, meta, key)?;
        put_u32(&mut out, meta + 8, cardinality as u32)?;
        stats.keys += 1;
        stats.posting_ids += cardinality as u64;
        if cardinality == 1 {
            out[meta + 2] = ENCODING_SINGLETON;
            put_u16(&mut out, meta + 4, ids[0] as u16)?;
            stats.singleton_keys += 1;
        } else {
            let raw_bytes = cardinality
                .checked_mul(2)
                .ok_or_else(|| SearchError::Format("sparse raw size overflow".into()))?;
            let posting_off = out.len() - postings_start;
            put_u32(&mut out, meta + 12, posting_off as u32)?;
            if bitmap_bytes < raw_bytes {
                out[meta + 2] = ENCODING_DENSE_BITMAP;
                let start = out.len();
                out.resize(start + bitmap_bytes, 0);
                for packed in ids {
                    let id = (*packed as u16) as usize;
                    if id >= universe {
                        return Err(SearchError::Format(
                            "sparse q2 posting ID out of range".into(),
                        ));
                    }
                    out[start + id / 8] |= 1u8 << (id % 8);
                }
                stats.dense_bitmap_keys += 1;
                stats.posting_bytes += bitmap_bytes as u64;
            } else {
                out[meta + 2] = ENCODING_RAW_U16;
                for packed in ids {
                    let id = *packed as u16;
                    if usize::from(id) >= universe {
                        return Err(SearchError::Format(
                            "sparse q2 posting ID out of range".into(),
                        ));
                    }
                    out.extend_from_slice(&id.to_le_bytes());
                }
                stats.raw_u16_keys += 1;
                stats.posting_bytes += raw_bytes as u64;
            }
        }
        entry_index += 1;
    }
    debug_assert!(validate_sparse_fixed_index(&out, 2, universe).is_ok());
    Ok(FixedIndexData { bytes: out, stats })
}

// Content q2 uses an 8+8 key decomposition. The high byte selects one of 256 shards; within a
// shard, owner IDs are still emitted monotonically and only the low key byte needs a stable
// counting pass. The 256-entry u32 histogram is 1 KiB and stays in L1, replacing the previous
// 65,536-entry / 256 KiB histogram while preserving exact full-key order after shard concatenation.
fn order_q2_shard_low8<'a>(
    values: &[u32],
    scratch: &'a mut Vec<u32>,
    counts: &mut [u32; 256],
) -> (&'a [u32], usize) {
    if values.is_empty() {
        scratch.clear();
        return (scratch, 0);
    }
    debug_assert!(
        values
            .windows(2)
            .all(|pair| (pair[0] as u16) <= (pair[1] as u16))
    );
    counts.fill(0);
    for &value in values {
        counts[((value >> 16) as u8) as usize] += 1;
    }
    let mut sum = 0u32;
    let mut key_count = 0usize;
    for count in counts.iter_mut() {
        let current = *count;
        key_count += usize::from(current != 0);
        *count = sum;
        sum = sum
            .checked_add(current)
            .expect("q2 shard pair count is bounded below u32::MAX");
    }
    scratch.clear();
    scratch.resize(values.len(), 0);
    for &value in values {
        let bucket = ((value >> 16) as u8) as usize;
        scratch[counts[bucket] as usize] = value;
        counts[bucket] += 1;
    }
    (scratch, key_count)
}

fn encode_sparse_q2_shards(
    universe: usize,
    shards: Vec<Vec<u32>>,
    key_count: usize,
) -> Result<FixedIndexData, SearchError> {
    if shards.len() != 256 {
        return Err(SearchError::Format(
            "sharded q2 builder requires 256 high-byte shards".into(),
        ));
    }
    let mut scratch = Vec::<u32>::new();
    let mut counts = [0u32; 256];
    let mut observed_key_count = 0usize;
    let mut encoder = SparseQ2Encoder::new(universe, key_count)?;
    for (shard_id, shard) in shards.iter().enumerate() {
        let (ordered, shard_key_count) = order_q2_shard_low8(shard, &mut scratch, &mut counts);
        observed_key_count = observed_key_count
            .checked_add(shard_key_count)
            .ok_or_else(|| SearchError::Format("q2 shard key count overflow".into()))?;
        debug_assert!(ordered.windows(2).all(|pair| pair[0] != pair[1]));
        let mut position = 0usize;
        while position < ordered.len() {
            let key = (ordered[position] >> 16) as u16;
            debug_assert_eq!(usize::from((key >> 8) as u8), shard_id);
            let begin = position;
            while position < ordered.len() && (ordered[position] >> 16) as u16 == key {
                position += 1;
            }
            encoder.push_group(key, &ordered[begin..position])?;
        }
    }
    if observed_key_count != key_count {
        return Err(SearchError::Format(
            "q2 sharded key count mismatch while encoding".into(),
        ));
    }
    encoder.finish()
}

fn encode_dense_fixed_index(
    gram_len: usize,
    universe: usize,
    lists: Vec<Vec<u16>>,
) -> Result<FixedIndexData, SearchError> {
    let domain = lists.len();
    let meta_bytes = domain
        .checked_mul(META_SIZE)
        .ok_or_else(|| SearchError::Format("fixed index metadata overflow".into()))?;
    let mut out = vec![0u8; HEADER_SIZE + meta_bytes];
    out[..8].copy_from_slice(MAGIC_DENSE);
    put_u32(&mut out, 8, gram_len as u32)?;
    put_u32(&mut out, 12, universe as u32)?;
    put_u32(&mut out, 16, domain as u32)?;
    put_u32(&mut out, 20, META_SIZE as u32)?;
    let bitmap_bytes = universe.div_ceil(8);
    let mut stats = FixedIndexStats::default();
    for (key, ids) in lists.iter().enumerate() {
        if ids.is_empty() {
            continue;
        }
        stats.keys += 1;
        stats.posting_ids += ids.len() as u64;
        let meta = HEADER_SIZE + key * META_SIZE;
        put_u32(&mut out, meta + 4, ids.len() as u32)?;
        if ids.len() == 1 {
            out[meta] = ENCODING_SINGLETON;
            put_u16(&mut out, meta + 2, ids[0])?;
            stats.singleton_keys += 1;
            continue;
        }
        let raw_bytes = ids
            .len()
            .checked_mul(2)
            .ok_or_else(|| SearchError::Format("fixed posting size overflow".into()))?;
        let posting_off = out.len() - (HEADER_SIZE + meta_bytes);
        if bitmap_bytes < raw_bytes {
            out[meta] = ENCODING_DENSE_BITMAP;
            put_u32(&mut out, meta + 8, posting_off as u32)?;
            put_u32(&mut out, meta + 12, bitmap_bytes as u32)?;
            let start = out.len();
            out.resize(start + bitmap_bytes, 0);
            for &id in ids {
                out[start + usize::from(id) / 8] |= 1u8 << (usize::from(id) % 8);
            }
            stats.dense_bitmap_keys += 1;
            stats.posting_bytes += bitmap_bytes as u64;
        } else {
            out[meta] = ENCODING_RAW_U16;
            put_u32(&mut out, meta + 8, posting_off as u32)?;
            put_u32(&mut out, meta + 12, raw_bytes as u32)?;
            for &id in ids {
                out.extend_from_slice(&id.to_le_bytes());
            }
            stats.raw_u16_keys += 1;
            stats.posting_bytes += raw_bytes as u64;
        }
    }
    debug_assert!(validate_dense_fixed_index(&out, gram_len, universe).is_ok());
    Ok(FixedIndexData { bytes: out, stats })
}

pub(crate) fn lookup_fixed_index<'a>(
    bytes: &'a [u8],
    gram: &[u8],
    universe: usize,
) -> Result<FixedPosting<'a>, SearchError> {
    match bytes.get(..8) {
        Some(magic) if magic == MAGIC_DENSE => lookup_dense_fixed_index(bytes, gram, universe),
        Some(magic) if magic == MAGIC_SPARSE => lookup_sparse_fixed_index(bytes, gram, universe),
        _ => Err(SearchError::Format("bad fixed index header".into())),
    }
}

pub(crate) fn validate_fixed_index(
    bytes: &[u8],
    gram_len: usize,
    universe: usize,
) -> Result<FixedIndexStats, SearchError> {
    match bytes.get(..8) {
        Some(magic) if magic == MAGIC_DENSE => {
            validate_dense_fixed_index(bytes, gram_len, universe)
        }
        Some(magic) if magic == MAGIC_SPARSE => {
            validate_sparse_fixed_index(bytes, gram_len, universe)
        }
        _ => Err(SearchError::Format("bad fixed index header".into())),
    }
}

fn lookup_dense_fixed_index<'a>(
    bytes: &'a [u8],
    gram: &[u8],
    universe: usize,
) -> Result<FixedPosting<'a>, SearchError> {
    validate_dense_header(bytes, gram.len(), universe)?;
    let key = gram_key(gram);
    let domain = rd_u32(bytes, 16)? as usize;
    if key >= domain {
        return Ok(empty_posting(universe));
    }
    decode_dense_posting(bytes, HEADER_SIZE + key * META_SIZE, universe)
}

fn validate_dense_fixed_index(
    bytes: &[u8],
    gram_len: usize,
    universe: usize,
) -> Result<FixedIndexStats, SearchError> {
    validate_dense_header(bytes, gram_len, universe)?;
    let domain = rd_u32(bytes, 16)? as usize;
    let postings_start = HEADER_SIZE + domain * META_SIZE;
    let mut stats = FixedIndexStats::default();
    for key in 0..domain {
        let posting = decode_dense_posting(bytes, HEADER_SIZE + key * META_SIZE, universe)?;
        accumulate_validated_posting(&posting, universe, &mut stats)?;
    }
    if postings_start > bytes.len() {
        return Err(SearchError::Format(
            "fixed postings start out of bounds".into(),
        ));
    }
    Ok(stats)
}

fn validate_dense_header(
    bytes: &[u8],
    gram_len: usize,
    universe: usize,
) -> Result<(), SearchError> {
    if bytes.len() < HEADER_SIZE || bytes.get(..8) != Some(MAGIC_DENSE.as_slice()) {
        return Err(SearchError::Format("bad fixed index header".into()));
    }
    if rd_u32(bytes, 8)? as usize != gram_len || rd_u32(bytes, 12)? as usize != universe {
        return Err(SearchError::Format("fixed index shape mismatch".into()));
    }
    let expected_domain = 1usize << (gram_len * 8);
    if rd_u32(bytes, 16)? as usize != expected_domain || rd_u32(bytes, 20)? as usize != META_SIZE {
        return Err(SearchError::Format("bad fixed index metadata".into()));
    }
    let min = HEADER_SIZE
        .checked_add(expected_domain * META_SIZE)
        .ok_or_else(|| SearchError::Format("fixed index size overflow".into()))?;
    if bytes.len() < min {
        return Err(SearchError::Format("fixed index truncated".into()));
    }
    Ok(())
}

fn lookup_sparse_fixed_index<'a>(
    bytes: &'a [u8],
    gram: &[u8],
    universe: usize,
) -> Result<FixedPosting<'a>, SearchError> {
    validate_sparse_header(bytes, gram.len(), universe)?;
    if gram.len() != 2 {
        return Err(SearchError::Format("sparse fixed index is q2-only".into()));
    }
    let target = gram_key(gram) as u16;
    let key_count = rd_u32(bytes, 16)? as usize;
    let postings_start = rd_u32(bytes, 24)? as usize;
    let mut low = 0usize;
    let mut high = key_count;
    while low < high {
        let mid = low + (high - low) / 2;
        let meta = HEADER_SIZE + mid * META_SIZE;
        let key = rd_u16(bytes, meta)?;
        match key.cmp(&target) {
            std::cmp::Ordering::Less => low = mid + 1,
            std::cmp::Ordering::Greater => high = mid,
            std::cmp::Ordering::Equal => {
                return decode_sparse_posting(bytes, meta, postings_start, universe);
            }
        }
    }
    Ok(empty_posting(universe))
}

fn validate_sparse_fixed_index(
    bytes: &[u8],
    gram_len: usize,
    universe: usize,
) -> Result<FixedIndexStats, SearchError> {
    validate_sparse_header(bytes, gram_len, universe)?;
    if gram_len != 2 {
        return Err(SearchError::Format("sparse fixed index is q2-only".into()));
    }
    let key_count = rd_u32(bytes, 16)? as usize;
    let postings_start = rd_u32(bytes, 24)? as usize;
    let mut expected_posting_off = 0usize;
    let mut previous_key = None;
    let mut stats = FixedIndexStats::default();
    for index in 0..key_count {
        let meta = HEADER_SIZE + index * META_SIZE;
        let key = rd_u16(bytes, meta)?;
        if previous_key.is_some_and(|previous| key <= previous) {
            return Err(SearchError::Format(
                "sparse fixed keys are not strictly sorted".into(),
            ));
        }
        if bytes.get(meta + 3).copied() != Some(0) || rd_u16(bytes, meta + 6)? != 0 {
            return Err(SearchError::Format(
                "sparse fixed reserved metadata is nonzero".into(),
            ));
        }
        let posting = decode_sparse_posting(bytes, meta, postings_start, universe)?;
        let posting_off = rd_u32(bytes, meta + 12)? as usize;
        match posting.encoding() {
            FixedPostingEncoding::Singleton => {
                if posting_off != 0 {
                    return Err(SearchError::Format("bad sparse singleton offset".into()));
                }
            }
            FixedPostingEncoding::RawU16 | FixedPostingEncoding::DenseBitmap => {
                if posting_off != expected_posting_off {
                    return Err(SearchError::Format(
                        "sparse fixed posting ranges are not contiguous".into(),
                    ));
                }
                expected_posting_off = expected_posting_off
                    .checked_add(posting.bytes.len())
                    .ok_or_else(|| SearchError::Format("sparse posting size overflow".into()))?;
            }
            FixedPostingEncoding::Empty => {
                return Err(SearchError::Format(
                    "sparse fixed entry cannot encode empty posting".into(),
                ));
            }
        }
        accumulate_validated_posting(&posting, universe, &mut stats)?;
        previous_key = Some(key);
    }
    if postings_start.checked_add(expected_posting_off) != Some(bytes.len()) {
        return Err(SearchError::Format(
            "sparse fixed posting coverage mismatch".into(),
        ));
    }
    Ok(stats)
}

fn validate_sparse_header(
    bytes: &[u8],
    gram_len: usize,
    universe: usize,
) -> Result<(), SearchError> {
    if bytes.len() < HEADER_SIZE || bytes.get(..8) != Some(MAGIC_SPARSE.as_slice()) {
        return Err(SearchError::Format("bad sparse fixed index header".into()));
    }
    if rd_u32(bytes, 8)? as usize != gram_len || rd_u32(bytes, 12)? as usize != universe {
        return Err(SearchError::Format(
            "sparse fixed index shape mismatch".into(),
        ));
    }
    let key_count = rd_u32(bytes, 16)? as usize;
    if key_count > 65_536 || rd_u32(bytes, 20)? as usize != META_SIZE {
        return Err(SearchError::Format("bad sparse fixed metadata".into()));
    }
    let expected_postings_start = HEADER_SIZE
        .checked_add(key_count * META_SIZE)
        .ok_or_else(|| SearchError::Format("sparse fixed size overflow".into()))?;
    if rd_u32(bytes, 24)? as usize != expected_postings_start
        || bytes.len() < expected_postings_start
    {
        return Err(SearchError::Format("sparse fixed index truncated".into()));
    }
    Ok(())
}

fn decode_dense_posting<'a>(
    bytes: &'a [u8],
    meta: usize,
    universe: usize,
) -> Result<FixedPosting<'a>, SearchError> {
    let encoding = *bytes
        .get(meta)
        .ok_or_else(|| SearchError::Format("fixed metadata out of bounds".into()))?;
    let singleton = rd_u16(bytes, meta + 2)?;
    let cardinality = rd_u32(bytes, meta + 4)? as usize;
    let posting_off = rd_u32(bytes, meta + 8)? as usize;
    let posting_len = rd_u32(bytes, meta + 12)? as usize;
    let domain = rd_u32(bytes, 16)? as usize;
    let postings_start = HEADER_SIZE + domain * META_SIZE;
    decode_posting_common(
        bytes,
        encoding,
        singleton,
        cardinality,
        posting_off,
        Some(posting_len),
        postings_start,
        universe,
        true,
    )
}

fn decode_sparse_posting<'a>(
    bytes: &'a [u8],
    meta: usize,
    postings_start: usize,
    universe: usize,
) -> Result<FixedPosting<'a>, SearchError> {
    let encoding = *bytes
        .get(meta + 2)
        .ok_or_else(|| SearchError::Format("sparse metadata out of bounds".into()))?;
    let singleton = rd_u16(bytes, meta + 4)?;
    let cardinality = rd_u32(bytes, meta + 8)? as usize;
    let posting_off = rd_u32(bytes, meta + 12)? as usize;
    decode_posting_common(
        bytes,
        encoding,
        singleton,
        cardinality,
        posting_off,
        None,
        postings_start,
        universe,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn decode_posting_common<'a>(
    bytes: &'a [u8],
    encoding: u8,
    singleton: u16,
    cardinality: usize,
    posting_off: usize,
    explicit_len: Option<usize>,
    postings_start: usize,
    universe: usize,
    dense_layout: bool,
) -> Result<FixedPosting<'a>, SearchError> {
    let make = |encoding, bytes| FixedPosting {
        encoding,
        cardinality,
        singleton,
        bytes,
        universe,
    };
    match encoding {
        ENCODING_EMPTY if dense_layout => {
            if cardinality != 0 || singleton != 0 || posting_off != 0 || explicit_len != Some(0) {
                return Err(SearchError::Format("nonzero empty fixed posting".into()));
            }
            Ok(empty_posting(universe))
        }
        ENCODING_SINGLETON => {
            if cardinality != 1
                || usize::from(singleton) >= universe
                || posting_off != 0
                || explicit_len.is_some_and(|len| len != 0)
            {
                return Err(SearchError::Format("bad singleton fixed posting".into()));
            }
            Ok(make(FixedPostingEncoding::Singleton, &[]))
        }
        ENCODING_RAW_U16 => {
            let expected = cardinality
                .checked_mul(2)
                .ok_or_else(|| SearchError::Format("fixed raw size overflow".into()))?;
            if cardinality < 2 || explicit_len.is_some_and(|len| len != expected) {
                return Err(SearchError::Format("bad raw fixed posting".into()));
            }
            let start = postings_start
                .checked_add(posting_off)
                .ok_or_else(|| SearchError::Format("fixed posting offset overflow".into()))?;
            let end = start
                .checked_add(expected)
                .ok_or_else(|| SearchError::Format("fixed posting end overflow".into()))?;
            let data = bytes
                .get(start..end)
                .ok_or_else(|| SearchError::Format("fixed posting out of bounds".into()))?;
            Ok(make(FixedPostingEncoding::RawU16, data))
        }
        ENCODING_DENSE_BITMAP => {
            let expected = universe.div_ceil(8);
            if cardinality < 2
                || explicit_len.is_some_and(|len| len != expected)
                || cardinality > universe
            {
                return Err(SearchError::Format("bad bitmap fixed posting".into()));
            }
            let start = postings_start
                .checked_add(posting_off)
                .ok_or_else(|| SearchError::Format("fixed posting offset overflow".into()))?;
            let end = start
                .checked_add(expected)
                .ok_or_else(|| SearchError::Format("fixed posting end overflow".into()))?;
            let data = bytes
                .get(start..end)
                .ok_or_else(|| SearchError::Format("fixed bitmap out of bounds".into()))?;
            if !universe.is_multiple_of(8) && !data.is_empty() {
                let valid = universe % 8;
                let mask = !((1u8 << valid) - 1);
                if data[data.len() - 1] & mask != 0 {
                    return Err(SearchError::Format(
                        "fixed bitmap has out-of-range bits".into(),
                    ));
                }
            }
            Ok(make(FixedPostingEncoding::DenseBitmap, data))
        }
        _ => Err(SearchError::Format("unknown fixed posting encoding".into())),
    }
}

fn accumulate_validated_posting(
    posting: &FixedPosting<'_>,
    universe: usize,
    stats: &mut FixedIndexStats,
) -> Result<(), SearchError> {
    if posting.is_empty() {
        return Ok(());
    }
    stats.keys += 1;
    stats.posting_ids += posting.len() as u64;
    let mut prev = None;
    let mut count = 0usize;
    for id in posting.iter() {
        if usize::from(id) >= universe {
            return Err(SearchError::Format("fixed posting ID out of range".into()));
        }
        if prev.is_some_and(|p| id <= p) {
            return Err(SearchError::Format(
                "fixed posting IDs not strictly sorted".into(),
            ));
        }
        prev = Some(id);
        count += 1;
    }
    if count != posting.len() {
        return Err(SearchError::Format(
            "fixed posting cardinality mismatch".into(),
        ));
    }
    match posting.encoding() {
        FixedPostingEncoding::Singleton => stats.singleton_keys += 1,
        FixedPostingEncoding::RawU16 => {
            stats.raw_u16_keys += 1;
            stats.posting_bytes += posting.bytes.len() as u64;
        }
        FixedPostingEncoding::DenseBitmap => {
            stats.dense_bitmap_keys += 1;
            stats.posting_bytes += posting.bytes.len() as u64;
        }
        FixedPostingEncoding::Empty => {}
    }
    Ok(())
}

const fn empty_posting(universe: usize) -> FixedPosting<'static> {
    FixedPosting {
        encoding: FixedPostingEncoding::Empty,
        cardinality: 0,
        singleton: 0,
        bytes: &[],
        universe,
    }
}

fn gram_key(gram: &[u8]) -> usize {
    match gram {
        [a] => usize::from(*a),
        [a, b] => (usize::from(*a) << 8) | usize::from(*b),
        _ => usize::MAX,
    }
}
fn rd_u16(bytes: &[u8], off: usize) -> Result<u16, SearchError> {
    let d = bytes
        .get(off..off + 2)
        .ok_or_else(|| SearchError::Format("fixed u16 OOB".into()))?;
    Ok(u16::from_le_bytes([d[0], d[1]]))
}
fn rd_u32(bytes: &[u8], off: usize) -> Result<u32, SearchError> {
    let d = bytes
        .get(off..off + 4)
        .ok_or_else(|| SearchError::Format("fixed u32 OOB".into()))?;
    Ok(u32::from_le_bytes(d.try_into().expect("u32 slice")))
}
fn put_u16(bytes: &mut [u8], off: usize, value: u16) -> Result<(), SearchError> {
    bytes
        .get_mut(off..off + 2)
        .ok_or_else(|| SearchError::Format("fixed u16 write OOB".into()))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}
fn put_u32(bytes: &mut [u8], off: usize, value: u32) -> Result<(), SearchError> {
    bytes
        .get_mut(off..off + 4)
        .ok_or_else(|| SearchError::Format("fixed u32 write OOB".into()))?
        .copy_from_slice(&value.to_le_bytes());
    Ok(())
}

#[cfg(test)]
mod monotonic_q2_radix_tests {
    use super::*;
    use crate::vnext_q3::build_q3_index_with_q2_projection;

    fn build_content_q1_reference(
        docs: &[VNextDocumentInput],
        first_blocks: &[u16],
        block_size: u32,
        block_count: usize,
    ) -> Result<FixedIndexData, SearchError> {
        let mut lists = (0..256).map(|_| Vec::<u16>::new()).collect::<Vec<_>>();
        let mut stamps = vec![u32::MAX; 256];
        let block_size = usize::try_from(block_size)
            .map_err(|_| SearchError::InvalidArgument("block size too large".into()))?;
        for (doc_index, doc) in docs.iter().enumerate() {
            let first = usize::from(first_blocks[doc_index]);
            for start in 0..doc.normalized_content.len() {
                let owner = first
                    .checked_add(start / block_size)
                    .ok_or_else(|| SearchError::Format("fixed content block overflow".into()))?;
                if owner >= block_count {
                    return Err(SearchError::Format(
                        "fixed content block out of range".into(),
                    ));
                }
                let key = usize::from(doc.normalized_content[start]);
                let stamp = u32::try_from(owner)
                    .map_err(|_| SearchError::Format("block ID exceeds u32".into()))?;
                if stamps[key] != stamp {
                    stamps[key] = stamp;
                    lists[key].push(
                        u16::try_from(owner)
                            .map_err(|_| SearchError::Format("block ID exceeds u16".into()))?,
                    );
                }
            }
        }
        encode_dense_fixed_index(1, block_count, lists)
    }

    #[test]
    fn q3_projection_reconstructs_exact_q1_q2_with_tails_short_docs_and_boundaries() {
        let block_size = 8_192u32;
        let mut docs = Vec::new();
        for doc_id in 0..6u64 {
            let mut content = format!("header-{doc_id}: ").into_bytes();
            while content.len() < 10_000 {
                content.extend_from_slice(b"abcdefghijklmnopqrstuvwxyz");
            }
            content.truncate(10_000);
            docs.push(VNextDocumentInput::new(
                doc_id + 1,
                format!("periodic-{doc_id}.txt"),
                content,
            ));
        }
        docs.push(VNextDocumentInput::new(100, "one.bin", vec![0xa5]));
        docs.push(VNextDocumentInput::new(101, "two.bin", vec![0x12, 0x34]));

        let mut first_blocks = Vec::with_capacity(docs.len());
        let mut block_count = 0usize;
        for doc in &docs {
            first_blocks.push(block_count as u16);
            block_count += doc.normalized_content.len().div_ceil(block_size as usize);
        }

        let mut q3 =
            build_q3_index_with_q2_projection(&docs, &first_blocks, block_size, block_count, true)
                .unwrap();
        let projected_pairs = q3
            .projected_q2
            .take()
            .expect("periodic coverage should enable post-q3-dedup q2 projection");
        assert!(projected_pairs.iter().any(|chunk| !chunk.is_empty()));

        let (projected_q1, projected_q2) = build_content_q1_q2_from_q3_projection(
            &docs,
            &first_blocks,
            block_size,
            block_count,
            projected_pairs,
        )
        .unwrap();
        let reference_q1 =
            build_content_fixed_index(&docs, &first_blocks, block_size, block_count, 1).unwrap();
        let reference_q2 =
            build_content_fixed_index(&docs, &first_blocks, block_size, block_count, 2).unwrap();

        assert_eq!(projected_q1.bytes, reference_q1.bytes);
        assert_eq!(projected_q1.stats, reference_q1.stats);
        assert_eq!(projected_q2.bytes, reference_q2.bytes);
        assert_eq!(projected_q2.stats, reference_q2.stats);
    }

    #[test]
    fn block_local_q1_bitset_is_byte_identical_to_stamp_reference() {
        let block_size = 64u32;
        let mut docs = Vec::new();
        let mut first_blocks = Vec::new();
        let mut next_block = 0u16;
        let mut state = 0x243f_6a88_85a3_08d3u64;
        for doc_id in 0..96u64 {
            let len = 31 + (doc_id as usize * 37 % 611);
            let mut content = Vec::with_capacity(len);
            for index in 0..len {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let byte = if index % 19 == 0 {
                    (doc_id as u8).wrapping_mul(17)
                } else {
                    (state >> 24) as u8
                };
                content.push(byte);
            }
            first_blocks.push(next_block);
            next_block = next_block
                .checked_add(content.len().div_ceil(block_size as usize) as u16)
                .unwrap();
            docs.push(VNextDocumentInput::new(
                doc_id + 1,
                format!("q1-bitset-{doc_id}.bin"),
                content,
            ));
        }

        let optimized =
            build_content_q1_dense(&docs, &first_blocks, block_size, usize::from(next_block))
                .unwrap();
        let reference =
            build_content_q1_reference(&docs, &first_blocks, block_size, usize::from(next_block))
                .unwrap();
        assert_eq!(optimized.bytes, reference.bytes);
        assert_eq!(optimized.stats, reference.stats);
    }

    #[test]
    fn fused_q1_q2_flat_is_byte_identical_to_separate_builders() {
        let block_size = 64u32;
        let mut docs = Vec::new();
        let mut first_blocks = Vec::new();
        let mut next_block = 0u16;
        let mut state = 0x1319_8a2e_0370_7344u64;
        for doc_id in 0..128u64 {
            let len = 1 + (doc_id as usize * 53 % 777);
            let mut content = Vec::with_capacity(len);
            for index in 0..len {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let byte = if index % 11 == 0 {
                    (doc_id as u8).wrapping_add(index as u8)
                } else {
                    (state >> 32) as u8
                };
                content.push(byte);
            }
            first_blocks.push(next_block);
            next_block = next_block
                .checked_add(content.len().div_ceil(block_size as usize) as u16)
                .unwrap();
            docs.push(VNextDocumentInput::new(
                doc_id + 1,
                format!("fused-q12-{doc_id}.bin"),
                content,
            ));
        }
        let universe = usize::from(next_block);
        let (fused_q1, fused_q2) =
            build_content_q1_q2_flat_fused(&docs, &first_blocks, block_size, universe, None)
                .unwrap();
        let separate_q1 =
            build_content_q1_dense(&docs, &first_blocks, block_size, universe).unwrap();
        let separate_q2 =
            build_content_q2_sparse_flat(&docs, &first_blocks, block_size, universe).unwrap();
        assert_eq!(fused_q1.bytes, separate_q1.bytes);
        assert_eq!(fused_q1.stats, separate_q1.stats);
        assert_eq!(fused_q2.bytes, separate_q2.bytes);
        assert_eq!(fused_q2.stats, separate_q2.stats);
    }

    #[test]
    fn q3_periodic_proof_reuse_preserves_q1_q2_bytes() {
        let block_size = 8_192u32;
        let mut content = b"header: periodic q2 reuse\n".to_vec();
        while content.len() < 7_500 {
            content.extend_from_slice(b"abcdefghijklmnopqrstuvwxyz");
        }
        content.truncate(7_500);
        let docs = vec![VNextDocumentInput::new(1, "periodic-q12.bin", content)];
        let first_blocks = vec![0u16];
        let q3 = crate::vnext_q3::build_q3_index_with_workers(
            &docs,
            &first_blocks,
            block_size,
            1,
            true,
            1,
        )
        .unwrap();
        assert_eq!(q3.periodic_q2_skip_from.len(), 1);
        assert_ne!(q3.periodic_q2_skip_from[0], u16::MAX);

        let baseline =
            build_content_q1_q2_flat_fused(&docs, &first_blocks, block_size, 1, None).unwrap();
        let reused = build_content_q1_q2_flat_fused(
            &docs,
            &first_blocks,
            block_size,
            1,
            Some(&q3.periodic_q2_skip_from),
        )
        .unwrap();

        assert_eq!(reused.0.bytes, baseline.0.bytes);
        assert_eq!(reused.0.stats, baseline.0.stats);
        assert_eq!(reused.1.bytes, baseline.1.bytes);
        assert_eq!(reused.1.stats, baseline.1.stats);
    }

    #[test]
    fn periodic_q2_active_lists_are_byte_identical_to_radix_oracle() {
        let block_size = 8_192u32;
        let mut docs = Vec::new();
        let mut first_blocks = Vec::new();
        let mut next_block = 0u16;
        for doc_id in 0..24u64 {
            let mut content = format!("active-list-oracle-{doc_id:04}: ").into_bytes();
            let pattern = match doc_id % 4 {
                0 => b"abcdefghijklmnopqrstuvwxyz012345".as_slice(),
                1 => b"the-quick-brown-fox-jumps-over-the-lazy-dog".as_slice(),
                2 => b"0123456789ABCDEF".as_slice(),
                _ => b"periodic-q2-owner-list".as_slice(),
            };
            while content.len() < 16_100 {
                content.extend_from_slice(pattern);
            }
            content.truncate(16_100);
            first_blocks.push(next_block);
            next_block = next_block
                .checked_add(content.len().div_ceil(block_size as usize) as u16)
                .unwrap();
            docs.push(VNextDocumentInput::new(
                doc_id + 1,
                format!("active-list-oracle-{doc_id}.bin"),
                content,
            ));
        }
        let universe = usize::from(next_block);
        let q3 = crate::vnext_q3::build_q3_index_with_workers(
            &docs,
            &first_blocks,
            block_size,
            universe,
            true,
            1,
        )
        .unwrap();
        assert!(use_periodic_q2_active_lists(
            &q3.periodic_q2_skip_from,
            universe
        ));

        let active = build_content_q1_q2_flat_fused_active_lists(
            &docs,
            &first_blocks,
            block_size,
            universe,
            &q3.periodic_q2_skip_from,
        )
        .unwrap();
        let radix = build_content_q1_q2_flat_fused_radix(
            &docs,
            &first_blocks,
            block_size,
            universe,
            Some(&q3.periodic_q2_skip_from),
        )
        .unwrap();

        assert_eq!(active.0.bytes, radix.0.bytes);
        assert_eq!(active.0.stats, radix.0.stats);
        assert_eq!(active.1.bytes, radix.1.bytes);
        assert_eq!(active.1.stats, radix.1.stats);
    }

    #[test]
    fn periodic_q2_active_list_gate_requires_three_quarters_exact_proof() {
        assert!(use_periodic_q2_active_lists(&[1, 2, 3, u16::MAX], 4));
        assert!(!use_periodic_q2_active_lists(
            &[1, 2, u16::MAX, u16::MAX],
            4
        ));
        assert!(!use_periodic_q2_active_lists(&[1, 2, 3], 4));
        assert!(!use_periodic_q2_active_lists(&[], 0));
    }

    #[test]
    fn q1_derived_from_unique_q2_preserves_final_and_boundary_bytes() {
        let block_size = 4u32;
        let contents = [
            Vec::new(),
            vec![0xff],
            b"aaaaa".to_vec(),
            b"abcdWXYZq".to_vec(),
            vec![0x00, 0xff, 0x00, 0xff, 0x7f],
        ];
        let mut docs = Vec::new();
        let mut first_blocks = Vec::new();
        let mut next_block = 0u16;
        for (doc_id, content) in contents.into_iter().enumerate() {
            first_blocks.push(next_block);
            next_block = next_block
                .checked_add(content.len().div_ceil(block_size as usize) as u16)
                .unwrap();
            docs.push(VNextDocumentInput::new(
                doc_id as u64 + 1,
                format!("q1-from-q2-{doc_id}.bin"),
                content,
            ));
        }
        let universe = usize::from(next_block);
        let (fused_q1, fused_q2) =
            build_content_q1_q2_flat_fused(&docs, &first_blocks, block_size, universe, None)
                .unwrap();
        let separate_q1 =
            build_content_q1_dense(&docs, &first_blocks, block_size, universe).unwrap();
        let separate_q2 =
            build_content_q2_sparse_flat(&docs, &first_blocks, block_size, universe).unwrap();

        assert_eq!(fused_q1.bytes, separate_q1.bytes);
        assert_eq!(fused_q1.stats, separate_q1.stats);
        assert_eq!(fused_q2.bytes, separate_q2.bytes);
        assert_eq!(fused_q2.stats, separate_q2.stats);
    }

    #[test]
    fn sharded_q2_matches_flat_oracle_across_many_high_bytes() {
        let block_size = 64u32;
        let mut docs = Vec::new();
        let mut first_blocks = Vec::new();
        let mut next_block = 0u16;
        let mut state = 0x6a09_e667_f3bc_c909u64;
        for doc_id in 0..96u64 {
            let mut content = Vec::with_capacity(513);
            for _ in 0..513 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                content.push((state >> 24) as u8);
            }
            first_blocks.push(next_block);
            let blocks = content.len().div_ceil(block_size as usize);
            next_block = next_block.checked_add(blocks as u16).unwrap();
            docs.push(VNextDocumentInput::new(
                doc_id + 1,
                format!("q2-shard-{doc_id}.bin"),
                content,
            ));
        }
        let sharded =
            build_content_q2_sparse(&docs, &first_blocks, block_size, usize::from(next_block))
                .unwrap();
        let flat =
            build_content_q2_sparse_flat(&docs, &first_blocks, block_size, usize::from(next_block))
                .unwrap();
        assert_eq!(sharded.bytes, flat.bytes);
        assert_eq!(sharded.stats, flat.stats);
    }

    #[test]
    fn sharded_q2_matches_flat_oracle_for_repetition_and_block_boundaries() {
        let block_size = 8u32;
        let docs = vec![
            VNextDocumentInput::new(1, "a.txt", b"aaaaaaaaabbbbbbbbbccccccccc".to_vec()),
            VNextDocumentInput::new(2, "b.txt", b"0123456789abcdef0123456789abcdef".to_vec()),
            VNextDocumentInput::new(3, "c.bin", (0u8..=255).collect::<Vec<_>>()),
        ];
        let mut first_blocks = Vec::new();
        let mut next_block = 0u16;
        for doc in &docs {
            first_blocks.push(next_block);
            next_block = next_block
                .checked_add(doc.normalized_content.len().div_ceil(block_size as usize) as u16)
                .unwrap();
        }
        let sharded =
            build_content_q2_sparse(&docs, &first_blocks, block_size, usize::from(next_block))
                .unwrap();
        let flat =
            build_content_q2_sparse_flat(&docs, &first_blocks, block_size, usize::from(next_block))
                .unwrap();
        assert_eq!(sharded.bytes, flat.bytes);
        assert_eq!(sharded.stats, flat.stats);
    }

    #[test]
    fn q2_adaptive_gate_selects_shards_only_below_measured_crossover() {
        fn docs_with_total(total: usize) -> Vec<VNextDocumentInput> {
            vec![VNextDocumentInput::new(
                1,
                "gate.bin",
                vec![b'x'; total + 1],
            )]
        }
        let small = docs_with_total(Q2_SHARDED_MAX_LOGICAL_OCCURRENCES as usize);
        let large = docs_with_total(Q2_SHARDED_MAX_LOGICAL_OCCURRENCES as usize + 1);
        let small_occurrences = small[0].normalized_content.len().saturating_sub(1) as u64;
        let large_occurrences = large[0].normalized_content.len().saturating_sub(1) as u64;
        assert!(use_sharded_content_q2(small_occurrences));
        assert!(!use_sharded_content_q2(large_occurrences));
    }

    #[test]
    fn one_pass_q2_radix_matches_full_packed_pair_order() {
        let mut values = Vec::new();
        for owner in 0u16..=1024 {
            let keys = [
                owner.wrapping_mul(17),
                0x0001,
                0x7f7f,
                owner.rotate_left(5),
                0xffff,
                owner.wrapping_mul(257),
            ];
            for key in keys {
                values.push((u32::from(key) << 16) | u32::from(owner));
            }
        }
        let mut expected = values.clone();
        expected.sort_unstable();
        let key_count = radix_sort_q2_monotonic_owner_upper16(&mut values);
        assert_eq!(values, expected);
        assert_eq!(
            key_count,
            expected
                .iter()
                .map(|value| value >> 16)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
        );
    }

    #[test]
    fn one_pass_q2_radix_preserves_owner_order_within_each_key() {
        let mut values = Vec::new();
        for owner in 0u16..4096 {
            values.push((0x1234u32 << 16) | u32::from(owner));
            if owner % 3 == 0 {
                values.push((0x4321u32 << 16) | u32::from(owner));
            }
        }
        let key_count = radix_sort_q2_monotonic_owner_upper16(&mut values);
        assert_eq!(key_count, 2);
        for key in [0x1234u16, 0x4321] {
            let owners = values
                .iter()
                .filter(|value| (**value >> 16) as u16 == key)
                .map(|value| *value as u16)
                .collect::<Vec<_>>();
            assert!(owners.windows(2).all(|pair| pair[0] <= pair[1]));
        }
    }
}
