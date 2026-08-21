use super::*;

fn frontier_profile_enabled() -> bool {
    std::env::var_os("PR_PROFILE_FRONTIER").is_some_and(|value| value == "1")
}

pub(super) const MAX_GRAM: usize = 16;

// q4 grouping is a hot path with millions of u32 lookups per segment. Use a small,
// deterministic avalanche mixer for this internal u32-only table instead of SipHash. Equality
// still decides key identity, so this changes performance only, never serialized semantics.
#[derive(Default)]
struct U32KeyHasher(u64);

impl std::hash::Hasher for U32KeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for &byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        self.0 = hash;
    }

    fn write_u32(&mut self, value: u32) {
        let mut z = u64::from(value).wrapping_add(0x9e37_79b9_7f4a_7c15);
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        self.0 = z ^ (z >> 31);
    }
}

type U32Map<V> = std::collections::HashMap<u32, V, std::hash::BuildHasherDefault<U32KeyHasher>>;

// q3 keys are exactly 24 bits. A 2 MiB membership bitmap is both bounded and much cheaper than
// hashing every candidate start while scanning TEXT_BLOB. Four workers consume only 8 MiB here.
const Q3_WORDS: usize = 1 << 18;

pub(super) struct DenseQ3Keys {
    bits: Box<[u64]>,
}

impl DenseQ3Keys {
    fn empty() -> Self {
        Self {
            bits: vec![0u64; Q3_WORDS].into_boxed_slice(),
        }
    }

    fn insert(&mut self, key: u32) {
        let key = key as usize;
        self.bits[key >> 6] |= 1u64 << (key & 63);
    }

    fn contains(&self, key: u32) -> bool {
        let key = key as usize;
        self.bits[key >> 6] & (1u64 << (key & 63)) != 0
    }

