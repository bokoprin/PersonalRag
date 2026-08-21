use std::cmp::Ordering as CmpOrdering;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Instant;

use crate::fold_ascii;
use crate::format::section;
use crate::format::{
    BuilderKind, Desc, FOOTER_MAGIC, HEADER_SIZE, MANIFEST_MAGIC, Q3DirKind, Q3Encoding, Result,
    SECTION_COUNT, SEG_MAGIC, SearchError, fnv1a, k2, k3, rd16, rd32, rd64,
};
use crate::mapped_file::MappedFile;

mod positional;
mod positional2;
mod positional23;
mod positional3;
mod positional_frontier;
pub use positional::{
    PosCodec, PosSidecarBuildReport, PositionalIndex, build_positional_sidecars,
    verify_positional_sidecars,
};
pub use positional2::{
    Pos2BuildReport, Positional2Index, build_positional2_sidecars, verify_positional2_sidecars,
};
pub use positional3::{
    Pos3BuildReport, Pos3Policy, Positional3Index, build_positional3_sidecars,
    verify_positional3_sidecars,
};
pub use positional23::{Pos23BuildReport, build_positional23_sidecars};

#[derive(Clone, Copy, Debug)]
struct Q3Meta {
    encoding: Q3Encoding,
    offset: u32,
    bytes: u32,
    count: u32,
}

pub(crate) struct ExactMatcher<'a> {
    needle: &'a [u8],
    skip: [usize; 256],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentPlanMode {
    CandidateDriven,
    OrderDriven,
    ScanDriven,
    PositionalDriven,
    VariableGramDriven,
    AdaptiveGramDriven,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentQueryPlan {
    pub mode: ContentPlanMode,
    pub workers: usize,
    pub estimated_candidates: u64,
    pub estimated_density_ppm: u32,
    pub verifier_weight: u8,
}

fn verifier_weight(query_len: usize) -> u8 {
    match query_len {
        0 => 0,
        1 | 3 => 1,
        2 => 4,
        _ => 6,
    }
}

fn plan_workers(
    query_len: usize,
    estimated_candidates: u64,
    segments: usize,
    available_workers: usize,
) -> usize {
    if query_len == 0 || segments < 2 || available_workers <= 1 {
        return 1;
    }
    let weight = u64::from(verifier_weight(query_len));
    let work = estimated_candidates.saturating_mul(weight.max(1));
    if work < 16_384 {
        1
    } else if work < 131_072 {
        2.min(available_workers).min(segments)
    } else {
        available_workers.min(segments)
    }
}

fn first_n_candidate_multiplier(query_len: usize) -> u64 {
    match query_len {
        1 | 3 => 64,
        2 => 16,
        _ => 12,
    }
}

fn first_n_candidate_threshold(query_len: usize, limit: usize, docs: u64) -> u64 {
    let limit = u64::try_from(limit).unwrap_or(u64::MAX);
    let base = limit.saturating_mul(first_n_candidate_multiplier(query_len));
    if query_len <= 3 {
        return base;
    }

    // Long literals can have dense q3 candidates while exact matches are sparse or absent.
    // Scale the candidate path with corpus size so those false-positive-heavy queries do not
    // fall into an O(docs) ordered scan too early. Short queries retain the proven fixed rule.
    base.saturating_add(limit.saturating_mul(docs).isqrt())
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ContentSearchDiagnostics {
    pub segments: u64,
    pub candidates: u64,
    pub matched_units: u64,
    pub hit_docs: u64,
    pub candidate_ns: u128,
    pub verify_ns: u128,
    pub expand_ns: u128,
    pub best_q3_candidates: u64,
    pub second_q3_candidates: u64,
    pub best2_q3_intersection: u64,
}

const Q2_SIDECAR_MAGIC: &[u8; 8] = b"PRQ2C001";
const Q2_SIDECAR_HEADER_BYTES: usize = 32;
const Q2_SIDECAR_PREFIX_BYTES: usize = 257 * 4;
const Q2_SIDECAR_RECORD_BYTES: usize = 8;
const Q2_SIDECAR_FOOTER_BYTES: usize = 8;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Q2SidecarBuildReport {
    pub segments: usize,
    pub bytes: u64,
    pub records: u64,
    pub dense_records: u64,
}

struct Q2CompactSidecar {
    mapped: MappedFile,
    unit_count: u32,
    records: u32,
    payload_offset: usize,
    payload_bytes: u32,
}

#[derive(Clone, Copy, Debug)]
struct Q2RecordMatch {
    dense: bool,
    count: u16,
    begin: u32,
    end: u32,
}

impl Q2CompactSidecar {
    fn open(path: &Path, expected_units: u32, expected_segment_checksum: u64) -> Result<Self> {
        let mapped = MappedFile::open(path)?;
        let data = mapped.as_slice();
        if data.len() < Q2_SIDECAR_HEADER_BYTES + Q2_SIDECAR_PREFIX_BYTES + Q2_SIDECAR_FOOTER_BYTES
        {
            return Err(SearchError::Format("q2 sidecar too small".into()));
        }
        if data.get(..8) != Some(Q2_SIDECAR_MAGIC.as_slice()) {
            return Err(SearchError::Format("bad q2 sidecar magic".into()));
        }
        let unit_count = rd32(data, 8)?;
        let records = rd32(data, 12)?;
        let payload_bytes = rd32(data, 16)?;
        let segment_checksum = rd64(data, 24)?;
        if unit_count != expected_units || segment_checksum != expected_segment_checksum {
            return Err(SearchError::Format(
                "q2 sidecar does not match segment".into(),
            ));
        }
        let records_usize = usize::try_from(records)
            .map_err(|_| SearchError::Format("q2 sidecar record count too large".into()))?;
        let payload_offset = Q2_SIDECAR_HEADER_BYTES
            .checked_add(Q2_SIDECAR_PREFIX_BYTES)
            .and_then(|value| {
                value.checked_add(records_usize.saturating_mul(Q2_SIDECAR_RECORD_BYTES))
            })
            .ok_or_else(|| SearchError::Format("q2 sidecar layout overflow".into()))?;
        let payload_end = payload_offset
            .checked_add(
                usize::try_from(payload_bytes)
                    .map_err(|_| SearchError::Format("q2 sidecar payload too large".into()))?,
            )
            .ok_or_else(|| SearchError::Format("q2 sidecar payload overflow".into()))?;
        if payload_end.checked_add(Q2_SIDECAR_FOOTER_BYTES) != Some(data.len()) {
            return Err(SearchError::Format("q2 sidecar length mismatch".into()));
        }
        let expected = rd64(data, payload_end)?;
        if fnv1a(&data[..payload_end]) != expected {
            return Err(SearchError::Format("q2 sidecar checksum mismatch".into()));
        }
        let prefix =
            &data[Q2_SIDECAR_HEADER_BYTES..Q2_SIDECAR_HEADER_BYTES + Q2_SIDECAR_PREFIX_BYTES];
        let mut previous = 0u32;
        for index in 0..=256usize {
            let value = rd32(prefix, index * 4)?;
            if value < previous || value > records {
                return Err(SearchError::Format(
                    "q2 sidecar prefix table is not monotonic".into(),
                ));
            }
            previous = value;
        }
        if previous != records {
            return Err(SearchError::Format(
                "q2 sidecar prefix terminal mismatch".into(),
            ));
        }
        let sidecar = Self {
            mapped,
            unit_count,
            records,
            payload_offset,
            payload_bytes,
        };
        sidecar.validate_records()?;
        Ok(sidecar)
    }

    fn data(&self) -> &[u8] {
        self.mapped.as_slice()
    }

    fn record_offset(&self, index: usize) -> Result<usize> {
        if index >= self.records as usize {
            return Err(SearchError::Format(
                "q2 sidecar record index out of bounds".into(),
            ));
        }
        Q2_SIDECAR_HEADER_BYTES
            .checked_add(Q2_SIDECAR_PREFIX_BYTES)
            .and_then(|value| value.checked_add(index.saturating_mul(Q2_SIDECAR_RECORD_BYTES)))
            .ok_or_else(|| SearchError::Format("q2 sidecar record offset overflow".into()))
    }

    fn prefix(&self, high: usize) -> Result<usize> {
        usize::try_from(rd32(self.data(), Q2_SIDECAR_HEADER_BYTES + high * 4)?)
            .map_err(|_| SearchError::Format("q2 sidecar prefix too large".into()))
    }

    fn validate_records(&self) -> Result<()> {
        let mut last_key: Option<u16> = None;
        let mut last_offset = 0u32;
        for high in 0..256usize {
            let begin = self.prefix(high)?;
            let end = self.prefix(high + 1)?;
            let mut last_low = None;
            for index in begin..end {
                let off = self.record_offset(index)?;
                let low = self.data()[off];
                if last_low.is_some_and(|previous| low <= previous) {
                    return Err(SearchError::Format(
                        "q2 sidecar records are not sorted".into(),
                    ));
                }
                last_low = Some(low);
                let key = ((high as u16) << 8) | u16::from(low);
                if last_key.is_some_and(|previous| key <= previous) {
                    return Err(SearchError::Format("q2 sidecar key order invalid".into()));
                }
                last_key = Some(key);
                let payload_offset = rd32(self.data(), off + 4)?;
                if payload_offset < last_offset || payload_offset > self.payload_bytes {
                    return Err(SearchError::Format(
                        "q2 sidecar payload offsets invalid".into(),
                    ));
                }
                last_offset = payload_offset;
            }
        }
        Ok(())
    }

    fn find_record(&self, key: u16) -> Result<Option<Q2RecordMatch>> {
        let high = usize::from((key >> 8) as u8);
        let low = key as u8;
        let mut lo = self.prefix(high)?;
        let mut hi = self.prefix(high + 1)?;
        while lo < hi {
            let middle = (lo + hi) / 2;
            let off = self.record_offset(middle)?;
            if self.data()[off] < low {
                lo = middle + 1;
            } else {
                hi = middle;
            }
        }
        if lo >= self.prefix(high + 1)? {
            return Ok(None);
        }
        let off = self.record_offset(lo)?;
        if self.data()[off] != low {
            return Ok(None);
        }
        let dense = self.data()[off + 1] & 1 != 0;
        let count = rd16(self.data(), off + 2)?;
        let begin = rd32(self.data(), off + 4)?;
        let end = if lo + 1 < self.records as usize {
            rd32(self.data(), self.record_offset(lo + 1)? + 4)?
        } else {
            self.payload_bytes
        };
        if end < begin || end > self.payload_bytes {
            return Err(SearchError::Format(
                "q2 sidecar record payload range invalid".into(),
            ));
        }
        Ok(Some(Q2RecordMatch {
            dense,
            count,
            begin,
            end,
        }))
    }

    fn unit_ids(&self, key: u16) -> Result<Vec<u32>> {
        let Some(record) = self.find_record(key)? else {
            return Ok(Vec::new());
        };
        let begin = self.payload_offset
            + usize::try_from(record.begin)
                .map_err(|_| SearchError::Format("q2 sidecar offset too large".into()))?;
        let end = self.payload_offset
            + usize::try_from(record.end)
                .map_err(|_| SearchError::Format("q2 sidecar offset too large".into()))?;
        let bytes = self
            .data()
            .get(begin..end)
            .ok_or_else(|| SearchError::Format("q2 sidecar payload out of bounds".into()))?;
        let mut out = Vec::with_capacity(usize::from(record.count));
        if record.dense {
            for (byte_index, &byte) in bytes.iter().enumerate() {
                let mut bits = byte;
                while bits != 0 {
                    let bit = bits.trailing_zeros() as usize;
                    let unit = byte_index * 8 + bit;
                    if unit < self.unit_count as usize {
                        out.push(unit as u32);
                    }
                    bits &= bits - 1;
                }
            }
        } else {
            if bytes.len() % 2 != 0 {
                return Err(SearchError::Format(
                    "q2 sparse sidecar payload misaligned".into(),
                ));
            }
            for encoded in bytes.chunks_exact(2) {
                out.push(u32::from(u16::from_le_bytes([encoded[0], encoded[1]])));
            }
        }
        if out.len() != usize::from(record.count) {
            return Err(SearchError::Format(
                "q2 sidecar posting count mismatch".into(),
            ));
        }
        Ok(out)
    }
}

#[derive(Clone, Copy, Debug)]
struct Q2PrototypeRecord {
    key: u16,
    offset: u32,
    bytes: u32,
    count: u16,
    dense: bool,
}

#[derive(Debug)]
struct Q2PrototypeSegment {
    unit_count: u32,
    records: Vec<Q2PrototypeRecord>,
    payload: Vec<u8>,
    persisted_bytes: u64,
}

fn build_q2_prototype_from_pairs(
    unit_count: u32,
    mut pairs: Vec<u32>,
) -> Result<Q2PrototypeSegment> {
    if unit_count > u16::MAX.into() {
        return Err(SearchError::Format(
            "q2 prototype requires <=65535 content units per segment".into(),
        ));
    }
    pairs.sort_unstable();
    pairs.dedup();
    let dense_bytes = usize::try_from(unit_count)
        .map_err(|_| SearchError::Format("q2 prototype unit count too large".into()))?
        .div_ceil(8);
    let mut records = Vec::new();
    let mut payload = Vec::new();
    let mut index = 0usize;
    while index < pairs.len() {
        let key = (pairs[index] >> 16) as u16;
        let start = index;
        index += 1;
        while index < pairs.len() && (pairs[index] >> 16) as u16 == key {
            index += 1;
        }
        let count = index - start;
        let dense = dense_bytes < count * 2;
        let offset = u32::try_from(payload.len())
            .map_err(|_| SearchError::Format("q2 prototype payload too large".into()))?;
        if dense {
            let begin = payload.len();
            payload.resize(begin + dense_bytes, 0);
            for &pair in &pairs[start..index] {
                let unit = usize::from(pair as u16);
                payload[begin + unit / 8] |= 1u8 << (unit % 8);
            }
        } else {
            for &pair in &pairs[start..index] {
                payload.extend_from_slice(&(pair as u16).to_le_bytes());
            }
        }
        let bytes = u32::try_from(payload.len())
            .ok()
            .and_then(|end| end.checked_sub(offset))
            .ok_or_else(|| SearchError::Format("q2 prototype payload overflow".into()))?;
        records.push(Q2PrototypeRecord {
            key,
            offset,
            bytes,
            count: u16::try_from(count)
                .map_err(|_| SearchError::Format("q2 posting count too large".into()))?,
            dense,
        });
    }
    let directory_bytes = 257u64 * 4
        + u64::try_from(records.len())
            .map_err(|_| SearchError::Format("q2 record count too large".into()))?
            * Q2_SIDECAR_RECORD_BYTES as u64;
    let persisted_bytes = directory_bytes
        .checked_add(
            u64::try_from(payload.len())
                .map_err(|_| SearchError::Format("q2 prototype payload length too large".into()))?,
        )
        .ok_or_else(|| SearchError::Format("q2 prototype size overflow".into()))?;
    Ok(Q2PrototypeSegment {
        unit_count,
        records,
        payload,
        persisted_bytes,
    })
}

fn serialize_q2_sidecar_parts(
    unit_count: u32,
    segment_checksum: u64,
    prototype: &Q2PrototypeSegment,
) -> Result<Vec<u8>> {
    if prototype.unit_count != unit_count {
        return Err(SearchError::Format(
            "q2 sidecar prototype/segment mismatch".into(),
        ));
    }
    let record_count = u32::try_from(prototype.records.len())
        .map_err(|_| SearchError::Format("q2 sidecar record count overflow".into()))?;
    let payload_bytes = u32::try_from(prototype.payload.len())
        .map_err(|_| SearchError::Format("q2 sidecar payload overflow".into()))?;
    let mut prefix = [0u32; 257];
    let mut cursor = 0usize;
    for (high, slot) in prefix.iter_mut().enumerate().take(256) {
        *slot = u32::try_from(cursor)
            .map_err(|_| SearchError::Format("q2 sidecar prefix overflow".into()))?;
        while cursor < prototype.records.len()
            && usize::from((prototype.records[cursor].key >> 8) as u8) == high
        {
            cursor += 1;
        }
    }
    prefix[256] = record_count;
    if cursor != prototype.records.len() {
        return Err(SearchError::Format(
            "q2 sidecar prefix construction mismatch".into(),
        ));
    }
    let capacity = Q2_SIDECAR_HEADER_BYTES
        .checked_add(Q2_SIDECAR_PREFIX_BYTES)
        .and_then(|value| {
            value.checked_add(
                prototype
                    .records
                    .len()
                    .saturating_mul(Q2_SIDECAR_RECORD_BYTES),
            )
        })
        .and_then(|value| value.checked_add(prototype.payload.len()))
        .and_then(|value| value.checked_add(Q2_SIDECAR_FOOTER_BYTES))
        .ok_or_else(|| SearchError::Format("q2 sidecar size overflow".into()))?;
    let mut out = Vec::with_capacity(capacity);
    out.extend_from_slice(Q2_SIDECAR_MAGIC);
    out.extend_from_slice(&unit_count.to_le_bytes());
    out.extend_from_slice(&record_count.to_le_bytes());
    out.extend_from_slice(&payload_bytes.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&segment_checksum.to_le_bytes());
    for value in prefix {
        out.extend_from_slice(&value.to_le_bytes());
    }
    for record in &prototype.records {
        out.push(record.key as u8);
        out.push(u8::from(record.dense));
        out.extend_from_slice(&record.count.to_le_bytes());
        out.extend_from_slice(&record.offset.to_le_bytes());
    }
    out.extend_from_slice(&prototype.payload);
    let checksum = fnv1a(&out);
    out.extend_from_slice(&checksum.to_le_bytes());
    Ok(out)
}

#[derive(Debug)]
pub struct Q2PrototypeIndex {
    segments: Vec<Q2PrototypeSegment>,
    persisted_bytes: u64,
    records: u64,
    dense_records: u64,
}

impl Q2PrototypeIndex {
    #[must_use]
    pub const fn persisted_bytes(&self) -> u64 {
        self.persisted_bytes
    }

    #[must_use]
    pub const fn records(&self) -> u64 {
        self.records
    }

    #[must_use]
    pub const fn dense_records(&self) -> u64 {
        self.dense_records
    }
}

impl ContentSearchDiagnostics {
    fn merge(&mut self, other: Self) {
        self.segments += other.segments;
        self.candidates += other.candidates;
        self.matched_units += other.matched_units;
        self.hit_docs += other.hit_docs;
        self.candidate_ns += other.candidate_ns;
        self.verify_ns += other.verify_ns;
        self.expand_ns += other.expand_ns;
        self.best_q3_candidates += other.best_q3_candidates;
        self.second_q3_candidates += other.second_q3_candidates;
        self.best2_q3_intersection += other.best2_q3_intersection;
    }
}

impl<'a> ExactMatcher<'a> {
    pub(crate) fn new(needle: &'a [u8]) -> Self {
        let mut skip = [needle.len(); 256];
        if needle.len() >= 2 {
            for (index, &byte) in needle[..needle.len() - 1].iter().enumerate() {
                skip[usize::from(byte)] = needle.len() - 1 - index;
            }
        }
        Self { needle, skip }
    }

    pub(crate) const fn needle(&self) -> &'a [u8] {
        self.needle
    }

    pub(crate) fn contains(&self, haystack: &[u8]) -> bool {
        let size = self.needle.len();
        if size == 0 {
            return true;
        }
        if size == 1 {
            return haystack.contains(&self.needle[0]);
        }
        if haystack.len() < size {
            return false;
        }
        #[cfg(target_arch = "x86_64")]
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: runtime feature detection guarantees AVX2 availability. The helper
            // performs unaligned loads only for ranges proven to be inside `haystack`.
            return unsafe { self.contains_avx2(haystack) };
        }
        self.contains_bmh(haystack)
    }

    /// Return the first exact match at or after `start`.
    ///
    /// This deliberately reuses the precomputed Boyer-Moore-Horspool skip table rather than
    /// allocating a temporary slice/matcher for every document. vNext's dense-query content-blob
    /// scan uses it to walk one immutable blob monotonically and jump straight to the next
    /// document after the first accepted hit.
    pub(crate) fn find_from(&self, haystack: &[u8], start: usize) -> Option<usize> {
        let size = self.needle.len();
        if size == 0 {
            return (start <= haystack.len()).then_some(start);
        }
        if start > haystack.len() || haystack.len().saturating_sub(start) < size {
            return None;
        }
        if size == 1 {
            return haystack[start..]
                .iter()
                .position(|byte| *byte == self.needle[0])
                .map(|offset| start + offset);
        }

        // Long literals retain the proven BMH path. SIMD is most useful when a short literal has
        // little skip distance and enough remaining input to amortize one 32-lane probe.
        #[cfg(target_arch = "x86_64")]
        if size <= 8
            && std::arch::is_x86_feature_detected!("avx2")
            && haystack.len().saturating_sub(start) >= 64
        {
            let last_index = size - 1;
            let first_last = haystack[start + last_index];
            if first_last == self.needle[last_index]
                && &haystack[start..start + size] == self.needle
            {
                return Some(start);
            }
            let next_start = start.saturating_add(self.skip[usize::from(first_last)]);
            if next_start > haystack.len() - size {
                return None;
            }
            // SAFETY: runtime feature detection guarantees AVX2 availability.
            return unsafe { self.find_from_avx2_fast(haystack, next_start) };
        }

        self.find_from_bmh_profiled(haystack, start).0
    }

    /// Same result as [`Self::find_from`], plus the number of candidate start positions examined.
    ///
    /// The counter is used only by the opt-in vNext query profiler. It deliberately counts SIMD
    /// lanes as candidate positions so AVX2 and scalar runs can be compared without timing every
    /// inner-loop operation.
    pub(crate) fn find_from_profiled(
        &self,
        haystack: &[u8],
        start: usize,
    ) -> (Option<usize>, usize) {
        let size = self.needle.len();
        if size == 0 {
            return ((start <= haystack.len()).then_some(start), 0);
        }
        if start > haystack.len() || haystack.len().saturating_sub(start) < size {
            return (None, 0);
        }
        if size == 1 {
            let suffix = &haystack[start..];
            let found = suffix
                .iter()
                .position(|byte| *byte == self.needle[0])
                .map(|offset| start + offset);
            let examined = found.map_or(suffix.len(), |position| position - start + 1);
            return (found, examined);
        }

        let last_index = size - 1;
        let first_last = haystack[start + last_index];
        if first_last == self.needle[last_index] && &haystack[start..start + size] == self.needle {
            return (Some(start), 1);
        }
        let next_start = start.saturating_add(self.skip[usize::from(first_last)]);
        if next_start > haystack.len() - size {
            return (None, 1);
        }

        #[cfg(target_arch = "x86_64")]
        if size <= 8
            && std::arch::is_x86_feature_detected!("avx2")
            && haystack.len().saturating_sub(next_start) >= 64
        {
            // SAFETY: runtime feature detection guarantees AVX2 availability. The helper performs
            // unaligned loads only after proving that all 32 candidate lanes fit in `haystack`.
            let (found, examined) = unsafe { self.find_from_avx2(haystack, next_start) };
            return (found, examined.saturating_add(1));
        }

        let (found, examined) = self.find_from_bmh_profiled(haystack, next_start);
        (found, examined.saturating_add(1))
    }

    fn find_from_bmh_profiled(&self, haystack: &[u8], start: usize) -> (Option<usize>, usize) {
        let size = self.needle.len();
        let last_index = size - 1;
        let last_needle = self.needle[last_index];
        let maximum_start = haystack.len() - size;
        let mut offset = start;
        let mut examined = 0usize;
        while offset <= maximum_start {
            examined += 1;
            let last = haystack[offset + last_index];
            if last == last_needle && &haystack[offset..offset + size] == self.needle {
                return (Some(offset), examined);
            }
            offset = offset.saturating_add(self.skip[usize::from(last)]);
        }
        (None, examined)
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn find_from_avx2_fast(&self, haystack: &[u8], start: usize) -> Option<usize> {
        use std::arch::x86_64::{
            __m256i, _mm256_cmpeq_epi8, _mm256_loadu_si256, _mm256_movemask_epi8, _mm256_set1_epi8,
        };

        let size = self.needle.len();
        let last_index = size - 1;
        let maximum_start = haystack.len() - size;
        let first = _mm256_set1_epi8(self.needle[0] as i8);
        let last = _mm256_set1_epi8(self.needle[last_index] as i8);
        let mut offset = start;
        while offset + 31 <= maximum_start {
            // SAFETY: loop bounds prove both unaligned 32-byte loads fit.
            let first_bytes =
                unsafe { _mm256_loadu_si256(haystack.as_ptr().add(offset).cast::<__m256i>()) };
            let last_bytes = unsafe {
                _mm256_loadu_si256(haystack.as_ptr().add(offset + last_index).cast::<__m256i>())
            };
            let mut matches = (_mm256_movemask_epi8(_mm256_cmpeq_epi8(first_bytes, first))
                & _mm256_movemask_epi8(_mm256_cmpeq_epi8(last_bytes, last)))
                as u32;
            if size == 2 && matches != 0 {
                return Some(offset + matches.trailing_zeros() as usize);
            }
            while matches != 0 {
                let bit = matches.trailing_zeros() as usize;
                let candidate = offset + bit;
                if &haystack[candidate..candidate + size] == self.needle {
                    return Some(candidate);
                }
                matches &= matches - 1;
            }
            offset += 32;
        }
        self.find_from_bmh_profiled(haystack, offset).0
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn find_from_avx2(&self, haystack: &[u8], start: usize) -> (Option<usize>, usize) {
        use std::arch::x86_64::{
            __m256i, _mm256_cmpeq_epi8, _mm256_loadu_si256, _mm256_movemask_epi8, _mm256_set1_epi8,
        };

        let size = self.needle.len();
        let last_index = size - 1;
        let maximum_start = haystack.len() - size;
        let first = _mm256_set1_epi8(self.needle[0] as i8);
        let last = _mm256_set1_epi8(self.needle[last_index] as i8);
        let mut offset = start;
        let mut examined = 0usize;

        while offset + 31 <= maximum_start {
            // SAFETY: `offset + 31 <= maximum_start` proves both 32-byte loads fit, including the
            // second load shifted by `last_index`.
            let first_bytes =
                unsafe { _mm256_loadu_si256(haystack.as_ptr().add(offset).cast::<__m256i>()) };
            let last_bytes = unsafe {
                _mm256_loadu_si256(haystack.as_ptr().add(offset + last_index).cast::<__m256i>())
            };
            let mut matches = (_mm256_movemask_epi8(_mm256_cmpeq_epi8(first_bytes, first))
                & _mm256_movemask_epi8(_mm256_cmpeq_epi8(last_bytes, last)))
                as u32;
            examined += 32;
            if size == 2 && matches != 0 {
                return (Some(offset + matches.trailing_zeros() as usize), examined);
            }
            while matches != 0 {
                let bit = matches.trailing_zeros() as usize;
                let candidate = offset + bit;
                if &haystack[candidate..candidate + size] == self.needle {
                    return (Some(candidate), examined);
                }
                matches &= matches - 1;
            }
            offset += 32;
        }

        let (found, tail_examined) = self.find_from_bmh_profiled(haystack, offset);
        (found, examined.saturating_add(tail_examined))
    }

    fn contains_bmh(&self, haystack: &[u8]) -> bool {
        let size = self.needle.len();
        let last_index = size - 1;
        let last_needle = self.needle[last_index];
        let mut offset = 0usize;
        while offset <= haystack.len() - size {
            let last = haystack[offset + last_index];
            if last == last_needle && &haystack[offset..offset + size] == self.needle {
                return true;
            }
            offset += self.skip[usize::from(last)];
        }
        false
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn contains_avx2(&self, haystack: &[u8]) -> bool {
        use std::arch::x86_64::{
            __m256i, _mm256_cmpeq_epi8, _mm256_loadu_si256, _mm256_movemask_epi8, _mm256_set1_epi8,
        };

        let size = self.needle.len();
        let last_index = size - 1;
        let maximum_start = haystack.len() - size;
        let first = _mm256_set1_epi8(self.needle[0] as i8);
        let last = _mm256_set1_epi8(self.needle[last_index] as i8);
        let mut offset = 0usize;

        while offset + 31 <= maximum_start {
            // SAFETY: `offset + 31 <= maximum_start` proves both 32-byte loads are within
            // the haystack, including the second load shifted by `last_index`.
            let first_bytes =
                unsafe { _mm256_loadu_si256(haystack.as_ptr().add(offset).cast::<__m256i>()) };
            let last_bytes = unsafe {
                _mm256_loadu_si256(haystack.as_ptr().add(offset + last_index).cast::<__m256i>())
            };
            let mut matches = (_mm256_movemask_epi8(_mm256_cmpeq_epi8(first_bytes, first))
                & _mm256_movemask_epi8(_mm256_cmpeq_epi8(last_bytes, last)))
                as u32;
            if size == 2 && matches != 0 {
                return true;
            }
            while matches != 0 {
                let bit = matches.trailing_zeros() as usize;
                let candidate = offset + bit;
                if &haystack[candidate..candidate + size] == self.needle {
                    return true;
                }
                matches &= matches - 1;
            }
            offset += 32;
        }

        while offset <= maximum_start {
            if haystack[offset] == self.needle[0]
                && haystack[offset + last_index] == self.needle[last_index]
                && (size == 2 || &haystack[offset..offset + size] == self.needle)
            {
                return true;
            }
            offset += 1;
        }
        false
    }
}

pub(super) trait SegmentBuildSource {
    fn build_unit_count(&self) -> u32;
    fn build_text_blob(&self) -> Result<&[u8]>;
    fn build_q3_directory(&self) -> Result<&[u8]>;
    fn build_unit_text_bounds(&self, unit: u32) -> Result<(usize, usize)>;
    fn build_segment_checksum(&self) -> Result<u64>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccelerationProfile {
    /// Build every accelerator supported by the current frozen segment formats.
    Full,
    /// Production-oriented full-build profile: keep the cheap Q2 accelerator, but skip the
    /// expensive positional frontier whose build cost is not amortized by current exact-literal
    /// query workloads. Exact search semantics remain identical to `Full`.
    Balanced,
    /// Build only accelerators whose fixed/materialization cost is justified by the delta size.
    AdaptiveDelta,
    /// Build only the exact base segment.
    None,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct MemoryAccelerationReport {
    pub(crate) q2_bytes: u64,
    pub(crate) pos1_bytes: u64,
    pub(crate) pos2_bytes: u64,
    pub(crate) pos3_bytes: u64,
}

pub(crate) struct MemorySegmentBuildSource<'a> {
    pub(crate) unit_text_off: &'a [u64],
    pub(crate) text_blob: &'a [u8],
    pub(crate) q3_directory: &'a [u8],
    pub(crate) segment_checksum: u64,
}

impl SegmentBuildSource for MemorySegmentBuildSource<'_> {
    fn build_unit_count(&self) -> u32 {
        u32::try_from(self.unit_text_off.len().saturating_sub(1)).unwrap_or(u32::MAX)
    }

    fn build_text_blob(&self) -> Result<&[u8]> {
        Ok(self.text_blob)
    }

    fn build_q3_directory(&self) -> Result<&[u8]> {
        Ok(self.q3_directory)
    }

    fn build_unit_text_bounds(&self, unit: u32) -> Result<(usize, usize)> {
        let index = usize::try_from(unit)
            .map_err(|_| SearchError::Format("content unit index too large".into()))?;
        let begin = *self
            .unit_text_off
            .get(index)
            .ok_or_else(|| SearchError::Format("unit text offset OOB".into()))?;
        let end = *self
            .unit_text_off
            .get(index + 1)
            .ok_or_else(|| SearchError::Format("unit text end OOB".into()))?;
        let begin = usize::try_from(begin)
            .map_err(|_| SearchError::Format("unit text offset too large".into()))?;
        let end = usize::try_from(end)
            .map_err(|_| SearchError::Format("unit text offset too large".into()))?;
        if begin > end || end > self.text_blob.len() {
            return Err(SearchError::Format("unit text range OOB".into()));
        }
        Ok((begin, end))
    }

    fn build_segment_checksum(&self) -> Result<u64> {
        Ok(self.segment_checksum)
    }
}

fn write_unpublished_sidecar(path: &Path, bytes: &[u8], durable: bool) -> Result<()> {
    if path.exists() {
        fs::remove_file(path)?;
    }
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(bytes)?;
    if durable {
        file.sync_all()?;
    }
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o444))?;
    }
    Ok(())
}

fn adaptive_accelerators(unit_count: u32, text_bytes: usize) -> (bool, bool, bool) {
    // Delta generations are latency-sensitive L0 components. Q2 is cheap enough to materialize
    // once the batch is non-trivial, while positional sidecars are intentionally deferred to
    // Full compaction: measured 1k-5k document deltas spent far more time building POS1/POS23
    // than they can amortize before the existing compaction policy takes over.
    let q2 = unit_count >= 64 && text_bytes >= 64 * 1024;
    (q2, false, false)
}