    fn count(&self) -> usize {
        self.bits
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(super) struct GramKey {
    pub(super) len: u8,
    pub(super) bytes: [u8; MAX_GRAM],
}

impl GramKey {
    pub(super) fn new(bytes: &[u8]) -> Self {
        debug_assert!((4..=MAX_GRAM).contains(&bytes.len()));
        let mut key = [0u8; MAX_GRAM];
        key[..bytes.len()].copy_from_slice(bytes);
        Self {
            len: bytes.len() as u8,
            bytes: key,
        }
    }

    fn extended(self, byte: u8) -> Self {
        debug_assert!(usize::from(self.len) < MAX_GRAM);
        let mut next = self;
        next.bytes[usize::from(next.len)] = byte;
        next.len += 1;
        next
    }

    pub(super) fn as_slice(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

pub(super) fn dense_q3_keys<S: SegmentBuildSource>(
    segment: &S,
    threshold_ppm: u32,
) -> Result<DenseQ3Keys> {
    let directory = segment.build_q3_directory()?;
    if directory.len() < 257 * 4 {
        return Err(SearchError::Format("bad q3 directory".into()));
    }
    let entries = rd32(directory, 256 * 4)? as usize;
    const PREFIX_BYTES: usize = 257 * 4;
    if directory.len() != PREFIX_BYTES + entries * 10 {
        return Err(SearchError::Format("bad q3 directory shape".into()));
    }
    let minimum = u64::from(segment.build_unit_count())
        .saturating_mul(u64::from(threshold_ppm))
        .div_ceil(1_000_000);
    let mut keys = DenseQ3Keys::empty();
    for high in 0..256usize {
        let begin = rd32(directory, high * 4)? as usize;
        let end = rd32(directory, (high + 1) * 4)? as usize;
        for idx in begin..end {
            let off = PREFIX_BYTES + idx * 10;
            let suffix = u32::from(rd16(directory, off)?);
            let count = rd32(directory, off + 2)? & 0x3fff_ffff;
            if u64::from(count) >= minimum {
                keys.insert((high as u32) << 16 | suffix);
            }
        }
    }
    Ok(keys)
}

// Once q4 is grouped, every longer gram has exactly one parent prefix. Keeping occurrences
// grouped by that parent lets the next depth branch only on one byte (256 buckets), avoiding the
// old full-GramKey HashMap count pass plus a second HashMap posting pass at every depth.
struct FrontierGroup {
    key: GramKey,
    occurrences: Vec<u64>,
    unit_count: u32,
}

struct InitialGroup {
    key: GramKey,
    occurrences: Vec<u64>,
    last_unit: u32,
    unit_count: u32,
}

fn build_q4_groups<S: SegmentBuildSource>(
    segment: &S,
    text_blob: &[u8],
    dense_q3: &DenseQ3Keys,
    minimum_child: u32,
) -> Result<Vec<FrontierGroup>> {
    let mut slots = U32Map::<usize>::default();
    let mut groups = Vec::<InitialGroup>::new();
    for unit in 0..segment.build_unit_count() {
        let (begin, end) = segment.build_unit_text_bounds(unit)?;
        let text = text_blob
            .get(begin..end)
            .ok_or_else(|| SearchError::Format("shared frontier unit text OOB".into()))?;
        if text.len() < 4 {
            continue;
        }
        for local in 0..=text.len() - 4 {
            let q3 = k3(text[local], text[local + 1], text[local + 2]);
            if !dense_q3.contains(q3) {
                continue;
            }
            let q4 = u32::from_le_bytes([
                text[local],
                text[local + 1],
                text[local + 2],
                text[local + 3],
            ]);
            let index = match slots.entry(q4) {
                std::collections::hash_map::Entry::Occupied(entry) => *entry.get(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let index = groups.len();
                    groups.push(InitialGroup {
                        key: GramKey::new(&text[local..local + 4]),
                        occurrences: Vec::new(),
                        last_unit: u32::MAX,
                        unit_count: 0,
                    });
                    entry.insert(index);
                    index
                }
            };
            let group = &mut groups[index];
            let absolute = u32::try_from(begin + local)
                .map_err(|_| SearchError::Format("shared frontier offset >4GiB".into()))?;
            group
                .occurrences
                .push((u64::from(unit) << 32) | u64::from(absolute));
            if group.last_unit != unit {
                group.last_unit = unit;
                group.unit_count += 1;
            }
        }
    }

    let mut dense = groups
        .into_iter()
        .filter_map(|group| {
            (group.unit_count >= minimum_child).then_some(FrontierGroup {
                key: group.key,
                occurrences: group.occurrences,
                unit_count: group.unit_count,
            })
        })
        .collect::<Vec<_>>();
    dense.sort_unstable_by_key(|group| group.key);
    Ok(dense)
}

fn append_posting(occurrences: &[u64], unit_count: u32, out: &mut Vec<u32>) {
    out.reserve(unit_count as usize);
    let mut last_unit = u32::MAX;
    for &packed in occurrences {
        let unit = (packed >> 32) as u32;
        if unit != last_unit {
            out.push(unit);
            last_unit = unit;
        }
    }
    debug_assert_eq!(out.len(), unit_count as usize);
}

fn extend_groups(
    text_blob: &[u8],
    groups: Vec<FrontierGroup>,
    next_len: usize,
    minimum_child: u32,
    minimum_parent: u32,
    all_keys: &mut Vec<GramKey>,
    all_postings: &mut Vec<Vec<u32>>,
) -> Vec<FrontierGroup> {
    let mut next_groups = Vec::new();
    for group in groups {
        let mut posting = Vec::with_capacity(group.unit_count as usize);
        append_posting(&group.occurrences, group.unit_count, &mut posting);
        all_keys.push(group.key);
        all_postings.push(posting);

        if group.unit_count < minimum_parent {
            continue;
        }

        let mut occurrences: [Vec<u64>; 256] = std::array::from_fn(|_| Vec::new());
        let mut last_units = [u32::MAX; 256];
        let mut unit_counts = [0u32; 256];
        let next_byte_offset = next_len - 1;
        for packed in group.occurrences {
            let unit = (packed >> 32) as u32;
            let absolute = packed as u32 as usize;
            let Some(&byte) = text_blob.get(absolute + next_byte_offset) else {
                continue;
            };
            let bucket = usize::from(byte);
            occurrences[bucket].push(packed);
            if last_units[bucket] != unit {
                last_units[bucket] = unit;
                unit_counts[bucket] += 1;
            }
        }
        for byte in 0..=u8::MAX {
            let bucket = usize::from(byte);
            if unit_counts[bucket] < minimum_child {
                continue;
            }
            next_groups.push(FrontierGroup {
                key: group.key.extended(byte),
                occurrences: std::mem::take(&mut occurrences[bucket]),
                unit_count: unit_counts[bucket],
            });
        }
    }
    next_groups
}

struct ShardFrontierState {
    groups: Vec<FrontierGroup>,
    emitted_keys: Vec<GramKey>,
    emitted_postings: Vec<Vec<u32>>,
}

fn process_shard_level(
    text_blob: &[u8],
    state: &mut ShardFrontierState,
    next_len: Option<usize>,
    minimum_child: u32,
    minimum_parent: u32,
) {
    let groups = std::mem::take(&mut state.groups);
    let mut next_groups = Vec::new();
    for group in groups {
        let mut posting = Vec::with_capacity(group.unit_count as usize);
        append_posting(&group.occurrences, group.unit_count, &mut posting);
        state.emitted_keys.push(group.key);
        state.emitted_postings.push(posting);

        let Some(next_len) = next_len else {
            continue;
        };
        if group.unit_count < minimum_parent {
            continue;
        }

        let mut occurrences: [Vec<u64>; 256] = std::array::from_fn(|_| Vec::new());
        let mut last_units = [u32::MAX; 256];
        let mut unit_counts = [0u32; 256];
        let next_byte_offset = next_len - 1;
        for packed in group.occurrences {
            let unit = (packed >> 32) as u32;
            let absolute = packed as u32 as usize;
            let Some(&byte) = text_blob.get(absolute + next_byte_offset) else {
                continue;
            };
            let bucket = usize::from(byte);
            occurrences[bucket].push(packed);
            if last_units[bucket] != unit {
                last_units[bucket] = unit;
                unit_counts[bucket] += 1;
            }
        }
        for byte in 0..=u8::MAX {
            let bucket = usize::from(byte);
            if unit_counts[bucket] < minimum_child {
                continue;
            }
            next_groups.push(FrontierGroup {
                key: group.key.extended(byte),
                occurrences: std::mem::take(&mut occurrences[bucket]),
                unit_count: unit_counts[bucket],
            });
        }
    }
    state.groups = next_groups;
}

fn finish_frontier_groups_sharded(
    text_blob: &[u8],
    groups: Vec<FrontierGroup>,
    pos2_minimum_child: u32,
    pos3_minimum_child: u32,
    max_gram: usize,
    preserve_pos2_q8_collisions: bool,
    workers: usize,
) -> (Vec<GramKey>, Vec<Vec<u32>>) {
    const PARALLEL_OCCURRENCE_THRESHOLD: usize = 250_000;
    let shared_minimum = pos2_minimum_child.min(pos3_minimum_child);
    let occurrence_count = groups
        .iter()
        .map(|group| group.occurrences.len())
        .sum::<usize>();
    let worker_count = workers.clamp(1, 4);
    if worker_count == 1 || occurrence_count < PARALLEL_OCCURRENCE_THRESHOLD {
        let mut groups = groups;
        let mut all_keys = Vec::<GramKey>::new();
        let mut all_postings = Vec::<Vec<u32>>::new();
        let mut len = 4usize;
        loop {
            if groups.is_empty() {
                break;
            }
            if len == max_gram {
                for group in groups {
                    let mut posting = Vec::with_capacity(group.unit_count as usize);
                    append_posting(&group.occurrences, group.unit_count, &mut posting);
                    all_keys.push(group.key);
                    all_postings.push(posting);
                }
                break;
            }
            let next_len = len + 1;
            let next_minimum = if next_len == 8 && preserve_pos2_q8_collisions {
                1
            } else if next_len <= 8 {
                shared_minimum
            } else {
                pos3_minimum_child
            };
            let minimum_parent = if len == 8 && preserve_pos2_q8_collisions {
                pos3_minimum_child
            } else {
                1
            };
            groups = extend_groups(
                text_blob,
                groups,
                next_len,
                next_minimum,
                minimum_parent,
                &mut all_keys,
                &mut all_postings,
            );
            len = next_len;
        }
        return (all_keys, all_postings);
    }

    let mut states = (0..256)
        .map(|_| ShardFrontierState {
            groups: Vec::new(),
            emitted_keys: Vec::new(),
            emitted_postings: Vec::new(),
        })
        .collect::<Vec<_>>();
    for group in groups {
        states[usize::from(group.key.bytes[0])].groups.push(group);
    }

    let mut all_keys = Vec::<GramKey>::new();
    let mut all_postings = Vec::<Vec<u32>>::new();
    let mut len = 4usize;
    loop {
        if states.iter().all(|state| state.groups.is_empty()) {
            break;
        }
        let (next_len, next_minimum, minimum_parent) = if len == max_gram {
            (None, 0, 0)
        } else {
            let next_len = len + 1;
            let next_minimum = if next_len == 8 && preserve_pos2_q8_collisions {
                1
            } else if next_len <= 8 {
                shared_minimum
            } else {
                pos3_minimum_child
            };
            let minimum_parent = if len == 8 && preserve_pos2_q8_collisions {
                pos3_minimum_child
            } else {
                1
            };
            (Some(next_len), next_minimum, minimum_parent)
        };

        let chunk_size = states.len().div_ceil(worker_count);
        std::thread::scope(|scope| {
            for chunk in states.chunks_mut(chunk_size) {
                scope.spawn(move || {
                    for state in chunk {
                        process_shard_level(
                            text_blob,
                            state,
                            next_len,
                            next_minimum,
                            minimum_parent,
                        );
                    }
                });
            }
        });

        for state in &mut states {
            all_keys.append(&mut state.emitted_keys);
            all_postings.append(&mut state.emitted_postings);
        }
        if next_len.is_none() {
            break;
        }
        len += 1;
    }
    debug_assert!(all_keys.windows(2).all(|pair| pair[0] <= pair[1]));
    (all_keys, all_postings)
}

pub(super) struct SharedPositionalRequest<'a, S: SegmentBuildSource> {
    pub(super) segment: &'a S,
    pub(super) dense_q3: &'a DenseQ3Keys,
    pub(super) pos1_keys: &'a [u32],
    pub(super) pos2_minimum_child: u32,
    pub(super) pos3_minimum_child: u32,
    pub(super) max_gram: usize,
    pub(super) preserve_pos2_q8_collisions: bool,
    pub(super) workers: usize,
}

pub(super) struct SharedPositionalBuild {
    pub(super) pos1_positions: Vec<Vec<u32>>,
    pub(super) keys: Vec<GramKey>,
    pub(super) postings: Vec<Vec<u32>>,
}

/// Scans TEXT_BLOB once to collect both PRPOS001 q3 positions and the q4 seed frontier used by
/// PRPOS002/003. This is the unified-build hot path; legacy standalone sidecar builders remain
/// available as independent repair/oracle paths.
pub(super) fn build_shared_postings_with_pos1<S: SegmentBuildSource>(
    request: SharedPositionalRequest<'_, S>,
) -> Result<SharedPositionalBuild> {
    let SharedPositionalRequest {
        segment,
        dense_q3,
        pos1_keys,
        pos2_minimum_child,
        pos3_minimum_child,
        max_gram,
        preserve_pos2_q8_collisions,
        workers,
    } = request;
    if !(4..=MAX_GRAM).contains(&max_gram) {
        return Err(SearchError::Format(
            "invalid shared frontier max gram".into(),
        ));
    }
    let text_blob = segment.build_text_blob()?;
    if text_blob.len() > u32::MAX as usize {
        return Err(SearchError::Format(
            "shared frontier text blob >4GiB limit".into(),
        ));
    }
    let mut pos_slots = U32Map::<usize>::default();
    for (slot, &key) in pos1_keys.iter().enumerate() {
        pos_slots.insert(key, slot);
    }
    let mut positions = vec![Vec::<u32>::new(); pos1_keys.len()];

    let shared_minimum = pos2_minimum_child.min(pos3_minimum_child);
    let profile_enabled = frontier_profile_enabled();
    let seed_started = profile_enabled.then(std::time::Instant::now);
    let mut q4_slots = U32Map::<usize>::default();
    let mut initial = Vec::<InitialGroup>::new();
    for unit in 0..segment.build_unit_count() {
        let (begin, end) = segment.build_unit_text_bounds(unit)?;
        let text = text_blob
            .get(begin..end)
            .ok_or_else(|| SearchError::Format("shared frontier unit text OOB".into()))?;
        if text.len() < 3 {
            continue;
        }
        let base = u32::try_from(begin)
            .map_err(|_| SearchError::Format("text offset exceeds u32".into()))?;
        for local in 0..=text.len() - 3 {
            let q3 = k3(text[local], text[local + 1], text[local + 2]);
            if let Some(&slot) = pos_slots.get(&q3) {
                positions[slot].push(
                    base + u32::try_from(local)
                        .map_err(|_| SearchError::Format("position exceeds u32".into()))?,
                );
            }
            if local + 3 >= text.len() || !dense_q3.contains(q3) {
                continue;
            }
            let q4 = u32::from_le_bytes([
                text[local],
                text[local + 1],
                text[local + 2],
                text[local + 3],
            ]);
            let index = match q4_slots.entry(q4) {
                std::collections::hash_map::Entry::Occupied(entry) => *entry.get(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let index = initial.len();
                    initial.push(InitialGroup {
                        key: GramKey::new(&text[local..local + 4]),
                        occurrences: Vec::new(),
                        last_unit: u32::MAX,
                        unit_count: 0,
                    });
                    entry.insert(index);
                    index
                }
            };
            let group = &mut initial[index];
            let absolute = base
                + u32::try_from(local)
                    .map_err(|_| SearchError::Format("shared frontier offset >4GiB".into()))?;
            group
                .occurrences
                .push((u64::from(unit) << 32) | u64::from(absolute));
            if group.last_unit != unit {
                group.last_unit = unit;
                group.unit_count += 1;
            }
        }
    }
    let mut groups = initial
        .into_iter()
        .filter_map(|group| {
            (group.unit_count >= shared_minimum).then_some(FrontierGroup {
                key: group.key,
                occurrences: group.occurrences,
                unit_count: group.unit_count,
            })
        })
        .collect::<Vec<_>>();
    groups.sort_unstable_by_key(|group| group.key);
    let seed_ms = seed_started
        .map(|started| started.elapsed().as_secs_f64() * 1000.0)
        .unwrap_or(0.0);
    let finish_started = profile_enabled.then(std::time::Instant::now);

    let (all_keys, all_postings) = finish_frontier_groups_sharded(
        text_blob,
        groups,
        pos2_minimum_child,
        pos3_minimum_child,
        max_gram,
        preserve_pos2_q8_collisions,
        workers,
    );
    debug_assert!(all_keys.windows(2).all(|pair| pair[0] <= pair[1]));
    if profile_enabled {
        eprintln!(
            "FRONTIER_PROFILE units={} text_bytes={} dense_q3={} seed_ms={:.3} finish_ms={:.3} groups={} keys={} postings={}",
            segment.build_unit_count(),
            text_blob.len(),
            dense_q3.count(),
            seed_ms,
            finish_started
                .map(|started| started.elapsed().as_secs_f64() * 1000.0)
                .unwrap_or(0.0),
            q4_slots.len(),
            all_keys.len(),
            all_postings.len(),
        );
    }
    Ok(SharedPositionalBuild {
        pos1_positions: positions,
        keys: all_keys,
        postings: all_postings,
    })
}

/// Builds the dense-gram frontier once. q4..q8 retain records dense enough for either
/// PRPOS002 or PRPOS003 so both tiers can be serialized from the same traversal. At q9+
/// only the PRPOS003 threshold matters, because PRPOS002 stops at q8.
pub(super) fn build_shared_postings<S: SegmentBuildSource>(
    segment: &S,
    dense_q3: &DenseQ3Keys,
    pos2_minimum_child: u32,
    pos3_minimum_child: u32,
    max_gram: usize,
    preserve_pos2_q8_collisions: bool,
) -> Result<(Vec<GramKey>, Vec<Vec<u32>>)> {
    if !(4..=MAX_GRAM).contains(&max_gram) {
        return Err(SearchError::Format(
            "invalid shared frontier max gram".into(),
        ));
    }
    let text_blob = segment.build_text_blob()?;
    if text_blob.len() > u32::MAX as usize {
        return Err(SearchError::Format(
            "shared frontier text blob >4GiB limit".into(),
        ));
    }

    let shared_minimum = pos2_minimum_child.min(pos3_minimum_child);
    let mut groups = build_q4_groups(segment, text_blob, dense_q3, shared_minimum)?;
    let mut all_keys = Vec::<GramKey>::new();
    let mut all_postings = Vec::<Vec<u32>>::new();
    let mut len = 4usize;
    loop {
        if groups.is_empty() {
            break;
        }
        if len == max_gram {
            for group in groups {
                let mut posting = Vec::with_capacity(group.unit_count as usize);
                append_posting(&group.occurrences, group.unit_count, &mut posting);
                all_keys.push(group.key);
                all_postings.push(posting);
            }
            break;
        }

        let next_len = len + 1;
        let next_minimum = if next_len == 8 && preserve_pos2_q8_collisions {
            1
        } else if next_len <= 8 {
            shared_minimum
        } else {
            pos3_minimum_child
        };
        // q8 must retain every exact child for PRPOS002's historical packed-key alias, but q9+
        // can only become dense beneath a q8 parent that is already dense for PRPOS003. Avoid
        // feeding sparse compatibility-only q8 groups into the long-gram frontier.
        let minimum_parent = if len == 8 && preserve_pos2_q8_collisions {
            pos3_minimum_child
        } else {
            1
        };
        groups = extend_groups(
            text_blob,
            groups,
            next_len,
            next_minimum,
            minimum_parent,
            &mut all_keys,
            &mut all_postings,
        );
        len = next_len;
    }

    debug_assert!(all_keys.windows(2).all(|pair| pair[0] <= pair[1]));
    Ok((all_keys, all_postings))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dense_q3_bitmap_covers_full_24bit_domain() {
        let mut keys = DenseQ3Keys::empty();
        for key in [0, 1, 0x00ab_cdef, 0x00ff_ffff] {
            keys.insert(key);
        }
        for key in [0, 1, 0x00ab_cdef, 0x00ff_ffff] {
            assert!(keys.contains(key));
        }
        for key in [2, 0x0001_0000, 0x00ab_cdee, 0x00ff_fffe] {
            assert!(!keys.contains(key));
        }
    }

    #[test]
    fn q4_hot_map_preserves_distinct_u32_keys() {
        let mut map = U32Map::<u32>::default();
        let keys = [
            0,
            1,
            0x0102_0304,
            0x0403_0201,
            0x7fff_ffff,
            0x8000_0000,
            u32::MAX,
        ];
        for (index, key) in keys.into_iter().enumerate() {
            assert_eq!(map.insert(key, index as u32), None);
        }
        for (index, key) in keys.into_iter().enumerate() {
            assert_eq!(map.get(&key), Some(&(index as u32)));
        }
    }
}