pub(crate) struct MemoryAccelerationRequest<'a> {
    pub(crate) segment_path: &'a Path,
    pub(crate) unit_text_off: &'a [u64],
    pub(crate) text_blob: &'a [u8],
    pub(crate) q3_directory: &'a [u8],
    pub(crate) segment_checksum: u64,
    pub(crate) q2_pairs: Option<Vec<u32>>,
    pub(crate) profile: AccelerationProfile,
    pub(crate) build_workers: usize,
    pub(crate) durable: bool,
}

pub(crate) fn build_accelerators_from_memory(
    request: MemoryAccelerationRequest<'_>,
) -> Result<MemoryAccelerationReport> {
    let MemoryAccelerationRequest {
        segment_path,
        unit_text_off,
        text_blob,
        q3_directory,
        segment_checksum,
        q2_pairs,
        profile,
        build_workers,
        durable,
    } = request;
    if profile == AccelerationProfile::None {
        return Ok(MemoryAccelerationReport::default());
    }
    let source = MemorySegmentBuildSource {
        unit_text_off,
        text_blob,
        q3_directory,
        segment_checksum,
    };
    let unit_count = source.build_unit_count();
    let (build_q2, build_pos1, build_pos23) = match profile {
        AccelerationProfile::Full => (true, true, true),
        AccelerationProfile::Balanced => (unit_count > 0 && !text_blob.is_empty(), false, false),
        AccelerationProfile::AdaptiveDelta => adaptive_accelerators(unit_count, text_blob.len()),
        AccelerationProfile::None => unreachable!(),
    };
    if !build_q2 && !build_pos1 && !build_pos23 {
        return Ok(MemoryAccelerationReport::default());
    }
    let frontier_workers = if build_pos23 {
        let logical_cpus = std::thread::available_parallelism().map_or(1, usize::from);
        (logical_cpus / build_workers.max(1)).clamp(1, 4)
    } else {
        1
    };
    let mut report = MemoryAccelerationReport::default();
    let profile_build = std::env::var_os("PR_PROFILE_BUILD").is_some();
    let profile_total_started = Instant::now();
    let mut q2_ms = 0.0f64;
    let mut frontier_ms = 0.0f64;
    let mut pos1_ms = 0.0f64;
    let mut pos2_ms = 0.0f64;
    let mut pos3_ms = 0.0f64;
    let mut write_ms = 0.0f64;

    if build_q2 {
        let stage_started = Instant::now();
        if unit_count > u32::from(u16::MAX) {
            if profile == AccelerationProfile::Full {
                return Err(SearchError::Format(
                    "q2 prototype requires <=65535 content units per segment".into(),
                ));
            }
        } else {
            let prototype =
                build_q2_prototype_from_pairs(unit_count, q2_pairs.unwrap_or_default())?;
            let bytes = serialize_q2_sidecar_parts(unit_count, segment_checksum, &prototype)?;
            q2_ms = stage_started.elapsed().as_secs_f64() * 1000.0;
            let write_started = Instant::now();
            write_unpublished_sidecar(&segment_path.with_extension("q2c"), &bytes, durable)?;
            write_ms += write_started.elapsed().as_secs_f64() * 1000.0;
            report.q2_bytes = bytes.len() as u64;
        }
    }

    if build_pos1 && build_pos23 {
        const THRESHOLD: u32 = 500_000;
        let codec = positional::PosCodec::production();
        let frontier_started = Instant::now();
        let pos1_keys = positional::dense_q3_keys(&source, THRESHOLD)?;
        let dense_q3 = positional_frontier::dense_q3_keys(&source, THRESHOLD)?;
        let minimum = u64::from(unit_count)
            .saturating_mul(u64::from(THRESHOLD))
            .div_ceil(1_000_000) as u32;
        let shared = positional_frontier::build_shared_postings_with_pos1(
            positional_frontier::SharedPositionalRequest {
                segment: &source,
                dense_q3: &dense_q3,
                pos1_keys: &pos1_keys,
                pos2_minimum_child: minimum,
                pos3_minimum_child: minimum,
                max_gram: positional_frontier::MAX_GRAM,
                preserve_pos2_q8_collisions: true,
                workers: frontier_workers,
            },
        )?;
        frontier_ms = frontier_started.elapsed().as_secs_f64() * 1000.0;
        let stage_started = Instant::now();
        let pos1 = positional::serialize_segment_sidecar_from_positions(
            &source,
            codec,
            THRESHOLD,
            &pos1_keys,
            &shared.pos1_positions,
        )?;
        pos1_ms = stage_started.elapsed().as_secs_f64() * 1000.0;
        let stage_started = Instant::now();
        let pos2 = positional2::build_segment_sidecar2_from_postings(
            &source,
            THRESHOLD,
            THRESHOLD,
            &shared.keys,
            &shared.postings,
        )?;
        pos2_ms = stage_started.elapsed().as_secs_f64() * 1000.0;
        let stage_started = Instant::now();
        let pos3 = positional3::build_segment_sidecar3_from_postings(
            &source,
            THRESHOLD,
            THRESHOLD,
            positional_frontier::MAX_GRAM,
            positional3::Pos3Policy::Adaptive,
            &shared.keys,
            &shared.postings,
        )?;
        pos3_ms = stage_started.elapsed().as_secs_f64() * 1000.0;
        let write_started = Instant::now();
        write_unpublished_sidecar(
            &segment_path.with_extension(format!("pos-{}", codec.tag())),
            &pos1,
            durable,
        )?;
        write_unpublished_sidecar(&segment_path.with_extension("pos2"), &pos2, durable)?;
        write_unpublished_sidecar(&segment_path.with_extension("pos3"), &pos3, durable)?;
        write_ms += write_started.elapsed().as_secs_f64() * 1000.0;
        report.pos1_bytes = pos1.len() as u64;
        report.pos2_bytes = pos2.len() as u64;
        report.pos3_bytes = pos3.len() as u64;
    } else {
        if build_pos1 {
            let stage_started = Instant::now();
            let codec = positional::PosCodec::production();
            let bytes = positional::build_segment_sidecar(&source, codec, 500_000)?;
            pos1_ms = stage_started.elapsed().as_secs_f64() * 1000.0;
            let write_started = Instant::now();
            write_unpublished_sidecar(
                &segment_path.with_extension(format!("pos-{}", codec.tag())),
                &bytes,
                durable,
            )?;
            write_ms += write_started.elapsed().as_secs_f64() * 1000.0;
            report.pos1_bytes = bytes.len() as u64;
        }
        if build_pos23 {
            const THRESHOLD: u32 = 500_000;
            let dense_q3 = positional_frontier::dense_q3_keys(&source, THRESHOLD)?;
            let minimum = u64::from(unit_count)
                .saturating_mul(u64::from(THRESHOLD))
                .div_ceil(1_000_000) as u32;
            let (keys, postings) = positional_frontier::build_shared_postings(
                &source,
                &dense_q3,
                minimum,
                minimum,
                positional_frontier::MAX_GRAM,
                true,
            )?;
            let pos2 = positional2::build_segment_sidecar2_from_postings(
                &source, THRESHOLD, THRESHOLD, &keys, &postings,
            )?;
            let pos3 = positional3::build_segment_sidecar3_from_postings(
                &source,
                THRESHOLD,
                THRESHOLD,
                positional_frontier::MAX_GRAM,
                positional3::Pos3Policy::Adaptive,
                &keys,
                &postings,
            )?;
            write_unpublished_sidecar(&segment_path.with_extension("pos2"), &pos2, durable)?;
            write_unpublished_sidecar(&segment_path.with_extension("pos3"), &pos3, durable)?;
            report.pos2_bytes = pos2.len() as u64;
            report.pos3_bytes = pos3.len() as u64;
        }
    }
    if profile_build {
        eprintln!(
            "BUILD_ACCEL_PHASE segment={} q2_ms={:.3} frontier_ms={:.3} pos1_ms={:.3} pos2_ms={:.3} pos3_ms={:.3} write_ms={:.3} total_ms={:.3}",
            segment_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("?"),
            q2_ms,
            frontier_ms,
            pos1_ms,
            pos2_ms,
            pos3_ms,
            write_ms,
            profile_total_started.elapsed().as_secs_f64() * 1000.0,
        );
    }
    Ok(report)
}

pub struct SegmentReader {
    path: PathBuf,
    mapped: MappedFile,
    desc: [Desc; SECTION_COUNT],
    builder_kind: BuilderKind,
    q3_dir_kind: Q3DirKind,
    doc_base: u32,
    doc_count: u32,
    unit_count: u32,
}

impl SegmentBuildSource for SegmentReader {
    fn build_unit_count(&self) -> u32 {
        self.unit_count
    }

    fn build_text_blob(&self) -> Result<&[u8]> {
        self.section(section::TEXT_BLOB)
    }

    fn build_q3_directory(&self) -> Result<&[u8]> {
        self.section(section::CQ3DIR)
    }

    fn build_unit_text_bounds(&self, unit: u32) -> Result<(usize, usize)> {
        if unit >= self.unit_count {
            return Err(SearchError::Format("content unit out of bounds".into()));
        }
        let begin = self.u64_at(section::UNIT_TEXT_OFF, u64::from(unit))?;
        let end = self.u64_at(section::UNIT_TEXT_OFF, u64::from(unit) + 1)?;
        let begin = usize::try_from(begin)
            .map_err(|_| SearchError::Format("unit text offset too large".into()))?;
        let end = usize::try_from(end)
            .map_err(|_| SearchError::Format("unit text offset too large".into()))?;
        Ok((begin, end))
    }

    fn build_segment_checksum(&self) -> Result<u64> {
        self.stored_checksum()
    }
}

impl SegmentReader {
    pub fn open(path: impl AsRef<Path>, verify_checksum: bool) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let mapped = MappedFile::open(&path)?;
        let data = mapped.as_slice();
        if data.len() < HEADER_SIZE + 16 {
            return Err(SearchError::Format("segment too small".into()));
        }
        if data.get(..8) != Some(SEG_MAGIC.as_slice()) {
            return Err(SearchError::Format("bad segment magic".into()));
        }
        if rd32(data, 8)? != 5 {
            return Err(SearchError::Format("bad segment version".into()));
        }
        let builder_kind = BuilderKind::from_u32(rd32(data, 12)?)?;
        let doc_base = rd32(data, 16)?;
        let doc_count = rd32(data, 20)?;
        let unit_count = rd32(data, 24)?;
        if usize::try_from(rd32(data, 28)?).ok() != Some(SECTION_COUNT) {
            return Err(SearchError::Format("bad section count".into()));
        }
        let q3_dir_kind = Q3DirKind::from_u32(rd32(data, 480)?)?;
        let mut desc = [Desc::default(); SECTION_COUNT];
        let payload_end = data.len() - 16;
        for (index, item) in desc.iter_mut().enumerate() {
            let off = rd64(data, 32 + index * 16)?;
            let size = rd64(data, 40 + index * 16)?;
            let end = off
                .checked_add(size)
                .ok_or_else(|| SearchError::Format("section offset overflow".into()))?;
            if end > payload_end as u64 || off < HEADER_SIZE as u64 {
                return Err(SearchError::Format(format!(
                    "section {index} out of bounds"
                )));
            }
            *item = Desc { off, size };
        }
        if data.get(payload_end..payload_end + 8) != Some(FOOTER_MAGIC.as_slice()) {
            return Err(SearchError::Format("bad segment footer".into()));
        }
        if verify_checksum {
            let expected = rd64(data, payload_end + 8)?;
            let actual = fnv1a(&data[..payload_end]);
            if expected != actual {
                return Err(SearchError::Format("checksum mismatch".into()));
            }
        }
        let reader = Self {
            path,
            mapped,
            desc,
            builder_kind,
            q3_dir_kind,
            doc_base,
            doc_count,
            unit_count,
        };
        reader.validate_shape()?;
        Ok(reader)
    }

    fn validate_shape(&self) -> Result<()> {
        self.expect_count(section::DOC_NAME_OFF, u64::from(self.doc_count) + 1, 8)?;
        self.expect_count(section::DOC_UNIT, u64::from(self.doc_count), 4)?;
        self.expect_count(section::UNIT_TEXT_OFF, u64::from(self.unit_count) + 1, 8)?;
        self.expect_count(section::UNIT_DOC_OFF, u64::from(self.unit_count) + 1, 8)?;
        if self.desc[section::CQ1MASK].size != u64::from(self.unit_count) * 32 {
            return Err(SearchError::Format(
                "production q1 byte-mask missing".into(),
            ));
        }
        if self.desc[section::CQ1OFF].size != 0
            || self.desc[section::CQ1POST].size != 0
            || self.desc[section::CQ1RARE].size != 0
        {
            return Err(SearchError::Format(
                "unexpected legacy content q1 posting section".into(),
            ));
        }
        if self.desc[section::CQ2OFF].size != 0 || self.desc[section::CQ2POST].size != 0 {
            return Err(SearchError::Format("unexpected content q2 section".into()));
        }
        if self.desc[section::TEXT_BLOCK_DIR].size != 0
            || self.desc[section::TEXT_BLOCK_BLOB].size != 0
        {
            return Err(SearchError::Format(
                "compressed verifier store is not part of the production default".into(),
            ));
        }
        if self.q3_dir_kind != Q3DirKind::Prefix10 {
            return Err(SearchError::Format(
                "production prefix10 q3 directory required".into(),
            ));
        }
        Ok(())
    }

    fn expect_count(&self, section_index: usize, count: u64, element_bytes: u64) -> Result<()> {
        if self.desc[section_index].size != count.saturating_mul(element_bytes) {
            return Err(SearchError::Format(format!(
                "section {section_index} has unexpected size"
            )));
        }
        Ok(())
    }

    #[must_use]
    pub const fn doc_base(&self) -> u32 {
        self.doc_base
    }

    #[must_use]
    pub const fn doc_count(&self) -> u32 {
        self.doc_count
    }

    #[must_use]
    pub const fn unit_count(&self) -> u32 {
        self.unit_count
    }

    #[must_use]
    pub const fn builder_kind(&self) -> BuilderKind {
        self.builder_kind
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn stored_checksum(&self) -> Result<u64> {
        let data = self.mapped.as_slice();
        rd64(data, data.len().saturating_sub(8))
    }

    fn section(&self, index: usize) -> Result<&[u8]> {
        let desc = self.desc[index];
        let begin = usize::try_from(desc.off)
            .map_err(|_| SearchError::Format("section offset too large".into()))?;
        let size = usize::try_from(desc.size)
            .map_err(|_| SearchError::Format("section size too large".into()))?;
        self.mapped
            .as_slice()
            .get(begin..begin + size)
            .ok_or_else(|| SearchError::Format("section slice out of bounds".into()))
    }

    fn u32_at(&self, section_index: usize, index: u64) -> Result<u32> {
        let byte = usize::try_from(
            index
                .checked_mul(4)
                .ok_or_else(|| SearchError::Format("u32 section index overflow".into()))?,
        )
        .map_err(|_| SearchError::Format("u32 section index too large".into()))?;
        rd32(self.section(section_index)?, byte)
    }

    fn u64_at(&self, section_index: usize, index: u64) -> Result<u64> {
        let byte = usize::try_from(
            index
                .checked_mul(8)
                .ok_or_else(|| SearchError::Format("u64 section index overflow".into()))?,
        )
        .map_err(|_| SearchError::Format("u64 section index too large".into()))?;
        rd64(self.section(section_index)?, byte)
    }

    fn raw_blob(&self, section_index: usize, begin: u64, end: u64) -> Result<&[u8]> {
        if end < begin {
            return Err(SearchError::Format("reversed blob range".into()));
        }
        let section = self.section(section_index)?;
        let begin = usize::try_from(begin)
            .map_err(|_| SearchError::Format("blob offset too large".into()))?;
        let end = usize::try_from(end)
            .map_err(|_| SearchError::Format("blob offset too large".into()))?;
        section
            .get(begin..end)
            .ok_or_else(|| SearchError::Format("blob range out of bounds".into()))
    }

    fn unit_contains(&self, unit: u32, matcher: &ExactMatcher<'_>) -> Result<bool> {
        if unit >= self.unit_count {
            return Err(SearchError::Format("content unit out of bounds".into()));
        }
        let begin = self.u64_at(section::UNIT_TEXT_OFF, u64::from(unit))?;
        let end = self.u64_at(section::UNIT_TEXT_OFF, u64::from(unit) + 1)?;
        Ok(matcher.contains(self.raw_blob(section::TEXT_BLOB, begin, end)?))
    }

    fn name_contains(&self, doc: u32, matcher: &ExactMatcher<'_>) -> Result<bool> {
        if doc >= self.doc_count {
            return Err(SearchError::Format("document out of bounds".into()));
        }
        let begin = self.u64_at(section::DOC_NAME_OFF, u64::from(doc))?;
        let end = self.u64_at(section::DOC_NAME_OFF, u64::from(doc) + 1)?;
        Ok(matcher.contains(self.raw_blob(section::NAME_BLOB, begin, end)?))
    }

    fn document_name_bytes(&self, doc: u32) -> Result<&[u8]> {
        if doc >= self.doc_count {
            return Err(SearchError::Format("document out of bounds".into()));
        }
        let begin = self.u64_at(section::DOC_NAME_OFF, u64::from(doc))?;
        let end = self.u64_at(section::DOC_NAME_OFF, u64::from(doc) + 1)?;
        self.raw_blob(section::NAME_BLOB, begin, end)
    }

    fn document_content_bytes(&self, doc: u32) -> Result<&[u8]> {
        if doc >= self.doc_count {
            return Err(SearchError::Format("document out of bounds".into()));
        }
        let unit = self.u32_at(section::DOC_UNIT, u64::from(doc))?;
        if unit >= self.unit_count {
            return Err(SearchError::Format(
                "document content unit out of bounds".into(),
            ));
        }
        let begin = self.u64_at(section::UNIT_TEXT_OFF, u64::from(unit))?;
        let end = self.u64_at(section::UNIT_TEXT_OFF, u64::from(unit) + 1)?;
        self.raw_blob(section::TEXT_BLOB, begin, end)
    }

    fn document_contains_local(
        &self,
        doc: u32,
        matcher: &ExactMatcher<'_>,
        names: bool,
    ) -> Result<bool> {
        if names {
            Ok(matcher.contains(self.document_name_bytes(doc)?))
        } else {
            Ok(matcher.contains(self.document_content_bytes(doc)?))
        }
    }

    fn raw_range(&self, offsets_section: usize, key: u64) -> Result<(u32, u32)> {
        Ok((
            self.u32_at(offsets_section, key)?,
            self.u32_at(offsets_section, key + 1)?,
        ))
    }

    fn content_q3_meta(&self, key: u32) -> Result<Option<Q3Meta>> {
        let directory = self.section(section::CQ3DIR)?;
        if directory.len() < 257 * 4 {
            return Err(SearchError::Format("bad prefix10 q3 directory".into()));
        }
        let prefix = (key >> 16) as usize;
        let mut lo = usize::try_from(rd32(directory, prefix * 4)?)
            .map_err(|_| SearchError::Format("q3 prefix offset too large".into()))?;
        let end = usize::try_from(rd32(directory, (prefix + 1) * 4)?)
            .map_err(|_| SearchError::Format("q3 prefix offset too large".into()))?;
        let entries = usize::try_from(rd32(directory, 256 * 4)?)
            .map_err(|_| SearchError::Format("q3 entry count too large".into()))?;
        if lo > end || end > entries || directory.len() != 257 * 4 + entries * 10 {
            return Err(SearchError::Format(
                "bad prefix10 q3 directory shape".into(),
            ));
        }
        let suffix = key as u16;
        let records = 257 * 4;
        let mut hi = end;
        while lo < hi {
            let middle = (lo + hi) / 2;
            match rd16(directory, records + middle * 10)?.cmp(&suffix) {
                CmpOrdering::Less => lo = middle + 1,
                CmpOrdering::Equal | CmpOrdering::Greater => hi = middle,
            }
        }
        if lo == end || rd16(directory, records + lo * 10)? != suffix {
            return Ok(None);
        }
        let packed = rd32(directory, records + lo * 10 + 2)?;
        let offset = rd32(directory, records + lo * 10 + 6)?;
        let next_offset = if lo + 1 < entries {
            rd32(directory, records + (lo + 1) * 10 + 6)?
        } else {
            u32::try_from(self.desc[section::CQ3POST].size)
                .map_err(|_| SearchError::Format("q3 payload exceeds 4GiB".into()))?
        };
        if next_offset < offset {
            return Err(SearchError::Format("bad q3 posting offsets".into()));
        }
        Ok(Some(Q3Meta {
            encoding: Q3Encoding::from_packed(packed)?,
            offset,
            bytes: next_offset - offset,
            count: packed & 0x3fff_ffff,
        }))
    }

    fn decode_content_q3(&self, meta: Q3Meta) -> Result<Vec<u32>> {
        let postings = self.section(section::CQ3POST)?;
        let begin = usize::try_from(meta.offset)
            .map_err(|_| SearchError::Format("q3 offset too large".into()))?;
        let bytes = usize::try_from(meta.bytes)
            .map_err(|_| SearchError::Format("q3 size too large".into()))?;
        let payload = postings
            .get(begin..begin + bytes)
            .ok_or_else(|| SearchError::Format("q3 payload out of bounds".into()))?;
        let expected = usize::try_from(meta.count)
            .map_err(|_| SearchError::Format("q3 count too large".into()))?;
        let mut out = Vec::with_capacity(expected);
        match meta.encoding {
            Q3Encoding::InlineU32 => {
                if payload.len() != expected * 4 {
                    return Err(SearchError::Format("bad inline q3 payload".into()));
                }
                for offset in (0..payload.len()).step_by(4) {
                    out.push(rd32(payload, offset)?);
                }
            }
            Q3Encoding::DeltaVarint => {
                let mut pos = 0usize;
                let mut id = 0u32;
                while pos < payload.len() && out.len() < expected {
                    let mut value = 0u32;
                    let mut shift = 0u32;
                    loop {
                        let byte = *payload
                            .get(pos)
                            .ok_or_else(|| SearchError::Format("truncated q3 varint".into()))?;
                        pos += 1;
                        if shift > 28 {
                            return Err(SearchError::Format("invalid q3 varint".into()));
                        }
                        value |= u32::from(byte & 0x7f) << shift;
                        if byte & 0x80 == 0 {
                            break;
                        }
                        shift += 7;
                    }
                    id = if out.is_empty() {
                        value
                    } else {
                        id.checked_add(value)
                            .ok_or_else(|| SearchError::Format("q3 id overflow".into()))?
                    };
                    out.push(id);
                }
                if pos != payload.len() {
                    return Err(SearchError::Format("q3 varint trailing bytes".into()));
                }
            }
            Q3Encoding::Block256Bitmap => {
                let mut pos = 0usize;
                while pos < payload.len() {
                    if payload.len() - pos < 36 {
                        return Err(SearchError::Format("truncated block q3".into()));
                    }
                    let block = rd32(payload, pos)?;
                    pos += 4;
                    for byte_index in 0..32usize {
                        let mut bits = payload[pos + byte_index];
                        while bits != 0 {
                            let bit = bits.trailing_zeros();
                            out.push(block * 256 + (byte_index as u32) * 8 + bit);
                            bits &= bits - 1;
                        }
                    }
                    pos += 32;
                }
            }
            Q3Encoding::DenseBitset => {
                for (byte_index, &byte) in payload.iter().enumerate() {
                    let mut bits = byte;
                    while bits != 0 {
                        let bit = bits.trailing_zeros();
                        let id = u32::try_from(byte_index * 8)
                            .map_err(|_| SearchError::Format("dense q3 id overflow".into()))?
                            + bit;
                        if id < self.unit_count {
                            out.push(id);
                        }
                        bits &= bits - 1;
                    }
                }
            }
        }
        if out.len() != expected {
            return Err(SearchError::Format(format!(
                "q3 decoded count mismatch: expected {expected}, got {}",
                out.len()
            )));
        }
        Ok(out)
    }

    fn name_q3_best(&self, query: &[u8]) -> Result<Option<(u32, u32)>> {
        let directory = self.section(section::NQ3DIR)?;
        if directory.len() % 12 != 0 {
            return Err(SearchError::Format("bad name q3 directory".into()));
        }
        let entries = directory.len() / 12;
        let mut best: Option<(u32, u32)> = None;
        for gram in query.windows(3) {
            let key = k3(gram[0], gram[1], gram[2]);
            let mut lo = 0usize;
            let mut hi = entries;
            while lo < hi {
                let middle = (lo + hi) / 2;
                if rd32(directory, middle * 12)? < key {
                    lo = middle + 1;
                } else {
                    hi = middle;
                }
            }
            if lo == entries || rd32(directory, lo * 12)? != key {
                return Ok(None);
            }
            let start = rd32(directory, lo * 12 + 4)?;
            let count = rd32(directory, lo * 12 + 8)?;
            if best.is_none_or(|(_, current)| count < current) {
                best = Some((start, count));
            }
        }
        Ok(best)
    }

    fn content_candidates(&self, query: &[u8]) -> Result<Vec<u32>> {
        match query.len() {
            0 => Ok(Vec::new()),
            1 => {
                let mask = self.section(section::CQ1MASK)?;
                let byte = query[0];
                let mask_offset = usize::from(byte / 8);
                let bit = 1u8 << (byte % 8);
                let mut out = Vec::new();
                for (unit, row) in mask.chunks_exact(32).enumerate() {
                    if row[mask_offset] & bit != 0 {
                        out.push(u32::try_from(unit).map_err(|_| {
                            SearchError::Format("content unit id exceeds u32".into())
                        })?);
                    }
                }
                Ok(out)
            }
            2 => {
                let mask = self.section(section::CQ1MASK)?;
                let first = query[0];
                let second = query[1];
                let first_offset = usize::from(first / 8);
                let second_offset = usize::from(second / 8);
                let first_bit = 1u8 << (first % 8);
                let second_bit = 1u8 << (second % 8);
                let mut out = Vec::new();
                for (unit, row) in mask.chunks_exact(32).enumerate() {
                    if row[first_offset] & first_bit != 0 && row[second_offset] & second_bit != 0 {
                        out.push(u32::try_from(unit).map_err(|_| {
                            SearchError::Format("content unit id exceeds u32".into())
                        })?);
                    }
                }
                Ok(out)
            }
            _ => {
                let mut best: Option<Q3Meta> = None;
                for gram in query.windows(3) {
                    let Some(meta) = self.content_q3_meta(k3(gram[0], gram[1], gram[2]))? else {
                        return Ok(Vec::new());
                    };
                    if best.is_none_or(|current| meta.count < current.count) {
                        best = Some(meta);
                    }
                }
                self.decode_content_q3(
                    best.ok_or_else(|| {
                        SearchError::Format("missing q3 candidate metadata".into())
                    })?,
                )
            }
        }
    }

    fn content_candidate_count(&self, query: &[u8]) -> Result<u64> {
        match query.len() {
            0 => Ok(0),
            1 => {
                let mask = self.section(section::CQ1MASK)?;
                let byte = query[0];
                let mask_offset = usize::from(byte / 8);
                let bit = 1u8 << (byte % 8);
                Ok(mask
                    .chunks_exact(32)
                    .filter(|row| row[mask_offset] & bit != 0)
                    .count() as u64)
            }
            2 => {
                let mask = self.section(section::CQ1MASK)?;
                let first = query[0];
                let second = query[1];
                let first_offset = usize::from(first / 8);
                let second_offset = usize::from(second / 8);
                let first_bit = 1u8 << (first % 8);
                let second_bit = 1u8 << (second % 8);
                Ok(mask
                    .chunks_exact(32)
                    .filter(|row| {
                        row[first_offset] & first_bit != 0 && row[second_offset] & second_bit != 0
                    })
                    .count() as u64)
            }
            _ => {
                let mut best: Option<u32> = None;
                for gram in query.windows(3) {
                    let Some(meta) = self.content_q3_meta(k3(gram[0], gram[1], gram[2]))? else {
                        return Ok(0);
                    };
                    best = Some(best.map_or(meta.count, |count| count.min(meta.count)));
                }
                Ok(u64::from(best.unwrap_or(0)))
            }
        }
    }

    fn name_candidates(&self, query: &[u8]) -> Result<Vec<u32>> {
        match query.len() {
            0 => Ok(Vec::new()),
            1 => {
                let (begin, end) = self.raw_range(section::NQ1OFF, u64::from(query[0]))?;
                (begin..end)
                    .map(|offset| self.u32_at(section::NQ1POST, u64::from(offset)))
                    .collect()
            }
            2 => {
                let key = u64::from(k2(query[0], query[1]));
                let (begin, end) = self.raw_range(section::NQ2OFF, key)?;
                (begin..end)
                    .map(|offset| self.u32_at(section::NQ2POST, u64::from(offset)))
                    .collect()
            }
            _ => {
                let Some((start, count)) = self.name_q3_best(query)? else {
                    return Ok(Vec::new());
                };
                (start..start + count)
                    .map(|offset| self.u32_at(section::NQ3POST, u64::from(offset)))
                    .collect()
            }
        }
    }

    fn build_q2_prototype_segment(&self) -> Result<Q2PrototypeSegment> {
        if self.unit_count > u16::MAX.into() {
            return Err(SearchError::Format(
                "q2 prototype requires <=65535 content units per segment".into(),
            ));
        }
        let mut pairs = Vec::<u32>::new();
        let mut seen = vec![0u64; 1 << 10];
        let mut touched = Vec::<usize>::new();
        for unit in 0..self.unit_count {
            let begin = self.u64_at(section::UNIT_TEXT_OFF, u64::from(unit))?;
            let end = self.u64_at(section::UNIT_TEXT_OFF, u64::from(unit) + 1)?;
            let text = self.raw_blob(section::TEXT_BLOB, begin, end)?;
            for gram in text.windows(2) {
                let key = k2(gram[0], gram[1]);
                let word = usize::from(key >> 6);
                let bit = 1u64 << (key & 63);
                if seen[word] & bit == 0 {
                    if seen[word] == 0 {
                        touched.push(word);
                    }
                    seen[word] |= bit;
                    pairs.push((u32::from(key) << 16) | unit);
                }
            }
            for &word in &touched {
                seen[word] = 0;
            }
            touched.clear();
        }
        build_q2_prototype_from_pairs(self.unit_count, pairs)
    }

    fn serialize_q2_sidecar(&self, prototype: &Q2PrototypeSegment) -> Result<Vec<u8>> {
        serialize_q2_sidecar_parts(self.unit_count, self.stored_checksum()?, prototype)
    }

    fn search_q2_prototype_segment(
        &self,
        prototype: &Q2PrototypeSegment,
        key: u16,
    ) -> Result<Vec<u32>> {
        if prototype.unit_count != self.unit_count {
            return Err(SearchError::Format("q2 prototype segment mismatch".into()));
        }
        let Ok(record_index) = prototype
            .records
            .binary_search_by_key(&key, |record| record.key)
        else {
            return Ok(Vec::new());
        };
        let record = prototype.records[record_index];
        let begin = usize::try_from(record.offset)
            .map_err(|_| SearchError::Format("q2 prototype offset too large".into()))?;
        let end = begin
            .checked_add(
                usize::try_from(record.bytes)
                    .map_err(|_| SearchError::Format("q2 prototype size too large".into()))?,
            )
            .ok_or_else(|| SearchError::Format("q2 prototype range overflow".into()))?;
        let bytes = prototype
            .payload
            .get(begin..end)
            .ok_or_else(|| SearchError::Format("q2 prototype payload out of bounds".into()))?;
        let mut out = Vec::new();
        if record.dense {
            for (byte_index, &byte) in bytes.iter().enumerate() {
                let mut bits = byte;
                while bits != 0 {
                    let bit = bits.trailing_zeros() as usize;
                    let unit = byte_index * 8 + bit;
                    if unit < self.unit_count as usize {
                        let unit = unit as u32;
                        let docs_begin = self.u64_at(section::UNIT_DOC_OFF, u64::from(unit))?;
                        let docs_end = self.u64_at(section::UNIT_DOC_OFF, u64::from(unit) + 1)?;
                        for doc_index in docs_begin..docs_end {
                            out.push(self.doc_base + self.u32_at(section::UNIT_DOCS, doc_index)?);
                        }
                    }
                    bits &= bits - 1;
                }
            }
        } else {
            if bytes.len() % 2 != 0 {
                return Err(SearchError::Format(
                    "bad sparse q2 prototype payload".into(),
                ));
            }
            for encoded in bytes.chunks_exact(2) {
                let unit = u32::from(u16::from_le_bytes([encoded[0], encoded[1]]));
                let docs_begin = self.u64_at(section::UNIT_DOC_OFF, u64::from(unit))?;
                let docs_end = self.u64_at(section::UNIT_DOC_OFF, u64::from(unit) + 1)?;
                for doc_index in docs_begin..docs_end {
                    out.push(self.doc_base + self.u32_at(section::UNIT_DOCS, doc_index)?);
                }
            }
        }
        debug_assert_eq!(
            record.count as usize,
            if record.dense {
                bytes.iter().map(|byte| byte.count_ones() as usize).sum()
            } else {
                bytes.len() / 2
            }
        );
        out.sort_unstable();
        Ok(out)
    }

    fn scan_content(&self, query: &[u8]) -> Result<Vec<u32>> {
        let query = fold_ascii(query);
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let matcher = ExactMatcher::new(&query);
        let unit_text_off = self.section(section::UNIT_TEXT_OFF)?;
        let text_blob = self.section(section::TEXT_BLOB)?;
        let unit_doc_off = self.section(section::UNIT_DOC_OFF)?;
        let unit_docs = self.section(section::UNIT_DOCS)?;
        let mut out = Vec::new();
        for unit in 0..self.unit_count as usize {
            let begin = rd64(unit_text_off, unit * 8)?;
            let end = rd64(unit_text_off, (unit + 1) * 8)?;
            let begin_usize = usize::try_from(begin)
                .map_err(|_| SearchError::Format("text offset too large".into()))?;
            let end_usize = usize::try_from(end)
                .map_err(|_| SearchError::Format("text offset too large".into()))?;
            let text = text_blob
                .get(begin_usize..end_usize)
                .ok_or_else(|| SearchError::Format("text range out of bounds".into()))?;
            if !matcher.contains(text) {
                continue;
            }
            let docs_begin = rd64(unit_doc_off, unit * 8)?;
            let docs_end = rd64(unit_doc_off, (unit + 1) * 8)?;
            for doc_index in docs_begin..docs_end {
                let off = usize::try_from(doc_index)
                    .map_err(|_| SearchError::Format("content doc index too large".into()))?
                    .checked_mul(4)
                    .ok_or_else(|| SearchError::Format("content doc offset overflow".into()))?;
                out.push(self.doc_base + rd32(unit_docs, off)?);
            }
        }
        out.sort_unstable();
        Ok(out)
    }

    pub fn search_content(&self, query: &[u8]) -> Result<Vec<u32>> {
        let query = fold_ascii(query);
        if query.is_empty() {
            return Ok(Vec::new());
        }
        // The q1 presence mask is exact for a one-byte query. Expand matching units
        // directly to docs so this extremely common hot path avoids a candidate Vec.
        if query.len() == 1 {
            let mask = self.section(section::CQ1MASK)?;
            let unit_doc_off = self.section(section::UNIT_DOC_OFF)?;
            let unit_docs = self.section(section::UNIT_DOCS)?;
            let byte = query[0];
            let mask_offset = usize::from(byte / 8);
            let bit = 1u8 << (byte % 8);
            let mut out = Vec::new();
            for (unit, row) in mask.chunks_exact(32).enumerate() {
                if row[mask_offset] & bit == 0 {
                    continue;
                }
                let begin = rd64(unit_doc_off, unit * 8)?;
                let end = rd64(unit_doc_off, (unit + 1) * 8)?;
                for doc_index in begin..end {
                    let doc_offset = usize::try_from(doc_index)
                        .map_err(|_| SearchError::Format("content doc index too large".into()))?
                        .checked_mul(4)
                        .ok_or_else(|| SearchError::Format("content doc offset overflow".into()))?;
                    out.push(self.doc_base + rd32(unit_docs, doc_offset)?);
                }
            }
            out.sort_unstable();
            return Ok(out);
        }

        let candidates = self.content_candidates(&query)?;
        let exact_gram = query.len() == 3;
        let matcher = (!exact_gram).then(|| ExactMatcher::new(&query));
        let mut out = Vec::new();
        for unit in candidates {
            if let Some(matcher) = &matcher
                && !self.unit_contains(unit, matcher)?
            {
                continue;
            }
            let begin = self.u64_at(section::UNIT_DOC_OFF, u64::from(unit))?;
            let end = self.u64_at(section::UNIT_DOC_OFF, u64::from(unit) + 1)?;
            for index in begin..end {
                out.push(self.doc_base + self.u32_at(section::UNIT_DOCS, index)?);
            }
        }
        out.sort_unstable();
        Ok(out)
    }

    pub fn search_name(&self, query: &[u8]) -> Result<Vec<u32>> {
        let query = fold_ascii(query);
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let candidates = self.name_candidates(&query)?;
        let exact_gram = query.len() <= 3;
        let matcher = (!exact_gram).then(|| ExactMatcher::new(&query));
        let mut out = Vec::new();
        for doc in candidates {
            if let Some(matcher) = &matcher
                && !self.name_contains(doc, matcher)?
            {
                continue;
            }
            out.push(self.doc_base + doc);
        }
        out.sort_unstable();
        Ok(out)
    }

    pub fn first_n(&self, query: &[u8], names: bool, limit: usize) -> Result<Vec<u32>> {
        let query = fold_ascii(query);
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let matcher = ExactMatcher::new(&query);
        let mut out = Vec::with_capacity(limit.min(self.doc_count as usize));
        for doc in 0..self.doc_count {
            let hit = if names {
                self.name_contains(doc, &matcher)?
            } else {
                let unit = self.u32_at(section::DOC_UNIT, u64::from(doc))?;
                self.unit_contains(unit, &matcher)?
            };
            if hit {
                out.push(self.doc_base + doc);
                if out.len() == limit {
                    break;
                }
            }
        }
        Ok(out)
    }

    fn diagnose_content(&self, query: &[u8]) -> Result<ContentSearchDiagnostics> {
        let query = fold_ascii(query);
        if query.is_empty() {
            return Ok(ContentSearchDiagnostics::default());
        }
        let matcher = ExactMatcher::new(&query);
        let candidate_started = Instant::now();
        let candidates = self.content_candidates(&query)?;
        let candidate_ns = candidate_started.elapsed().as_nanos();

        let mut best_q3_candidates = 0u64;
        let mut second_q3_candidates = 0u64;
        let mut best2_q3_intersection = 0u64;
        if query.len() >= 4 {
            let mut metas = Vec::<Q3Meta>::new();
            for gram in query.windows(3) {
                let Some(meta) = self.content_q3_meta(k3(gram[0], gram[1], gram[2]))? else {
                    metas.clear();
                    break;
                };
                metas.push(meta);
            }
            metas.sort_unstable_by_key(|meta| meta.count);
            if let Some(best) = metas.first().copied() {
                best_q3_candidates = u64::from(best.count);
            }
            if let Some(second) = metas.get(1).copied() {
                second_q3_candidates = u64::from(second.count);
                let left = self.decode_content_q3(metas[0])?;
                let right = self.decode_content_q3(second)?;
                let mut li = 0usize;
                let mut ri = 0usize;
                while li < left.len() && ri < right.len() {
                    match left[li].cmp(&right[ri]) {
                        CmpOrdering::Less => li += 1,
                        CmpOrdering::Greater => ri += 1,
                        CmpOrdering::Equal => {
                            best2_q3_intersection += 1;
                            li += 1;
                            ri += 1;
                        }
                    }
                }
            }
        }

        let verify_started = Instant::now();
        let mut matched_units = 0u64;
        let mut matching_units = Vec::new();
        for &unit in &candidates {
            if self.unit_contains(unit, &matcher)? {
                matched_units += 1;
                matching_units.push(unit);
            }
        }
        let verify_ns = verify_started.elapsed().as_nanos();

        let expand_started = Instant::now();
        let mut hit_docs = 0u64;
        for unit in matching_units {
            let begin = self.u64_at(section::UNIT_DOC_OFF, u64::from(unit))?;
            let end = self.u64_at(section::UNIT_DOC_OFF, u64::from(unit) + 1)?;
            hit_docs = hit_docs
                .checked_add(end - begin)
                .ok_or_else(|| SearchError::Format("diagnostic hit count overflow".into()))?;
        }
        let expand_ns = expand_started.elapsed().as_nanos();

        Ok(ContentSearchDiagnostics {
            segments: 1,
            candidates: candidates.len() as u64,
            matched_units,
            hit_docs,
            candidate_ns,
            verify_ns,
            expand_ns,
            best_q3_candidates,
            second_q3_candidates,
            best2_q3_intersection,
        })
    }

    pub fn q3_encoding_count(&self, encoding: Q3Encoding) -> Result<u64> {
        let directory = self.section(section::CQ3DIR)?;
        let entries = usize::try_from(rd32(directory, 256 * 4)?)
            .map_err(|_| SearchError::Format("q3 entry count too large".into()))?;
        let records = 257 * 4;
        let mut count = 0u64;
        for index in 0..entries {
            if Q3Encoding::from_packed(rd32(directory, records + index * 10 + 2)?)? == encoding {
                count += 1;
            }
        }
        Ok(count)
    }
}

#[derive(Debug)]
struct ManifestSegment {
    file: String,
    base: u32,
    count: u32,
    bytes: u64,
}

type SegmentQueryResult = Option<Result<Vec<u32>>>;

fn merge_ordered_segment_results(
    results: Vec<SegmentQueryResult>,
    missing_segment: &'static str,
) -> Result<Vec<u32>> {
    let capacity = results
        .iter()
        .filter_map(Option::as_ref)
        .filter_map(|result| result.as_ref().ok())
        .try_fold(0usize, |total, hits| {
            total
                .checked_add(hits.len())
                .ok_or_else(|| SearchError::Format("segment result count overflow".into()))
        })?;
    let mut out = Vec::with_capacity(capacity);
    for result in results {
        out.extend(result.ok_or_else(|| SearchError::Format(missing_segment.into()))??);
    }
    Ok(out)
}

struct PoolQueryJob {
    query: Arc<[u8]>,
    active_workers: usize,
    next: AtomicUsize,
    remaining: AtomicUsize,
    results: Mutex<Vec<SegmentQueryResult>>,
    done_lock: Mutex<()>,
    done_cv: Condvar,
}

struct PoolCoordinatorState {
    generation: u64,
    job: Option<Arc<PoolQueryJob>>,
}

struct PoolCoordinator {
    state: Mutex<PoolCoordinatorState>,
    cv: Condvar,
    shutdown: std::sync::atomic::AtomicBool,
}

/// Long-lived query worker pool for a lazy persistent generation.
///
/// The normal `LazyPersistentIndex` path remains available; this wrapper exists to
/// amortize OS thread creation for repeated common exhaustive queries. A single
/// query is submitted at a time, matching PersonalRag's Latest-Wins UI contract.
pub struct PooledLazyPersistentIndex {
    index: Arc<LazyPersistentIndex>,
    coordinator: Arc<PoolCoordinator>,
    threads: Vec<std::thread::JoinHandle<()>>,
    submit_lock: Mutex<()>,
}

impl PooledLazyPersistentIndex {
    pub fn open(root: impl AsRef<Path>, workers: usize) -> Result<Self> {
        let index = Arc::new(LazyPersistentIndex::open(root)?);
        let worker_count = workers
            .max(1)
            .min(std::thread::available_parallelism().map_or(1, usize::from))
            .min(index.entries.len().max(1));
        let coordinator = Arc::new(PoolCoordinator {
            state: Mutex::new(PoolCoordinatorState {
                generation: 0,
                job: None,
            }),
            cv: Condvar::new(),
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
                            .expect("query pool coordinator poisoned");
                        while state.generation == seen_generation
                            && !coordinator
                                .shutdown
                                .load(std::sync::atomic::Ordering::Acquire)
                        {
                            state = coordinator
                                .cv
                                .wait(state)
                                .expect("query pool coordinator poisoned");
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
                        let segment_index = job.next.fetch_add(1, AtomicOrdering::Relaxed);
                        if segment_index >= index.entries.len() {
                            break;
                        }
                        let result =
                            index.search_segment_content(segment_index, job.query.as_ref());
                        job.results
                            .lock()
                            .expect("query pool result mutex poisoned")[segment_index] =
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
    pub fn index(&self) -> &LazyPersistentIndex {
        &self.index
    }

    #[inline]
    pub fn search_content(&self, query: impl AsRef<[u8]>) -> Result<Vec<u32>> {
        let query = query.as_ref();
        // Preserve the pre-PRPOS ultra-hot q1/q2/q3 path as a small early-return function body.
        // Positional/scan dispatch is kept out-of-line so adding optional long-query accelerators
        // does not perturb the instruction layout of short interactive queries.
        if query.len() <= 3 {
            let plan = self.index.plan_content_query(query, None)?;
            let workers = plan.workers.min(self.threads.len().max(1));
            return self.search_content_with_workers(query, workers);
        }
        self.search_long_content(query)
    }

    #[inline(never)]
    fn search_long_content(&self, query: &[u8]) -> Result<Vec<u32>> {
        let plan = self.index.plan_content_query(query, None)?;
        match plan.mode {
            ContentPlanMode::AdaptiveGramDriven => self
                .index
                .adaptive_content_with_workers(query, plan.workers),
            ContentPlanMode::VariableGramDriven => self
                .index
                .variable_content_with_workers(query, plan.workers),
            ContentPlanMode::PositionalDriven => self
                .index
                .positional_content_with_workers(query, plan.workers),
            ContentPlanMode::ScanDriven => {
                self.index.scan_content_with_workers(query, plan.workers)
            }
            ContentPlanMode::CandidateDriven | ContentPlanMode::OrderDriven => {
                self.index.search_content_with_workers(query, plan.workers)
            }
        }
    }

    pub fn search_content_with_workers(
        &self,
        query: impl AsRef<[u8]>,
        workers: usize,
    ) -> Result<Vec<u32>> {
        let query = query.as_ref();
        let workers = workers
            .max(1)
            .min(self.threads.len().max(1))
            .min(self.index.entries.len().max(1));
        if workers <= 1 || self.index.entries.len() <= 1 {
            return self.index.search_content_with_workers(query, 1);
        }
        let _submit = self
            .submit_lock
            .lock()
            .map_err(|_| SearchError::Format("query pool submit mutex poisoned".into()))?;
        let job = Arc::new(PoolQueryJob {
            query: Arc::<[u8]>::from(query),
            active_workers: workers,
            next: AtomicUsize::new(0),
            remaining: AtomicUsize::new(workers),
            results: Mutex::new(
                std::iter::repeat_with(|| None)
                    .take(self.index.entries.len())
                    .collect(),
            ),
            done_lock: Mutex::new(()),
            done_cv: Condvar::new(),
        });
        {
            let mut state = self
                .coordinator
                .state
                .lock()
                .map_err(|_| SearchError::Format("query pool coordinator poisoned".into()))?;
            state.generation = state
                .generation
                .checked_add(1)
                .ok_or_else(|| SearchError::Format("query pool generation overflow".into()))?;
            state.job = Some(Arc::clone(&job));
        }
        self.coordinator.cv.notify_all();
        let mut guard = job
            .done_lock
            .lock()
            .map_err(|_| SearchError::Format("query pool completion mutex poisoned".into()))?;
        while job.remaining.load(AtomicOrdering::Acquire) != 0 {
            guard = job
                .done_cv
                .wait(guard)
                .map_err(|_| SearchError::Format("query pool completion mutex poisoned".into()))?;
        }
        drop(guard);
        let mut results = job
            .results
            .lock()
            .map_err(|_| SearchError::Format("query pool result mutex poisoned".into()))?;
        let mut out = Vec::new();
        for (index, result) in results.iter_mut().enumerate() {
            let hits = result.take().ok_or_else(|| {
                SearchError::Format(format!("query pool segment {index} was not queried"))
            })??;
            out.extend(hits);
        }
        Ok(out)
    }
}

impl Drop for PooledLazyPersistentIndex {
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

/// Persistent application-facing search session. It keeps query workers alive across
/// repeated queries and shares the adaptive planner and optional accelerators with the lazy
/// index. A one-shot CLI can still use `LazyPersistentIndex` to avoid pool startup cost.
/// Persistent application-facing search session. It keeps query workers alive across
/// repeated queries and shares the adaptive planner and optional accelerators with the lazy
/// index. A one-shot CLI can still use `LazyPersistentIndex` to avoid pool startup cost.
pub struct SearchSession {
    pooled: PooledLazyPersistentIndex,
}

impl SearchSession {
    pub fn open(root: impl AsRef<Path>, workers: usize) -> Result<Self> {
        Ok(Self {
            pooled: PooledLazyPersistentIndex::open(root, workers)?,
        })
    }

    #[must_use]
    pub fn index(&self) -> &LazyPersistentIndex {
        self.pooled.index()
    }

    pub fn search_content(&self, query: impl AsRef<[u8]>) -> Result<Vec<u32>> {
        self.pooled.search_content(query)
    }

    pub fn search_name(&self, query: impl AsRef<[u8]>) -> Result<Vec<u32>> {
        self.pooled.index().search_name(query)
    }

    pub fn first_n(&self, query: impl AsRef<[u8]>, names: bool, limit: usize) -> Result<Vec<u32>> {
        let query = query.as_ref();
        if names || query.is_empty() || limit == 0 {
            return self.pooled.index().first_n(query, names, limit);
        }
        let plan = self.pooled.index().plan_content_query(query, Some(limit))?;
        if plan.mode == ContentPlanMode::CandidateDriven {
            let mut hits = self
                .pooled
                .search_content_with_workers(query, plan.workers)?;
            hits.truncate(limit);
            Ok(hits)
        } else {
            self.pooled.index().first_n(query, false, limit)
        }
    }
}

pub struct PersistentIndex {
    root: PathBuf,
    docs: u64,
    segments: Vec<SegmentReader>,
}

impl PersistentIndex {
    pub fn open(root: impl AsRef<Path>, verify_checksum: bool) -> Result<Self> {
        Self::open_with_workers(root, verify_checksum, 1)
    }

    pub fn open_with_workers(
        root: impl AsRef<Path>,
        verify_checksum: bool,
        workers: usize,
    ) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let manifest = parse_manifest(&root.join("manifest.txt"))?;
        validate_manifest_ranges(manifest.0, &manifest.1)?;
        if workers <= 1 || manifest.1.len() <= 1 {
            let mut segments = Vec::with_capacity(manifest.1.len());
            for entry in &manifest.1 {
                segments.push(open_manifest_segment(&root, entry, verify_checksum)?);
            }
            return Ok(Self {
                root,
                docs: manifest.0,
                segments,
            });
        }

        let worker_count = workers.max(1).min(manifest.1.len());
        let next = AtomicUsize::new(0);
        let results: Arc<Mutex<Vec<Option<Result<SegmentReader>>>>> = Arc::new(Mutex::new(
            std::iter::repeat_with(|| None)
                .take(manifest.1.len())
                .collect(),
        ));
        std::thread::scope(|scope| {
            for _ in 0..worker_count {
                let results = Arc::clone(&results);
                let root = &root;
                let entries = &manifest.1;
                let next = &next;
                scope.spawn(move || {
                    loop {
                        let index = next.fetch_add(1, AtomicOrdering::Relaxed);
                        if index >= entries.len() {
                            break;
                        }
                        let result = open_manifest_segment(root, &entries[index], verify_checksum);
                        results.lock().expect("segment open mutex poisoned")[index] = Some(result);
                    }
                });
            }
        });
        let segments = Arc::try_unwrap(results)
            .map_err(|_| SearchError::Format("segment open result ownership leak".into()))?
            .into_inner()
            .map_err(|_| SearchError::Format("segment open result mutex poisoned".into()))?
            .into_iter()
            .enumerate()
            .map(|(index, result)| {
                result
                    .ok_or_else(|| SearchError::Format(format!("segment {index} was not opened")))?
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            root,
            docs: manifest.0,
            segments,
        })
    }

    #[must_use]
    pub const fn docs(&self) -> u64 {
        self.docs
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn search_content(&self, query: impl AsRef<[u8]>) -> Result<Vec<u32>> {
        self.search_content_with_workers(query, 0)
    }

    pub fn search_content_with_workers(
        &self,
        query: impl AsRef<[u8]>,
        workers: usize,
    ) -> Result<Vec<u32>> {
        let query = query.as_ref();
        let workers = if workers == 0 {
            self.recommended_query_workers(query)?
        } else {
            workers
        };
        if workers <= 1 || self.segments.len() <= 1 {
            let mut out = Vec::new();
            for segment in &self.segments {
                out.extend(segment.search_content(query)?);
            }
            return Ok(out);
        }
        let worker_count = workers.max(1).min(self.segments.len());
        let next = AtomicUsize::new(0);
        let results: Arc<Mutex<Vec<SegmentQueryResult>>> = Arc::new(Mutex::new(
            std::iter::repeat_with(|| None)
                .take(self.segments.len())
                .collect(),
        ));
        std::thread::scope(|scope| {
            for _ in 0..worker_count {
                let results = Arc::clone(&results);
                let next = &next;
                let segments = &self.segments;
                scope.spawn(move || {
                    loop {
                        let index = next.fetch_add(1, AtomicOrdering::Relaxed);
                        if index >= segments.len() {
                            break;
                        }
                        let result = segments[index].search_content(query);
                        results.lock().expect("query result mutex poisoned")[index] = Some(result);
                    }
                });
            }
        });
        let mut out = Vec::new();
        for (index, result) in Arc::try_unwrap(results)
            .map_err(|_| SearchError::Format("query result ownership leak".into()))?
            .into_inner()
            .map_err(|_| SearchError::Format("query result mutex poisoned".into()))?
            .into_iter()
            .enumerate()
        {
            let hits = result
                .ok_or_else(|| SearchError::Format(format!("segment {index} was not queried")))??;
            out.extend(hits);
        }
        // Segment ranges are contiguous and each segment returns sorted global ids, so
        // concatenation preserves the same deterministic order without another O(n log n) sort.
        Ok(out)
    }

    pub fn build_q2_prototype(&self) -> Result<Q2PrototypeIndex> {
        let mut segments = Vec::with_capacity(self.segments.len());
        let mut persisted_bytes = 0u64;
        let mut records = 0u64;
        let mut dense_records = 0u64;
        for segment in &self.segments {
            let prototype = segment.build_q2_prototype_segment()?;
            persisted_bytes = persisted_bytes
                .checked_add(prototype.persisted_bytes)
                .ok_or_else(|| SearchError::Format("q2 prototype total size overflow".into()))?;
            records = records
                .checked_add(prototype.records.len() as u64)
                .ok_or_else(|| SearchError::Format("q2 prototype record overflow".into()))?;
            dense_records = dense_records
                .checked_add(
                    prototype
                        .records
                        .iter()
                        .filter(|record| record.dense)
                        .count() as u64,
                )
                .ok_or_else(|| SearchError::Format("q2 dense record overflow".into()))?;
            segments.push(prototype);
        }
        Ok(Q2PrototypeIndex {
            segments,
            persisted_bytes,
            records,
            dense_records,
        })
    }

    pub fn search_content_q2_prototype(
        &self,
        prototype: &Q2PrototypeIndex,
        query: impl AsRef<[u8]>,
    ) -> Result<Vec<u32>> {
        let query = fold_ascii(query.as_ref());
        if query.len() != 2 {
            return Err(SearchError::Format(
                "q2 prototype accepts exactly two query bytes".into(),
            ));
        }
        if prototype.segments.len() != self.segments.len() {
            return Err(SearchError::Format("q2 prototype index mismatch".into()));
        }
        let key = k2(query[0], query[1]);
        let mut out = Vec::new();
        for (segment, q2) in self.segments.iter().zip(&prototype.segments) {
            out.extend(segment.search_q2_prototype_segment(q2, key)?);
        }
        Ok(out)
    }

    fn estimated_content_candidates(&self, query: &[u8]) -> Result<u64> {
        if query.is_empty() || self.segments.is_empty() {
            return Ok(0);
        }
        let sample_segments = self.segments.len().min(2);
        let mut sample_candidates = 0u64;
        let mut sample_units = 0u64;
        for segment in &self.segments[..sample_segments] {
            sample_candidates = sample_candidates
                .checked_add(segment.content_candidate_count(query)?)
                .ok_or_else(|| SearchError::Format("candidate estimate overflow".into()))?;
            sample_units = sample_units
                .checked_add(u64::from(segment.unit_count()))
                .ok_or_else(|| SearchError::Format("candidate universe overflow".into()))?;
        }
        if sample_units == 0 {
            return Ok(0);
        }
        Ok(sample_candidates
            .saturating_mul(self.docs)
            .div_ceil(sample_units))
    }

    fn recommended_query_workers(&self, query: &[u8]) -> Result<usize> {
        if query.is_empty() || self.segments.len() < 4 {
            return Ok(1);
        }
        let available = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(4)
            .min(self.segments.len());
        if available <= 1 {
            return Ok(1);
        }
        let estimated_candidates = self.estimated_content_candidates(query)?;
        // Thread startup is measurable for rare queries. Parallelize only when each worker is
        // expected to receive enough verifier/candidate work to amortize that fixed overhead.
        if estimated_candidates < 8_192 {
            Ok(1)
        } else {
            Ok(available)
        }
    }

    pub fn diagnose_content(&self, query: impl AsRef<[u8]>) -> Result<ContentSearchDiagnostics> {
        let mut diagnostics = ContentSearchDiagnostics::default();
        for segment in &self.segments {
            diagnostics.merge(segment.diagnose_content(query.as_ref())?);
        }
        Ok(diagnostics)
    }

    pub fn search_name(&self, query: impl AsRef<[u8]>) -> Result<Vec<u32>> {
        let query = query.as_ref();
        let mut out = Vec::new();
        for segment in &self.segments {
            out.extend(segment.search_name(query)?);
        }
        out.sort_unstable();
        Ok(out)
    }

    pub fn first_n(&self, query: impl AsRef<[u8]>, names: bool, limit: usize) -> Result<Vec<u32>> {
        let query = query.as_ref();
        if !names && limit != 0 && !query.is_empty() {
            let folded = fold_ascii(query);
            let estimated = self.estimated_content_candidates(&folded)?;
            let candidate_threshold = (limit as u64).saturating_mul(16);
            if estimated <= candidate_threshold {
                let mut hits = self.search_content(&folded)?;
                hits.truncate(limit);
                return Ok(hits);
            }
        }
        let mut out = Vec::with_capacity(limit);
        for segment in &self.segments {
            out.extend(segment.first_n(query, names, limit.saturating_sub(out.len()))?);
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    pub fn document_contains(
        &self,
        physical_doc: u32,
        query: impl AsRef<[u8]>,
        names: bool,
    ) -> Result<bool> {
        let query = fold_ascii(query.as_ref());
        if query.is_empty() {
            return Ok(false);
        }
        let matcher = ExactMatcher::new(&query);
        let (segment, local_doc) = self.locate_document(physical_doc)?;
        segment.document_contains_local(local_doc, &matcher, names)
    }

    pub fn document_bytes(&self, physical_doc: u32) -> Result<(Vec<u8>, Vec<u8>)> {
        let (segment, local_doc) = self.locate_document(physical_doc)?;
        Ok((
            segment.document_name_bytes(local_doc)?.to_vec(),
            segment.document_content_bytes(local_doc)?.to_vec(),
        ))
    }

    fn locate_document(&self, physical_doc: u32) -> Result<(&SegmentReader, u32)> {
        if u64::from(physical_doc) >= self.docs {
            return Err(SearchError::InvalidArgument(
                "physical document id out of bounds".into(),
            ));
        }
        let mut lo = 0usize;
        let mut hi = self.segments.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            let segment = &self.segments[mid];
            let start = segment.doc_base();
            let end = start
                .checked_add(segment.doc_count())
                .ok_or_else(|| SearchError::Format("segment doc range overflow".into()))?;
            if physical_doc < start {
                hi = mid;
            } else if physical_doc >= end {
                lo = mid + 1;
            } else {
                return Ok((segment, physical_doc - start));
            }
        }
        Err(SearchError::Format(
            "physical document not covered by any segment".into(),
        ))
    }
}

pub struct LazyPersistentIndex {
    root: PathBuf,
    docs: u64,
    entries: Vec<ManifestSegment>,
    segments: Vec<OnceLock<SegmentReader>>,
    q2_sidecars: Vec<OnceLock<Option<Q2CompactSidecar>>>,
    q2_complete: bool,
    pos_sidecars: Vec<OnceLock<Option<positional::PosSidecar>>>,
    pos_complete: bool,
    pos2_sidecars: Vec<OnceLock<Option<positional2::Pos2Sidecar>>>,
    pos2_complete: bool,
    pos3_sidecars: Vec<OnceLock<Option<positional3::Pos3Sidecar>>>,
    pos3_complete: bool,
}

impl LazyPersistentIndex {
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let manifest = parse_manifest(&root.join("manifest.txt"))?;
        validate_manifest_ranges(manifest.0, &manifest.1)?;
        let segments = std::iter::repeat_with(OnceLock::new)
            .take(manifest.1.len())
            .collect();
        let q2_complete = !manifest.1.is_empty()
            && manifest
                .1
                .iter()
                .all(|entry| root.join(&entry.file).with_extension("q2c").exists());
        let q2_sidecars = std::iter::repeat_with(OnceLock::new)
            .take(manifest.1.len())
            .collect();
        let pos_complete = !manifest.1.is_empty()
            && manifest.1.iter().all(|entry| {
                root.join(&entry.file)
                    .with_extension(PosCodec::production().sidecar_extension())
                    .exists()
            });
        let pos_sidecars = std::iter::repeat_with(OnceLock::new)
            .take(manifest.1.len())
            .collect();
        let pos2_complete = !manifest.1.is_empty()
            && manifest
                .1
                .iter()
                .all(|entry| root.join(&entry.file).with_extension("pos2").exists());
        let pos2_sidecars = std::iter::repeat_with(OnceLock::new)
            .take(manifest.1.len())
            .collect();
        let pos3_complete = !manifest.1.is_empty()
            && manifest
                .1
                .iter()
                .all(|entry| root.join(&entry.file).with_extension("pos3").exists());
        let pos3_sidecars = std::iter::repeat_with(OnceLock::new)
            .take(manifest.1.len())
            .collect();
        Ok(Self {
            root,
            docs: manifest.0,
            entries: manifest.1,
            segments,
            q2_sidecars,
            q2_complete,
            pos_sidecars,
            pos_complete,
            pos2_sidecars,
            pos2_complete,
            pos3_sidecars,
            pos3_complete,
        })
    }

    #[must_use]
    pub const fn docs(&self) -> u64 {
        self.docs
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn opened_segments(&self) -> usize {
        self.segments
            .iter()
            .filter(|slot| slot.get().is_some())
            .count()
    }

    fn segment(&self, index: usize) -> Result<&SegmentReader> {
        let slot = self
            .segments
            .get(index)
            .ok_or_else(|| SearchError::Format("lazy segment index out of bounds".into()))?;
        if slot.get().is_none() {
            let entry = self
                .entries
                .get(index)
                .ok_or_else(|| SearchError::Format("lazy manifest entry missing".into()))?;
            let reader = open_manifest_segment(&self.root, entry, false)?;
            let _ = slot.set(reader);
        }
        slot.get()
            .ok_or_else(|| SearchError::Format("lazy segment initialization failed".into()))
    }

    fn q2_sidecar(&self, index: usize) -> Result<Option<&Q2CompactSidecar>> {
        let slot = self
            .q2_sidecars
            .get(index)
            .ok_or_else(|| SearchError::Format("lazy q2 sidecar index out of bounds".into()))?;
        if slot.get().is_none() {
            let entry = self
                .entries
                .get(index)
                .ok_or_else(|| SearchError::Format("lazy manifest entry missing".into()))?;
            let path = self.root.join(&entry.file).with_extension("q2c");
            let loaded = if path.exists() {
                let segment = self.segment(index)?;
                Some(Q2CompactSidecar::open(
                    &path,
                    segment.unit_count(),
                    segment.stored_checksum()?,
                )?)
            } else {
                None
            };
            let _ = slot.set(loaded);
        }
        Ok(slot.get().and_then(Option::as_ref))
    }

    fn positional_sidecar(&self, index: usize) -> Result<Option<&positional::PosSidecar>> {
        let slot = self.pos_sidecars.get(index).ok_or_else(|| {
            SearchError::Format("lazy positional sidecar index out of bounds".into())
        })?;
        if slot.get().is_none() {
            let entry = self
                .entries
                .get(index)
                .ok_or_else(|| SearchError::Format("lazy manifest entry missing".into()))?;
            let path = self
                .root
                .join(&entry.file)
                .with_extension(PosCodec::production().sidecar_extension());
            let loaded = if path.exists() {
                let segment = self.segment(index)?;
                Some(positional::PosSidecar::open_fast(&path, segment)?)
            } else {
                None
            };
            let _ = slot.set(loaded);
        }
        Ok(slot.get().and_then(Option::as_ref))
    }

    fn positional2_sidecar(&self, index: usize) -> Result<Option<&positional2::Pos2Sidecar>> {
        let slot = self.pos2_sidecars.get(index).ok_or_else(|| {
            SearchError::Format("lazy positional2 sidecar index out of bounds".into())
        })?;
        if slot.get().is_none() {
            let entry = self
                .entries
                .get(index)
                .ok_or_else(|| SearchError::Format("lazy manifest entry missing".into()))?;
            let segment_path = self.root.join(&entry.file);
            let path = positional2::sidecar2_path(&segment_path);
            let loaded = if path.exists() {
                let segment = self.segment(index)?;
                Some(positional2::Pos2Sidecar::open_fast(&path, segment)?)
            } else {
                None
            };
            let _ = slot.set(loaded);
        }
        Ok(slot.get().and_then(Option::as_ref))
    }

    fn positional2_can_help(&self, query: &[u8]) -> Result<bool> {
        if !self.pos2_complete || query.len() < 4 {
            return Ok(false);
        }
        let samples = self.entries.len().min(2);
        for index in 0..samples {
            if self
                .positional2_sidecar(index)?
                .is_some_and(|side| side.has_exact_variable_record(query).unwrap_or(false))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn positional3_sidecar(&self, index: usize) -> Result<Option<&positional3::Pos3Sidecar>> {
        let slot = self.pos3_sidecars.get(index).ok_or_else(|| {
            SearchError::Format("lazy positional3 sidecar index out of bounds".into())
        })?;
        if slot.get().is_none() {
            let entry = self
                .entries
                .get(index)
                .ok_or_else(|| SearchError::Format("lazy manifest entry missing".into()))?;
            let segment_path = self.root.join(&entry.file);
            let path = positional3::sidecar3_path(&segment_path);
            let loaded = if path.exists() {
                let segment = self.segment(index)?;
                Some(positional3::Pos3Sidecar::open_fast(&path, segment)?)
            } else {
                None
            };
            let _ = slot.set(loaded);
        }
        Ok(slot.get().and_then(Option::as_ref))
    }

    fn positional3_can_help(&self, query: &[u8]) -> Result<bool> {
        if !self.pos3_complete || !(9..=16).contains(&query.len()) || self.entries.is_empty() {
            return Ok(false);
        }
        // Sample the first two segments conservatively. AdaptiveGramDriven is only selected when
        // every sampled segment has the exact variable-gram record; otherwise the query keeps the
        // proven PRPOS001/PRSEG path instead of paying mixed fallback costs on real corpora.
        let samples = self.entries.len().min(2);
        for index in 0..samples {
            if !self
                .positional3_sidecar(index)?
                .is_some_and(|side| side.has_exact_record(query).unwrap_or(false))
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    #[must_use]
    pub fn positional3_sidecars_available(&self) -> usize {
        if self.pos3_complete {
            self.entries.len()
        } else {
            0
        }
    }

    #[must_use]
    pub fn positional_sidecars_available(&self) -> usize {
        if self.pos_complete {
            self.entries.len()
        } else {
            0
        }
    }

    #[must_use]
    pub fn positional2_sidecars_available(&self) -> usize {
        if self.pos2_complete {
            self.entries.len()
        } else {
            0
        }
    }

    #[must_use]
    pub fn q2_sidecars_available(&self) -> usize {
        if self.q2_complete {
            self.entries.len()
        } else {
            0
        }
    }

    pub(crate) fn search_segment_content(&self, index: usize, query: &[u8]) -> Result<Vec<u32>> {
        let segment = self.segment(index)?;
        if query.len() == 2 && self.q2_complete {
            let folded = fold_ascii(query);
            let key = k2(folded[0], folded[1]);
            let sidecar = self
                .q2_sidecar(index)?
                .ok_or_else(|| SearchError::Format("q2 sidecar disappeared".into()))?;
            let units = sidecar.unit_ids(key)?;
            let mut out = Vec::new();
            for unit in units {
                let begin = segment.u64_at(section::UNIT_DOC_OFF, u64::from(unit))?;
                let end = segment.u64_at(section::UNIT_DOC_OFF, u64::from(unit) + 1)?;
                for doc_index in begin..end {
                    out.push(segment.doc_base + segment.u32_at(section::UNIT_DOCS, doc_index)?);
                }
            }
            out.sort_unstable();
            return Ok(out);
        }
        segment.search_content(query)
    }

    pub(crate) fn search_segment_positional_content(
        &self,
        index: usize,
        query: &[u8],
    ) -> Result<Vec<u32>> {
        let segment = self.segment(index)?;
        match self.positional_sidecar(index)? {
            Some(sidecar) => {
                match positional::search_segment_positional(segment, sidecar, query)? {
                    Some(hits) => Ok(hits),
                    None => segment.search_content(query),
                }
            }
            None => segment.search_content(query),
        }
    }

    pub(crate) fn search_segment_adaptive_content(
        &self,
        index: usize,
        query: &[u8],
    ) -> Result<Vec<u32>> {
        let segment = self.segment(index)?;
        if let Some(sidecar) = self.positional3_sidecar(index)?
            && let Some(hits) = positional3::search_segment_positional3(segment, sidecar, query)?
        {
            return Ok(hits);
        }
        if let Some(sidecar) = self.positional2_sidecar(index)?
            && sidecar.has_exact_variable_record(query)?
            && let Some(hits) = positional2::search_segment_positional2(segment, sidecar, query)?
        {
            return Ok(hits);
        }
        if let Some(sidecar) = self.positional_sidecar(index)?
            && let Some(hits) = positional::search_segment_positional(segment, sidecar, query)?
        {
            return Ok(hits);
        }
        segment.search_content(query)
    }

    pub(crate) fn search_segment_variable_content(
        &self,
        index: usize,
        query: &[u8],
    ) -> Result<Vec<u32>> {
        let segment = self.segment(index)?;
        if let Some(sidecar) = self.positional2_sidecar(index)?
            && sidecar.has_exact_variable_record(query)?
            && let Some(hits) = positional2::search_segment_positional2(segment, sidecar, query)?
        {
            return Ok(hits);
        }
        if let Some(sidecar) = self.positional_sidecar(index)?
            && let Some(hits) = positional::search_segment_positional(segment, sidecar, query)?
        {
            return Ok(hits);
        }
        segment.search_content(query)
    }

    pub(crate) fn search_segment_name(&self, index: usize, query: &[u8]) -> Result<Vec<u32>> {
        self.segment(index)?.search_name(query)
    }

    pub(crate) fn segment_count(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn estimated_content_candidates_for_planner(&self, query: &[u8]) -> Result<u64> {
        if query.is_empty() || self.entries.is_empty() {
            return Ok(0);
        }
        let sample_segments = self.entries.len().min(2);
        let mut sample_candidates = 0u64;
        let mut sample_units = 0u64;
        for index in 0..sample_segments {
            let segment = self.segment(index)?;
            sample_candidates = sample_candidates
                .checked_add(segment.content_candidate_count(query)?)
                .ok_or_else(|| SearchError::Format("candidate estimate overflow".into()))?;
            sample_units = sample_units
                .checked_add(u64::from(segment.unit_count()))
                .ok_or_else(|| SearchError::Format("candidate universe overflow".into()))?;
        }
        if sample_units == 0 {
            return Ok(0);
        }
        Ok(sample_candidates
            .saturating_mul(self.docs)
            .div_ceil(sample_units))
    }

    pub fn plan_content_query(
        &self,
        query: impl AsRef<[u8]>,
        limit: Option<usize>,
    ) -> Result<ContentQueryPlan> {
        let query = fold_ascii(query.as_ref());
        if query.is_empty() || self.entries.is_empty() {
            return Ok(ContentQueryPlan {
                mode: ContentPlanMode::CandidateDriven,
                workers: 1,
                estimated_candidates: 0,
                estimated_density_ppm: 0,
                verifier_weight: 0,
            });
        }
        // First-N samples only segment zero so lazy-open TTFR is not sacrificed just to plan.
        let estimated_candidates = if limit.is_some() {
            let sample = self.segment(0)?;
            let sample_units = u64::from(sample.unit_count()).max(1);
            sample
                .content_candidate_count(&query)?
                .saturating_mul(self.docs)
                .div_ceil(sample_units)
        } else {
            self.estimated_content_candidates_for_planner(&query)?
        };
        let estimated_density_ppm = estimated_candidates
            .min(self.docs)
            .saturating_mul(1_000_000)
            .checked_div(self.docs)
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(0);
        let available = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(4)
            .min(self.entries.len().max(1));
        let workers = plan_workers(
            query.len(),
            estimated_candidates,
            self.entries.len(),
            available,
        );
        let mode = if let Some(limit) = limit {
            let threshold = first_n_candidate_threshold(query.len(), limit, self.docs);
            if estimated_candidates <= threshold {
                ContentPlanMode::CandidateDriven
            } else {
                ContentPlanMode::OrderDriven
            }
        } else if (9..=16).contains(&query.len())
            && estimated_density_ppm >= 500_000
            && workers >= 2
            && self.pos3_complete
            && self.positional3_can_help(&query)?
        {
            ContentPlanMode::AdaptiveGramDriven
        } else if query.len() >= 4
            && estimated_density_ppm >= 500_000
            && workers >= 2
            && self.pos2_complete
            && self.positional2_can_help(&query)?
        {
            ContentPlanMode::VariableGramDriven
        } else if query.len() >= 4
            && estimated_density_ppm >= 500_000
            && workers >= 2
            && self.pos_complete
        {
            // Literature-driven PRPOS001 winner: once at least half of content units are
            // candidates, covering positional trigrams eliminate verifier reads and beat the
            // candidate/scan paths on large synthetic and real-source workloads.
            ContentPlanMode::PositionalDriven
        } else if query.len() >= 4 && estimated_density_ppm >= 900_000 && workers >= 4 {
            // If PRPOS001 is unavailable or cannot cover this literal, retain the previous
            // architecture-tournament ScanDriven winner for very-dense exhaustive queries.
            ContentPlanMode::ScanDriven
        } else {
            ContentPlanMode::CandidateDriven
        };
        Ok(ContentQueryPlan {
            mode,
            workers,
            estimated_candidates,
            estimated_density_ppm,
            verifier_weight: verifier_weight(query.len()),
        })
    }

    fn adaptive_content_with_workers(
        &self,
        query: impl AsRef<[u8]>,
        workers: usize,
    ) -> Result<Vec<u32>> {
        let query = fold_ascii(query.as_ref());
        let workers = workers.max(1).min(self.entries.len().max(1));
        if workers <= 1 || self.entries.len() <= 1 {
            let mut out = Vec::new();
            for index in 0..self.entries.len() {
                out.extend(self.search_segment_adaptive_content(index, &query)?);
            }
            return Ok(out);
        }
        let next = AtomicUsize::new(0);
        let results: Arc<Mutex<Vec<SegmentQueryResult>>> = Arc::new(Mutex::new(
            std::iter::repeat_with(|| None)
                .take(self.entries.len())
                .collect(),
        ));
        std::thread::scope(|scope| {
            for _ in 0..workers {
                let next = &next;
                let results = Arc::clone(&results);
                let query = &query;
                scope.spawn(move || {
                    loop {
                        let index = next.fetch_add(1, AtomicOrdering::Relaxed);
                        if index >= self.entries.len() {
                            break;
                        }
                        let result = self.search_segment_adaptive_content(index, query);
                        results.lock().expect("adaptive result mutex poisoned")[index] =
                            Some(result);
                    }
                });
            }
        });
        let results = Arc::try_unwrap(results)
            .map_err(|_| SearchError::Format("adaptive result ownership leak".into()))?
            .into_inner()
            .map_err(|_| SearchError::Format("adaptive result mutex poisoned".into()))?;
        merge_ordered_segment_results(results, "adaptive segment not queried")
    }

    fn variable_content_with_workers(
        &self,
        query: impl AsRef<[u8]>,
        workers: usize,
    ) -> Result<Vec<u32>> {
        let query = fold_ascii(query.as_ref());
        let workers = workers.max(1).min(self.entries.len().max(1));
        if workers <= 1 || self.entries.len() <= 1 {
            let mut out = Vec::new();
            for index in 0..self.entries.len() {
                out.extend(self.search_segment_variable_content(index, &query)?);
            }
            return Ok(out);
        }
        let next = AtomicUsize::new(0);
        let results: Arc<Mutex<Vec<SegmentQueryResult>>> = Arc::new(Mutex::new(
            std::iter::repeat_with(|| None)
                .take(self.entries.len())
                .collect(),
        ));
        std::thread::scope(|scope| {
            for _ in 0..workers {
                let next = &next;
                let results = Arc::clone(&results);
                let query = &query;
                scope.spawn(move || {
                    loop {
                        let index = next.fetch_add(1, AtomicOrdering::Relaxed);
                        if index >= self.entries.len() {
                            break;
                        }
                        let result = self.search_segment_variable_content(index, query);
                        results.lock().expect("variable result mutex poisoned")[index] =
                            Some(result);
                    }
                });
            }
        });
        let results = Arc::try_unwrap(results)
            .map_err(|_| SearchError::Format("variable result ownership leak".into()))?
            .into_inner()
            .map_err(|_| SearchError::Format("variable result mutex poisoned".into()))?;
        merge_ordered_segment_results(results, "variable segment not queried")
    }

    fn positional_content_with_workers(
        &self,
        query: impl AsRef<[u8]>,
        workers: usize,
    ) -> Result<Vec<u32>> {
        let query = fold_ascii(query.as_ref());
        let workers = workers.max(1).min(self.entries.len().max(1));
        if workers <= 1 || self.entries.len() <= 1 {
            let mut out = Vec::new();
            for index in 0..self.entries.len() {
                let segment = self.segment(index)?;
                let hits = match self.positional_sidecar(index)? {
                    Some(sidecar) => {
                        match positional::search_segment_positional(segment, sidecar, &query)? {
                            Some(hits) => hits,
                            None => segment.search_content(&query)?,
                        }
                    }
                    None => segment.search_content(&query)?,
                };
                out.extend(hits);
            }
            return Ok(out);
        }
        let next = AtomicUsize::new(0);
        let results: Arc<Mutex<Vec<SegmentQueryResult>>> = Arc::new(Mutex::new(
            std::iter::repeat_with(|| None)
                .take(self.entries.len())
                .collect(),
        ));
        std::thread::scope(|scope| {
            for _ in 0..workers {
                let next = &next;
                let results = Arc::clone(&results);
                let query = &query;
                scope.spawn(move || {
                    loop {
                        let index = next.fetch_add(1, AtomicOrdering::Relaxed);
                        if index >= self.entries.len() {
                            break;
                        }
                        let result = (|| -> Result<Vec<u32>> {
                            let segment = self.segment(index)?;
                            match self.positional_sidecar(index)? {
                                Some(sidecar) => match positional::search_segment_positional(
                                    segment, sidecar, query,
                                )? {
                                    Some(hits) => Ok(hits),
                                    None => segment.search_content(query),
                                },
                                None => segment.search_content(query),
                            }
                        })();
                        results.lock().expect("positional result mutex poisoned")[index] =
                            Some(result);
                    }
                });
            }
        });
        let results = Arc::try_unwrap(results)
            .map_err(|_| SearchError::Format("positional result ownership leak".into()))?
            .into_inner()
            .map_err(|_| SearchError::Format("positional result mutex poisoned".into()))?;
        merge_ordered_segment_results(results, "positional segment not queried")
    }

    fn scan_content_with_workers(
        &self,
        query: impl AsRef<[u8]>,
        workers: usize,
    ) -> Result<Vec<u32>> {
        let query = query.as_ref();
        let workers = workers.max(1).min(self.entries.len().max(1));
        if workers <= 1 || self.entries.len() <= 1 {
            let mut out = Vec::new();
            for index in 0..self.entries.len() {
                out.extend(self.segment(index)?.scan_content(query)?);
            }
            return Ok(out);
        }
        let next = AtomicUsize::new(0);
        let results: Arc<Mutex<Vec<SegmentQueryResult>>> = Arc::new(Mutex::new(
            std::iter::repeat_with(|| None)
                .take(self.entries.len())
                .collect(),
        ));
        std::thread::scope(|scope| {
            for _ in 0..workers {
                let next = &next;
                let results = Arc::clone(&results);
                scope.spawn(move || {
                    loop {
                        let index = next.fetch_add(1, AtomicOrdering::Relaxed);
                        if index >= self.entries.len() {
                            break;
                        }
                        let result = self
                            .segment(index)
                            .and_then(|segment| segment.scan_content(query));
                        results.lock().expect("scan result mutex poisoned")[index] = Some(result);
                    }
                });
            }
        });
        let mut out = Vec::new();
        for (index, result) in Arc::try_unwrap(results)
            .map_err(|_| SearchError::Format("scan result ownership leak".into()))?
            .into_inner()
            .map_err(|_| SearchError::Format("scan result mutex poisoned".into()))?
            .into_iter()
            .enumerate()
        {
            let hits = result.ok_or_else(|| {
                SearchError::Format(format!("scan segment {index} was not queried"))
            })??;
            out.extend(hits);
        }
        Ok(out)
    }

    pub fn search_content_with_worker_budget(
        &self,
        query: impl AsRef<[u8]>,
        worker_budget: usize,
    ) -> Result<Vec<u32>> {
        let query = query.as_ref();
        let plan = self.plan_content_query(query, None)?;
        let workers = plan.workers.min(worker_budget.max(1));
        match plan.mode {
            ContentPlanMode::AdaptiveGramDriven => {
                self.adaptive_content_with_workers(query, workers)
            }
            ContentPlanMode::VariableGramDriven => {
                self.variable_content_with_workers(query, workers)
            }
            ContentPlanMode::PositionalDriven => {
                self.positional_content_with_workers(query, workers)
            }
            ContentPlanMode::ScanDriven => self.scan_content_with_workers(query, workers),
            ContentPlanMode::CandidateDriven | ContentPlanMode::OrderDriven => {
                self.search_content_with_workers(query, workers)
            }
        }
    }

    pub fn search_content(&self, query: impl AsRef<[u8]>) -> Result<Vec<u32>> {
        self.search_content_with_worker_budget(query, usize::MAX)
    }

    pub fn search_content_with_workers(
        &self,
        query: impl AsRef<[u8]>,
        workers: usize,
    ) -> Result<Vec<u32>> {
        let query = query.as_ref();
        let workers = workers.max(1).min(self.entries.len().max(1));
        if workers <= 1 || self.entries.len() <= 1 {
            let mut out = Vec::new();
            for index in 0..self.entries.len() {
                out.extend(self.search_segment_content(index, query)?);
            }
            return Ok(out);
        }
        let next = AtomicUsize::new(0);
        let results: Arc<Mutex<Vec<SegmentQueryResult>>> = Arc::new(Mutex::new(
            std::iter::repeat_with(|| None)
                .take(self.entries.len())
                .collect(),
        ));
        std::thread::scope(|scope| {
            for _ in 0..workers {
                let next = &next;
                let results = Arc::clone(&results);
                scope.spawn(move || {
                    loop {
                        let index = next.fetch_add(1, AtomicOrdering::Relaxed);
                        if index >= self.entries.len() {
                            break;
                        }
                        let result = self.search_segment_content(index, query);
                        results.lock().expect("lazy query result mutex poisoned")[index] =
                            Some(result);
                    }
                });
            }
        });
        let mut out = Vec::new();
        for (index, result) in Arc::try_unwrap(results)
            .map_err(|_| SearchError::Format("lazy query result ownership leak".into()))?
            .into_inner()
            .map_err(|_| SearchError::Format("lazy query result mutex poisoned".into()))?
            .into_iter()
            .enumerate()
        {
            let hits = result.ok_or_else(|| {
                SearchError::Format(format!("lazy segment {index} was not queried"))
            })??;
            out.extend(hits);
        }
        Ok(out)
    }

    pub fn search_name(&self, query: impl AsRef<[u8]>) -> Result<Vec<u32>> {
        let query = query.as_ref();
        let mut out = Vec::new();
        for index in 0..self.entries.len() {
            out.extend(self.segment(index)?.search_name(query)?);
        }
        Ok(out)
    }

    pub fn first_n(&self, query: impl AsRef<[u8]>, names: bool, limit: usize) -> Result<Vec<u32>> {
        let query = query.as_ref();
        if !names && limit != 0 && !query.is_empty() && !self.entries.is_empty() {
            let folded = fold_ascii(query);
            let plan = self.plan_content_query(&folded, Some(limit))?;
            if plan.mode == ContentPlanMode::CandidateDriven {
                let mut hits = self.search_content_with_workers(&folded, plan.workers)?;
                hits.truncate(limit);
                return Ok(hits);
            }
        }
        let mut out = Vec::with_capacity(limit);
        for index in 0..self.entries.len() {
            out.extend(self.segment(index)?.first_n(
                query,
                names,
                limit.saturating_sub(out.len()),
            )?);
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    pub fn document_contains(
        &self,
        physical_doc: u32,
        query: impl AsRef<[u8]>,
        names: bool,
    ) -> Result<bool> {
        let query = fold_ascii(query.as_ref());
        if query.is_empty() {
            return Ok(false);
        }
        let matcher = ExactMatcher::new(&query);
        let (segment, local_doc) = self.locate_document(physical_doc)?;
        segment.document_contains_local(local_doc, &matcher, names)
    }

    pub fn document_bytes(&self, physical_doc: u32) -> Result<(Vec<u8>, Vec<u8>)> {
        let (segment, local_doc) = self.locate_document(physical_doc)?;
        Ok((
            segment.document_name_bytes(local_doc)?.to_vec(),
            segment.document_content_bytes(local_doc)?.to_vec(),
        ))
    }

    fn locate_document(&self, physical_doc: u32) -> Result<(&SegmentReader, u32)> {
        if u64::from(physical_doc) >= self.docs {
            return Err(SearchError::InvalidArgument(
                "physical document id out of bounds".into(),
            ));
        }
        let mut lo = 0usize;
        let mut hi = self.entries.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            let entry = &self.entries[mid];
            let start = entry.base;
            let end = start
                .checked_add(entry.count)
                .ok_or_else(|| SearchError::Format("segment doc range overflow".into()))?;
            if physical_doc < start {
                hi = mid;
            } else if physical_doc >= end {
                lo = mid + 1;
            } else {
                return Ok((self.segment(mid)?, physical_doc - start));
            }
        }
        Err(SearchError::Format(
            "physical document not covered by any segment".into(),
        ))
    }
}

fn validate_manifest_ranges(docs: u64, entries: &[ManifestSegment]) -> Result<()> {
    let mut expected_base = 0u64;
    for entry in entries {
        if u64::from(entry.base) != expected_base {
            return Err(SearchError::Format(
                "manifest document ranges are not contiguous".into(),
            ));
        }
        let relative = Path::new(&entry.file);
        if relative.is_absolute() || relative.components().count() != 1 {
            return Err(SearchError::Format("unsafe manifest segment path".into()));
        }
        expected_base = expected_base
            .checked_add(u64::from(entry.count))
            .ok_or_else(|| SearchError::Format("manifest document count overflow".into()))?;
    }
    if expected_base != docs {
        return Err(SearchError::Format("manifest docs/ranges mismatch".into()));
    }
    Ok(())
}

fn open_manifest_segment(
    root: &Path,
    entry: &ManifestSegment,
    verify_checksum: bool,
) -> Result<SegmentReader> {
    let relative = Path::new(&entry.file);
    let reader = SegmentReader::open(root.join(relative), verify_checksum)?;
    if reader.doc_base() != entry.base
        || reader.doc_count() != entry.count
        || fs::metadata(reader.path())?.len() != entry.bytes
    {
        return Err(SearchError::Format(format!(
            "manifest/segment mismatch for {}",
            entry.file
        )));
    }
    Ok(reader)
}

fn parse_manifest(path: &Path) -> Result<(u64, Vec<ManifestSegment>)> {
    let text = fs::read_to_string(path)?;
    let mut lines = text.lines();
    if lines.next() != Some(MANIFEST_MAGIC) {
        return Err(SearchError::Format("bad manifest magic".into()));
    }
    let mut docs = None;
    let mut declared_segments = None;
    let mut segments = Vec::new();
    for line in lines {
        let mut fields = line.split_whitespace();
        match fields.next() {
            Some("mode") => {}
            Some("docs") => {
                docs = Some(parse_field(fields.next(), "docs")?);
            }
            Some("segments") => {
                declared_segments = Some(parse_field::<usize>(fields.next(), "segments")?);
            }
            Some("segment") => {
                let file = fields
                    .next()
                    .ok_or_else(|| SearchError::Format("manifest segment filename missing".into()))?
                    .to_owned();
                let base = parse_field(fields.next(), "segment base")?;
                let count = parse_field(fields.next(), "segment count")?;
                let _kind = fields
                    .next()
                    .ok_or_else(|| SearchError::Format("segment kind missing".into()))?;
                let _duplicate_ratio = fields
                    .next()
                    .ok_or_else(|| SearchError::Format("duplicate ratio missing".into()))?;
                let _run_unique_ratio = fields
                    .next()
                    .ok_or_else(|| SearchError::Format("run ratio missing".into()))?;
                let bytes = parse_field(fields.next(), "segment bytes")?;
                segments.push(ManifestSegment {
                    file,
                    base,
                    count,
                    bytes,
                });
            }
            Some(tag) => return Err(SearchError::Format(format!("unknown manifest tag {tag}"))),
            None => {}
        }
    }
    let docs = docs.ok_or_else(|| SearchError::Format("manifest docs missing".into()))?;
    if declared_segments != Some(segments.len()) {
        return Err(SearchError::Format(
            "manifest segment count mismatch".into(),
        ));
    }
    Ok((docs, segments))
}

fn parse_field<T: std::str::FromStr>(value: Option<&str>, name: &str) -> Result<T> {
    value
        .ok_or_else(|| SearchError::Format(format!("{name} missing")))?
        .parse()
        .map_err(|_| SearchError::Format(format!("invalid {name}")))
}

fn build_one_q2_sidecar(
    root: &Path,
    entry: &ManifestSegment,
    segment_index: usize,
    durable: bool,
    _directory_sync_lock: &Mutex<()>,
) -> Result<(u64, u64, u64)> {
    let segment_path = root.join(&entry.file);
    let segment = SegmentReader::open(&segment_path, false)?;
    let prototype = segment.build_q2_prototype_segment()?;
    let bytes = segment.serialize_q2_sidecar(&prototype)?;
    let path = segment_path.with_extension("q2c");

    // Immutable accelerators are published once. If one already exists, validate that it is
    // bound to the same segment and leave it untouched; a corrupt/stale sidecar fails closed
    // instead of being silently replaced.
    if path.exists() {
        let _ = Q2CompactSidecar::open(&path, segment.unit_count(), segment.stored_checksum()?)?;
    } else {
        let temp = path.with_extension(format!("q2c.tmp-{}-{segment_index}", std::process::id()));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        let publish_result = (|| -> Result<()> {
            file.write_all(&bytes)?;
            if durable {
                file.sync_all()?;
            }
            drop(file);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&temp, fs::Permissions::from_mode(0o444))?;
            }
            fs::rename(&temp, &path)?;
            if durable {
                #[cfg(unix)]
                {
                    let _guard = _directory_sync_lock.lock().map_err(|_| {
                        SearchError::Format("q2 sidecar directory sync mutex poisoned".into())
                    })?;
                    fs::File::open(root)?.sync_all()?;
                }
            }
            Ok(())
        })();
        if publish_result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        publish_result?;
    }

    Ok((
        bytes.len() as u64,
        prototype.records.len() as u64,
        prototype
            .records
            .iter()
            .filter(|record| record.dense)
            .count() as u64,
    ))
}

pub fn build_q2_sidecars(root: impl AsRef<Path>, durable: bool) -> Result<Q2SidecarBuildReport> {
    let root = root.as_ref();
    let manifest = parse_manifest(&root.join("manifest.txt"))?;
    validate_manifest_ranges(manifest.0, &manifest.1)?;
    if manifest.1.is_empty() {
        return Ok(Q2SidecarBuildReport::default());
    }

    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(4)
        .min(manifest.1.len());
    let next = AtomicUsize::new(0);
    // File payloads can be written and synced independently, but concurrent fsync on the same
    // directory caused severe writeback stalls on large builds. Serialize only the directory
    // durability barrier while keeping segment computation and file I/O parallel.
    let directory_sync_lock = Mutex::new(());
    type BuildResult = Option<Result<(u64, u64, u64)>>;
    let results: Arc<Mutex<Vec<BuildResult>>> = Arc::new(Mutex::new(
        std::iter::repeat_with(|| None)
            .take(manifest.1.len())
            .collect(),
    ));

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let next = &next;
            let results = Arc::clone(&results);
            let entries = &manifest.1;
            let directory_sync_lock = &directory_sync_lock;
            scope.spawn(move || {
                loop {
                    let index = next.fetch_add(1, AtomicOrdering::Relaxed);
                    if index >= entries.len() {
                        break;
                    }
                    let result = build_one_q2_sidecar(
                        root,
                        &entries[index],
                        index,
                        durable,
                        directory_sync_lock,
                    );
                    results
                        .lock()
                        .expect("q2 sidecar build result mutex poisoned")[index] = Some(result);
                }
            });
        }
    });

    let mut report = Q2SidecarBuildReport::default();
    for result in Arc::try_unwrap(results)
        .map_err(|_| SearchError::Format("q2 sidecar build result ownership leak".into()))?
        .into_inner()
        .map_err(|_| SearchError::Format("q2 sidecar build result mutex poisoned".into()))?
    {
        let (bytes, records, dense_records) = result
            .ok_or_else(|| SearchError::Format("q2 sidecar segment was not built".into()))??;
        report.segments += 1;
        report.bytes = report
            .bytes
            .checked_add(bytes)
            .ok_or_else(|| SearchError::Format("q2 sidecar report byte overflow".into()))?;
        report.records = report
            .records
            .checked_add(records)
            .ok_or_else(|| SearchError::Format("q2 sidecar report record overflow".into()))?;
        report.dense_records = report
            .dense_records
            .checked_add(dense_records)
            .ok_or_else(|| SearchError::Format("q2 sidecar report dense overflow".into()))?;
    }
    Ok(report)
}

pub fn verify_index(root: impl AsRef<Path>) -> Result<()> {
    let workers = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(4);
    let _ = PersistentIndex::open_with_workers(root, true, workers)?;
    Ok(())
}

#[cfg(test)]
mod exact_matcher_simd_tests {
    use super::ExactMatcher;

    #[derive(Clone, Copy)]
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0
        }
    }

    fn naive_find(haystack: &[u8], needle: &[u8], start: usize) -> Option<usize> {
        if needle.is_empty() {
            return (start <= haystack.len()).then_some(start);
        }
        if start > haystack.len() || haystack.len().saturating_sub(start) < needle.len() {
            return None;
        }
        haystack[start..]
            .windows(needle.len())
            .position(|window| window == needle)
            .map(|offset| start + offset)
    }

    #[test]
    fn find_from_matches_naive_across_simd_boundaries_and_start_offsets() {
        let mut haystack = vec![b'x'; 320];
        for &position in &[0usize, 1, 30, 31, 32, 33, 63, 64, 127, 191, 287] {
            haystack[position..position + 7].copy_from_slice(b"timeout");
        }
        let matcher = ExactMatcher::new(b"timeout");
        for start in 0..=haystack.len() + 1 {
            assert_eq!(
                matcher.find_from(&haystack, start),
                naive_find(&haystack, b"timeout", start),
                "start={start}"
            );
        }
    }

    #[test]
    fn find_from_profiled_matches_naive_for_all_simd_needle_lengths_and_boundaries() {
        for needle_len in 2usize..=8 {
            let needle = (0..needle_len)
                .map(|index| b'a' + u8::try_from(index).expect("small needle"))
                .collect::<Vec<_>>();
            let mut haystack = vec![b'z'; 256];
            for &position in &[0usize, 1, 30, 31, 32, 33, 62, 63, 64, 127, 191] {
                if position + needle_len <= haystack.len() {
                    haystack[position..position + needle_len].copy_from_slice(&needle);
                }
            }
            let matcher = ExactMatcher::new(&needle);
            for start in 0..=haystack.len() + 1 {
                let expected = naive_find(&haystack, &needle, start);
                assert_eq!(matcher.find_from(&haystack, start), expected);
                assert_eq!(matcher.find_from_profiled(&haystack, start).0, expected);
            }
        }
    }

    #[test]
    fn find_from_matches_naive_for_random_binary_data() {
        let mut rng = Rng(0x5349_4d44_5f51_5545);
        for haystack_len in [0usize, 1, 2, 7, 31, 32, 33, 63, 64, 65, 127, 255, 511] {
            let mut haystack = vec![0u8; haystack_len];
            for byte in &mut haystack {
                *byte = (rng.next() >> 24) as u8;
            }
            for needle_len in [0usize, 1, 2, 3, 4, 7, 15, 31, 47] {
                if needle_len > 0 && needle_len <= haystack_len && rng.next() & 1 == 0 {
                    let max = haystack_len - needle_len;
                    let pos = (rng.next() as usize) % (max + 1);
                    let needle = haystack[pos..pos + needle_len].to_vec();
                    let matcher = ExactMatcher::new(&needle);
                    for start in [0usize, pos.saturating_sub(1), pos, pos + 1, haystack_len] {
                        assert_eq!(
                            matcher.find_from(&haystack, start),
                            naive_find(&haystack, &needle, start),
                            "haystack_len={haystack_len} needle_len={needle_len} start={start}"
                        );
                    }
                } else {
                    let mut needle = vec![0u8; needle_len];
                    for byte in &mut needle {
                        *byte = (rng.next() >> 32) as u8;
                    }
                    let matcher = ExactMatcher::new(&needle);
                    for start in [0usize, haystack_len / 2, haystack_len, haystack_len + 1] {
                        assert_eq!(
                            matcher.find_from(&haystack, start),
                            naive_find(&haystack, &needle, start),
                            "haystack_len={haystack_len} needle_len={needle_len} start={start}"
                        );
                    }
                }
            }
        }
    }
}
