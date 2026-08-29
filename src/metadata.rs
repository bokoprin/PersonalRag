use super::{
    AdaptiveGlobalPresence, GLOBAL_TRIGRAM_WORDS, TRIGRAM_UNIVERSE, higher_key,
    is_byte_preserving_fast_path, k3, k4, k5, normalize_str, stable_mix64,
};
use crate::persistent::crc64_ecma;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const METADATA_INDEX_MAGIC: &[u8; 8] = b"PRV2MET1";
pub const METADATA_INDEX_FORMAT_VERSION: u32 = 1;
pub const METADATA_SEARCH_SEMANTIC_ID: u32 = 0x0003_0001;
pub const METADATA_RARE_MAX_DF: usize = 4096;
pub const METADATA_FIRST_BATCH_DEFAULT: usize = 100;
const HEADER_BYTES: usize = 64;
const RECORD_BYTES: usize = 56;
const Q3_BUDGET_BYTES_PER_ENTRY: usize = 24;
const Q45_BUDGET_BYTES_PER_ENTRY: usize = 24;
const Q45_BLOOM_BYTES_PER_ENTRY_NUMERATOR: usize = 1;
const Q45_BLOOM_BYTES_PER_ENTRY_DENOMINATOR: usize = 4;
const Q45_BLOOM_MIN_BYTES: usize = 8 * 1024;
const Q45_BLOOM_MAX_BYTES: usize = 2 * 1024 * 1024;
const Q45_HASHES: usize = 4;
const Q45_SKETCH_ROWS: usize = 4;
const Q45_SKETCH_COLUMNS: usize = 1 << 18;
const Q45_POOL_MAX: usize = 131_072;
const PATH_ENCODING_UNIX_RAW: u8 = 1;
const PATH_ENCODING_WINDOWS_UTF16LE: u8 = 2;
const PATH_ENCODING_UTF8_FALLBACK: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum MetadataFileKind {
    File = 1,
    Directory = 2,
    Symlink = 3,
    Other = 4,
}

impl MetadataFileKind {
    fn from_u8(value: u8) -> Result<Self, MetadataError> {
        match value {
            1 => Ok(Self::File),
            2 => Ok(Self::Directory),
            3 => Ok(Self::Symlink),
            4 => Ok(Self::Other),
            _ => Err(MetadataError::InvalidFormat("file kind")),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetadataRecord {
    pub file_id: u64,
    pub path: PathBuf,
    pub source_root: u32,
    pub size: u64,
    pub modified_ns: u128,
    pub kind: MetadataFileKind,
    pub content_searchable: bool,
    pub extractable: bool,
}

impl MetadataRecord {
    pub fn file(file_id: u64, path: impl Into<PathBuf>, size: u64, modified_ns: u128) -> Self {
        Self {
            file_id,
            path: path.into(),
            source_root: 0,
            size,
            modified_ns,
            kind: MetadataFileKind::File,
            content_searchable: false,
            extractable: false,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct MetadataSearchRequest<'a> {
    pub filename: Option<&'a str>,
    pub full_path: Option<&'a str>,
    pub case_sensitive: bool,
    pub max_results: usize,
}

impl<'a> MetadataSearchRequest<'a> {
    pub fn filename(query: &'a str) -> Self {
        Self {
            filename: Some(query),
            max_results: METADATA_FIRST_BATCH_DEFAULT,
            ..Self::default()
        }
    }

    pub fn path(query: &'a str) -> Self {
        Self {
            full_path: Some(query),
            max_results: METADATA_FIRST_BATCH_DEFAULT,
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetadataHit {
    pub record_index: u32,
    pub file_id: u64,
}

#[derive(Clone, Debug, Default)]
pub struct MetadataSearchMetrics {
    pub candidate_records: usize,
    pub verified_records: usize,
    pub returned_records: usize,
    pub global_absent_shortcut: bool,
    pub selected_anchor_width: Option<u8>,
    pub selected_anchor_df: Option<usize>,
    pub filter_time: Duration,
    pub verify_time: Duration,
}

#[derive(Clone, Debug)]
pub struct MetadataSearchOutcome {
    pub hits: Vec<MetadataHit>,
    pub metrics: MetadataSearchMetrics,
}

#[derive(Clone, Copy, Debug)]
struct MetadataAnchorRecord {
    key: u64,
    df: u32,
    offset: u64,
}

#[derive(Clone, Debug, Default)]
struct MetadataCompressedAnchors {
    records: Vec<MetadataAnchorRecord>,
    posting_blob: Vec<u8>,
}

impl MetadataCompressedAnchors {
    fn from_postings(postings: HashMap<u64, Vec<u32>>, budget: usize) -> Self {
        let mut candidates = postings
            .into_iter()
            .filter(|(_, values)| !values.is_empty() && values.len() <= METADATA_RARE_MAX_DF)
            .collect::<Vec<_>>();
        candidates.sort_unstable_by_key(|(key, values)| (values.len(), stable_mix64(*key), *key));
        let mut selected = Vec::new();
        let mut used = 16_usize;
        for (key, values) in candidates {
            let bytes = 24 + delta_varint_len(&values);
            if used.saturating_add(bytes) > budget {
                continue;
            }
            used += bytes;
            selected.push((key, values));
        }
        selected.sort_unstable_by_key(|(key, _)| *key);
        let mut records = Vec::with_capacity(selected.len());
        let mut posting_blob = Vec::new();
        for (key, values) in selected {
            let offset = posting_blob.len() as u64;
            append_delta_varints(&mut posting_blob, &values);
            records.push(MetadataAnchorRecord {
                key,
                df: values.len() as u32,
                offset,
            });
        }
        Self {
            records,
            posting_blob,
        }
    }

    fn get(&self, key: u64) -> Result<Option<Vec<u32>>, MetadataError> {
        let Ok(index) = self.records.binary_search_by_key(&key, |record| record.key) else {
            return Ok(None);
        };
        let record = self.records[index];
        let (postings, _) = decode_delta_varints(
            &self.posting_blob,
            record.offset as usize,
            record.df as usize,
        )?;
        Ok(Some(postings))
    }

    fn persistent_bytes(&self) -> usize {
        16 + self.records.len() * 24 + self.posting_blob.len()
    }

    fn serialize_into(&self, out: &mut Vec<u8>) {
        put_u32(out, self.records.len() as u32);
        put_u32(out, 0);
        put_u64(out, self.posting_blob.len() as u64);
        for record in &self.records {
            put_u64(out, record.key);
            put_u32(out, record.df);
            put_u32(out, 0);
            put_u64(out, record.offset);
        }
        out.extend_from_slice(&self.posting_blob);
    }

    fn deserialize(cursor: &mut MetadataCursor<'_>) -> Result<Self, MetadataError> {
        let count = cursor.u32()? as usize;
        cursor.u32()?;
        let blob_len = cursor.u64()? as usize;
        let mut records = Vec::with_capacity(count);
        let mut previous_key = None;
        for _ in 0..count {
            let key = cursor.u64()?;
            let df = cursor.u32()?;
            cursor.u32()?;
            let offset = cursor.u64()?;
            if previous_key.is_some_and(|previous| key <= previous) {
                return Err(MetadataError::InvalidFormat("anchor key order"));
            }
            previous_key = Some(key);
            records.push(MetadataAnchorRecord { key, df, offset });
        }
        let posting_blob = cursor.take(blob_len)?.to_vec();
        for record in &records {
            if record.offset as usize > posting_blob.len() {
                return Err(MetadataError::InvalidFormat("anchor posting offset"));
            }
            if record.df as usize > METADATA_RARE_MAX_DF {
                return Err(MetadataError::InvalidFormat("anchor posting df"));
            }
        }
        Ok(Self {
            records,
            posting_blob,
        })
    }
}

#[derive(Clone, Debug)]
struct MetadataBloom {
    words: Vec<u64>,
    bit_count: usize,
}

impl MetadataBloom {
    fn new(entry_count: usize) -> Self {
        let bytes = entry_count.saturating_mul(Q45_BLOOM_BYTES_PER_ENTRY_NUMERATOR)
            / Q45_BLOOM_BYTES_PER_ENTRY_DENOMINATOR;
        let bytes = bytes.clamp(Q45_BLOOM_MIN_BYTES, Q45_BLOOM_MAX_BYTES) & !7;
        let words = vec![0_u64; bytes / 8];
        Self {
            bit_count: words.len() * 64,
            words,
        }
    }

    fn insert(&mut self, key: u64) {
        if self.bit_count == 0 {
            return;
        }
        for hash in 0..Q45_HASHES {
            let bit = metadata_bloom_bit(key, hash, self.bit_count);
            self.words[bit / 64] |= 1_u64 << (bit % 64);
        }
    }

    fn might_contain(&self, key: u64) -> bool {
        if self.bit_count == 0 {
            return true;
        }
        (0..Q45_HASHES).all(|hash| {
            let bit = metadata_bloom_bit(key, hash, self.bit_count);
            self.words[bit / 64] & (1_u64 << (bit % 64)) != 0
        })
    }
}

#[derive(Clone, Debug)]
struct MetadataFieldIndex {
    q3_presence: AdaptiveGlobalPresence,
    q3_anchors: MetadataCompressedAnchors,
    q45_bloom: MetadataBloom,
    q45_anchors: MetadataCompressedAnchors,
}

#[derive(Clone, Debug)]
pub struct MetadataIndex {
    records: Vec<MetadataRecord>,
    filename_index: MetadataFieldIndex,
    path_index: MetadataFieldIndex,
    persistent_bytes: usize,
}

#[derive(Debug)]
pub enum MetadataError {
    Io(io::Error),
    InvalidFormat(&'static str),
    FormatVersion(u32),
    SemanticVersion(u32),
    ChecksumMismatch,
    DuplicateFileId(u64),
    SnapshotExists(PathBuf),
}

impl fmt::Display for MetadataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::InvalidFormat(what) => write!(f, "invalid metadata index: {what}"),
            Self::FormatVersion(version) => {
                write!(f, "unsupported metadata format version: {version}")
            }
            Self::SemanticVersion(version) => {
                write!(f, "unsupported metadata semantic id: {version:#010x}")
            }
            Self::ChecksumMismatch => write!(f, "metadata index checksum mismatch"),
            Self::DuplicateFileId(file_id) => write!(f, "duplicate metadata file id: {file_id}"),
            Self::SnapshotExists(path) => {
                write!(f, "metadata snapshot already exists: {}", path.display())
            }
        }
    }
}

impl std::error::Error for MetadataError {}

impl From<io::Error> for MetadataError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl MetadataIndex {
    pub fn build(records: Vec<MetadataRecord>) -> Result<Self, MetadataError> {
        let mut seen = HashSet::with_capacity(records.len());
        for record in &records {
            if !seen.insert(record.file_id) {
                return Err(MetadataError::DuplicateFileId(record.file_id));
            }
        }
        let filename_storage = records
            .iter()
            .map(|record| metadata_folded_text(record, false))
            .collect::<Vec<_>>();
        let filename_strings = filename_storage
            .iter()
            .map(|value| value.as_deref())
            .collect::<Vec<_>>();
        let filename_index = MetadataFieldIndex::build(&filename_strings);
        drop(filename_strings);
        drop(filename_storage);

        let path_storage = records
            .iter()
            .map(|record| metadata_folded_text(record, true))
            .collect::<Vec<_>>();
        let path_strings = path_storage
            .iter()
            .map(|value| value.as_deref())
            .collect::<Vec<_>>();
        let path_index = MetadataFieldIndex::build(&path_strings);
        drop(path_strings);
        drop(path_storage);
        let path_bytes = records
            .iter()
            .map(|record| encode_exact_path(&record.path).1.len())
            .sum::<usize>();
        let persistent_bytes = HEADER_BYTES
            + records.len() * RECORD_BYTES
            + path_bytes
            + filename_index.persistent_bytes()
            + path_index.persistent_bytes();
        Ok(Self {
            records,
            filename_index,
            path_index,
            persistent_bytes,
        })
    }

    pub fn records(&self) -> &[MetadataRecord] {
        &self.records
    }

    pub fn persistent_bytes(&self) -> usize {
        self.persistent_bytes
    }

    pub fn bytes_per_record(&self) -> f64 {
        if self.records.is_empty() {
            0.0
        } else {
            self.persistent_bytes as f64 / self.records.len() as f64
        }
    }

    pub fn search(&self, request: MetadataSearchRequest<'_>) -> MetadataSearchOutcome {
        let max_results = if request.max_results == 0 {
            METADATA_FIRST_BATCH_DEFAULT
        } else {
            request.max_results
        };
        let filename_query = request
            .filename
            .filter(|query| !query.is_empty())
            .map(|query| normalize_metadata_query(query, request.case_sensitive));
        let path_query = request
            .full_path
            .filter(|query| !query.is_empty())
            .map(|query| normalize_metadata_path_query(query, request.case_sensitive));
        if filename_query.is_none() && path_query.is_none() {
            return MetadataSearchOutcome {
                hits: Vec::new(),
                metrics: MetadataSearchMetrics::default(),
            };
        }

        let filter_start = Instant::now();
        let mut absent = false;
        let mut best: Option<(Vec<u32>, u8)> = None;
        if let Some(query) = filename_query.as_ref() {
            match self.filename_index.best_candidates(query.folded.as_slice()) {
                CandidateChoice::Absent => absent = true,
                CandidateChoice::Posting(postings, width) => {
                    select_smaller_owned(&mut best, postings, width)
                }
                CandidateChoice::All => {}
            }
        }
        if let Some(query) = path_query.as_ref() {
            match self.path_index.best_candidates(query.folded.as_slice()) {
                CandidateChoice::Absent => absent = true,
                CandidateChoice::Posting(postings, width) => {
                    select_smaller_owned(&mut best, postings, width)
                }
                CandidateChoice::All => {}
            }
        }
        let filter_time = filter_start.elapsed();
        if absent {
            return MetadataSearchOutcome {
                hits: Vec::new(),
                metrics: MetadataSearchMetrics {
                    candidate_records: 0,
                    global_absent_shortcut: true,
                    filter_time,
                    ..MetadataSearchMetrics::default()
                },
            };
        }

        let candidate_records = best
            .as_ref()
            .map_or(self.records.len(), |(postings, _)| postings.len());
        let selected_anchor_width = best.as_ref().map(|(_, width)| *width);
        let selected_anchor_df = best.as_ref().map(|(postings, _)| postings.len());
        let verify_start = Instant::now();
        let mut hits = Vec::with_capacity(max_results.min(candidate_records));
        let mut verified_records = 0_usize;
        let mut visit = |record_index: usize| -> bool {
            verified_records += 1;
            if self.record_matches(
                record_index,
                filename_query.as_ref(),
                path_query.as_ref(),
                request.case_sensitive,
            ) {
                hits.push(MetadataHit {
                    record_index: record_index as u32,
                    file_id: self.records[record_index].file_id,
                });
            }
            hits.len() >= max_results
        };
        if let Some((postings, _)) = best.as_ref() {
            for record_index in postings.iter().copied().map(|value| value as usize) {
                if visit(record_index) {
                    break;
                }
            }
        } else {
            for record_index in 0..self.records.len() {
                if visit(record_index) {
                    break;
                }
            }
        }
        let verify_time = verify_start.elapsed();
        MetadataSearchOutcome {
            metrics: MetadataSearchMetrics {
                candidate_records,
                verified_records,
                returned_records: hits.len(),
                global_absent_shortcut: false,
                selected_anchor_width,
                selected_anchor_df,
                filter_time,
                verify_time,
            },
            hits,
        }
    }

    fn record_matches(
        &self,
        record_index: usize,
        filename_query: Option<&PreparedQuery>,
        path_query: Option<&PreparedQuery>,
        case_sensitive: bool,
    ) -> bool {
        let record = &self.records[record_index];
        if let Some(query) = filename_query {
            let Some(filename) = record.path.file_name().and_then(|value| value.to_str()) else {
                return false;
            };
            if !metadata_text_matches(filename, query, case_sensitive, false) {
                return false;
            }
        }
        if let Some(query) = path_query {
            let Some(path) = record.path.to_str() else {
                return false;
            };
            if !metadata_text_matches(path, query, case_sensitive, true) {
                return false;
            }
        }
        true
    }

    pub fn write_snapshot(&self, path: impl AsRef<Path>) -> Result<usize, MetadataError> {
        let bytes = self.serialize()?;
        let path = path.as_ref();
        if path.exists() {
            return Err(MetadataError::SnapshotExists(path.to_path_buf()));
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temp = path.with_extension("tmp");
        if temp.exists() {
            fs::remove_file(&temp)?;
        }
        {
            let mut file = File::create(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        fs::rename(&temp, path)?;
        if let Some(parent) = path.parent() {
            sync_directory(parent)?;
        }
        Ok(bytes.len())
    }

    pub fn load_snapshot(path: impl AsRef<Path>) -> Result<Self, MetadataError> {
        let bytes = fs::read(path)?;
        Self::deserialize(&bytes)
    }

    fn serialize(&self) -> Result<Vec<u8>, MetadataError> {
        let mut path_blob = Vec::new();
        let mut records = Vec::with_capacity(self.records.len() * RECORD_BYTES);
        for record in &self.records {
            let (encoding, path_bytes) = encode_exact_path(&record.path);
            let path_offset = path_blob.len() as u64;
            path_blob.extend_from_slice(&path_bytes);
            put_u64(&mut records, record.file_id);
            put_u32(&mut records, record.source_root);
            records.push(record.kind as u8);
            records.push(encoding);
            let mut flags = 0_u8;
            if record.content_searchable {
                flags |= 1;
            }
            if record.extractable {
                flags |= 2;
            }
            records.push(flags);
            records.push(0);
            put_u64(&mut records, record.size);
            put_u64(&mut records, record.modified_ns as u64);
            put_u64(&mut records, (record.modified_ns >> 64) as u64);
            put_u64(&mut records, path_offset);
            put_u32(&mut records, path_bytes.len() as u32);
            put_u32(&mut records, 0);
        }
        debug_assert_eq!(records.len(), self.records.len() * RECORD_BYTES);

        let filename_blob = self.filename_index.serialize();
        let path_index_blob = self.path_index.serialize();
        let records_offset = HEADER_BYTES as u64;
        let path_offset = records_offset + records.len() as u64;
        let filename_offset = path_offset + path_blob.len() as u64;
        let path_index_offset = filename_offset + filename_blob.len() as u64;
        let payload_end = path_index_offset + path_index_blob.len() as u64;
        let mut out = vec![0_u8; HEADER_BYTES];
        out.extend_from_slice(&records);
        out.extend_from_slice(&path_blob);
        out.extend_from_slice(&filename_blob);
        out.extend_from_slice(&path_index_blob);
        out[0..8].copy_from_slice(METADATA_INDEX_MAGIC);
        write_u32_at(&mut out, 8, METADATA_INDEX_FORMAT_VERSION);
        write_u32_at(&mut out, 12, METADATA_SEARCH_SEMANTIC_ID);
        write_u64_at(&mut out, 16, self.records.len() as u64);
        write_u64_at(&mut out, 24, records_offset);
        write_u64_at(&mut out, 32, path_offset);
        write_u64_at(&mut out, 40, filename_offset);
        write_u64_at(&mut out, 48, path_index_offset);
        debug_assert_eq!(payload_end as usize, out.len());
        let checksum = crc64_ecma(&out[HEADER_BYTES..]);
        write_u64_at(&mut out, 56, checksum);
        Ok(out)
    }

    fn deserialize(bytes: &[u8]) -> Result<Self, MetadataError> {
        if bytes.len() < HEADER_BYTES || &bytes[0..8] != METADATA_INDEX_MAGIC {
            return Err(MetadataError::InvalidFormat("magic"));
        }
        let format = read_u32(bytes, 8)?;
        if format != METADATA_INDEX_FORMAT_VERSION {
            return Err(MetadataError::FormatVersion(format));
        }
        let semantic = read_u32(bytes, 12)?;
        if semantic != METADATA_SEARCH_SEMANTIC_ID {
            return Err(MetadataError::SemanticVersion(semantic));
        }
        let count = read_u64(bytes, 16)? as usize;
        let records_offset = read_u64(bytes, 24)? as usize;
        let path_offset = read_u64(bytes, 32)? as usize;
        let filename_offset = read_u64(bytes, 40)? as usize;
        let path_index_offset = read_u64(bytes, 48)? as usize;
        let checksum = read_u64(bytes, 56)?;
        if records_offset != HEADER_BYTES
            || !(records_offset <= path_offset
                && path_offset <= filename_offset
                && filename_offset <= path_index_offset
                && path_index_offset <= bytes.len())
        {
            return Err(MetadataError::InvalidFormat("section offsets"));
        }
        if crc64_ecma(&bytes[HEADER_BYTES..]) != checksum {
            return Err(MetadataError::ChecksumMismatch);
        }
        let expected_records = count
            .checked_mul(RECORD_BYTES)
            .ok_or(MetadataError::InvalidFormat("record count"))?;
        if path_offset - records_offset != expected_records {
            return Err(MetadataError::InvalidFormat("record section length"));
        }
        let records_bytes = &bytes[records_offset..path_offset];
        let path_blob = &bytes[path_offset..filename_offset];
        let mut records = Vec::with_capacity(count);
        let mut seen = HashSet::with_capacity(count);
        for index in 0..count {
            let base = index * RECORD_BYTES;
            let file_id = read_u64(records_bytes, base)?;
            if !seen.insert(file_id) {
                return Err(MetadataError::DuplicateFileId(file_id));
            }
            let source_root = read_u32(records_bytes, base + 8)?;
            let kind = MetadataFileKind::from_u8(records_bytes[base + 12])?;
            let encoding = records_bytes[base + 13];
            let flags = records_bytes[base + 14];
            let size = read_u64(records_bytes, base + 16)?;
            let modified_low = read_u64(records_bytes, base + 24)? as u128;
            let modified_high = read_u64(records_bytes, base + 32)? as u128;
            let path_blob_offset = read_u64(records_bytes, base + 40)? as usize;
            let path_len = read_u32(records_bytes, base + 48)? as usize;
            let end = path_blob_offset
                .checked_add(path_len)
                .ok_or(MetadataError::InvalidFormat("path range"))?;
            let path_bytes = path_blob
                .get(path_blob_offset..end)
                .ok_or(MetadataError::InvalidFormat("path range"))?;
            records.push(MetadataRecord {
                file_id,
                path: decode_exact_path(encoding, path_bytes)?,
                source_root,
                size,
                modified_ns: modified_low | (modified_high << 64),
                kind,
                content_searchable: flags & 1 != 0,
                extractable: flags & 2 != 0,
            });
        }
        let filename_index =
            MetadataFieldIndex::deserialize(&bytes[filename_offset..path_index_offset])?;
        let path_index = MetadataFieldIndex::deserialize(&bytes[path_index_offset..])?;
        Ok(Self {
            records,
            filename_index,
            path_index,
            persistent_bytes: bytes.len(),
        })
    }
}

#[derive(Clone, Debug)]
struct PreparedQuery {
    exact: Vec<u8>,
    folded: Vec<u8>,
}

fn normalize_metadata_query(query: &str, case_sensitive: bool) -> PreparedQuery {
    PreparedQuery {
        exact: normalize_str(query, case_sensitive).into_bytes(),
        folded: normalize_str(query, false).into_bytes(),
    }
}

fn normalize_metadata_path_query(query: &str, case_sensitive: bool) -> PreparedQuery {
    let canonical = canonicalize_separators(query);
    normalize_metadata_query(&canonical, case_sensitive)
}

fn metadata_folded_text(record: &MetadataRecord, full_path: bool) -> Option<Vec<u8>> {
    let text = if full_path {
        record.path.to_str()?
    } else {
        record.path.file_name()?.to_str()?
    };
    if full_path {
        let canonical = canonicalize_separators(text);
        Some(normalize_str(&canonical, false).into_bytes())
    } else {
        Some(normalize_str(text, false).into_bytes())
    }
}

fn metadata_text_matches(
    text: &str,
    query: &PreparedQuery,
    case_sensitive: bool,
    path_mode: bool,
) -> bool {
    if is_byte_preserving_fast_path(text, case_sensitive) {
        return contains_metadata_fast(text.as_bytes(), &query.exact, case_sensitive, path_mode);
    }
    if path_mode {
        let canonical = canonicalize_separators(text);
        let normalized = normalize_str(&canonical, case_sensitive);
        contains_subslice(normalized.bytes(), &query.exact)
    } else {
        let normalized = normalize_str(text, case_sensitive);
        contains_subslice(normalized.bytes(), &query.exact)
    }
}

fn contains_metadata_fast(
    haystack: &[u8],
    needle: &[u8],
    case_sensitive: bool,
    path_mode: bool,
) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|window| {
        window.iter().zip(needle).all(|(left, right)| {
            if path_mode && matches!((*left, *right), (b'\\', b'/') | (b'/', b'/')) {
                return true;
            }
            if case_sensitive {
                left == right
            } else {
                left.eq_ignore_ascii_case(right)
            }
        })
    })
}

fn canonicalize_separators(value: &str) -> String {
    if value.contains('\\') {
        value.replace('\\', "/")
    } else {
        value.to_owned()
    }
}

enum CandidateChoice {
    Absent,
    Posting(Vec<u32>, u8),
    All,
}

impl MetadataFieldIndex {
    fn build(strings: &[Option<&[u8]>]) -> Self {
        let entry_count = strings.len();
        let mut dense = vec![0_u64; GLOBAL_TRIGRAM_WORDS];
        let mut q3_df = vec![0_u16; TRIGRAM_UNIVERSE];
        let mut q45_bloom = MetadataBloom::new(entry_count);
        let mut q45_sketch = MetadataQ45Sketch::new();

        for bytes in strings.iter().flatten() {
            let q3 = unique_q3(bytes);
            for gram in q3 {
                dense[gram as usize / 64] |= 1_u64 << (gram % 64);
                let df = &mut q3_df[gram as usize];
                *df = df.saturating_add(1).min((METADATA_RARE_MAX_DF + 1) as u16);
            }
            for key in unique_q45(bytes) {
                q45_bloom.insert(key);
                q45_sketch.add(key);
            }
        }
        let q3_presence = AdaptiveGlobalPresence::from_dense(&dense);
        drop(dense);

        let q3_budget = entry_count.saturating_mul(Q3_BUDGET_BYTES_PER_ENTRY);
        let q3_pool_limit = q3_budget.saturating_div(8).clamp(1, Q45_POOL_MAX);
        let mut q3_heap = BinaryHeap::<(u16, u64, u32)>::new();
        for (gram, df) in q3_df.iter().copied().enumerate() {
            if df == 0 || df as usize > METADATA_RARE_MAX_DF {
                continue;
            }
            let key = gram as u32;
            let score = (df, stable_mix64(key as u64), key);
            if q3_heap.len() < q3_pool_limit {
                q3_heap.push(score);
            } else if q3_heap.peek().is_some_and(|worst| score < *worst) {
                q3_heap.pop();
                q3_heap.push(score);
            }
        }
        drop(q3_df);
        let q3_keys = q3_heap
            .into_vec()
            .into_iter()
            .map(|(_, _, key)| key as u64)
            .collect::<HashSet<_>>();

        let q45_budget = entry_count.saturating_mul(Q45_BUDGET_BYTES_PER_ENTRY);
        let q45_pool_limit = q45_budget.saturating_div(8).clamp(1, Q45_POOL_MAX);
        let mut q45_heap = BinaryHeap::<(u16, u64, u64)>::new();
        let mut q45_seen = HashSet::<u64>::new();
        for bytes in strings.iter().flatten() {
            for key in unique_q45(bytes) {
                if q45_seen.contains(&key) {
                    continue;
                }
                let estimate = q45_sketch.estimate(key);
                if estimate == 0 || estimate as usize > METADATA_RARE_MAX_DF {
                    continue;
                }
                let score = (estimate, stable_mix64(key), key);
                if q45_heap.len() < q45_pool_limit {
                    q45_heap.push(score);
                    q45_seen.insert(key);
                } else if q45_heap.peek().is_some_and(|worst| score < *worst) {
                    if let Some(removed) = q45_heap.pop() {
                        q45_seen.remove(&removed.2);
                    }
                    q45_heap.push(score);
                    q45_seen.insert(key);
                }
            }
        }
        let q45_keys = q45_heap
            .into_vec()
            .into_iter()
            .map(|(_, _, key)| key)
            .collect::<HashSet<_>>();

        let mut q3_postings = q3_keys
            .iter()
            .copied()
            .map(|key| (key, Vec::<u32>::new()))
            .collect::<HashMap<_, _>>();
        let mut q45_postings = q45_keys
            .iter()
            .copied()
            .map(|key| (key, Vec::<u32>::new()))
            .collect::<HashMap<_, _>>();
        for (record_index, bytes) in strings.iter().enumerate() {
            let Some(bytes) = bytes else { continue };
            for gram in unique_q3(bytes) {
                if let Some(postings) = q3_postings.get_mut(&(gram as u64)) {
                    postings.push(record_index as u32);
                }
            }
            for key in unique_q45(bytes) {
                if let Some(postings) = q45_postings.get_mut(&key) {
                    postings.push(record_index as u32);
                }
            }
        }
        let q3_anchors = MetadataCompressedAnchors::from_postings(q3_postings, q3_budget);
        let q45_anchors = MetadataCompressedAnchors::from_postings(q45_postings, q45_budget);
        Self {
            q3_presence,
            q3_anchors,
            q45_bloom,
            q45_anchors,
        }
    }

    fn best_candidates(&self, query: &[u8]) -> CandidateChoice {
        if query.len() < 3 {
            return CandidateChoice::All;
        }
        let mut best: Option<(Vec<u32>, u8)> = None;
        let mut q3 = query
            .windows(3)
            .map(|gram| k3(gram[0], gram[1], gram[2]))
            .collect::<Vec<_>>();
        q3.sort_unstable();
        q3.dedup();
        for gram in q3 {
            if !self.q3_presence.contains(gram) {
                return CandidateChoice::Absent;
            }
            if let Ok(Some(postings)) = self.q3_anchors.get(gram as u64) {
                select_smaller_owned(&mut best, postings, 3);
            }
        }
        if query.len() >= 4 {
            for key in unique_q45(query) {
                if !self.q45_bloom.might_contain(key) {
                    return CandidateChoice::Absent;
                }
                if let Ok(Some(postings)) = self.q45_anchors.get(key) {
                    select_smaller_owned(&mut best, postings, (key >> 56) as u8);
                }
            }
        }
        best.map_or(CandidateChoice::All, |(postings, width)| {
            CandidateChoice::Posting(postings, width)
        })
    }

    fn persistent_bytes(&self) -> usize {
        self.q3_presence.persistent_bytes()
            + self.q3_anchors.persistent_bytes()
            + 16
            + self.q45_bloom.words.len() * 8
            + self.q45_anchors.persistent_bytes()
    }

    fn serialize(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.q3_presence.serialize_into(&mut out);
        self.q3_anchors.serialize_into(&mut out);
        put_u32(&mut out, self.q45_bloom.words.len() as u32);
        put_u32(&mut out, Q45_HASHES as u32);
        put_u64(&mut out, self.q45_bloom.bit_count as u64);
        for word in &self.q45_bloom.words {
            put_u64(&mut out, *word);
        }
        self.q45_anchors.serialize_into(&mut out);
        out
    }

    fn deserialize(bytes: &[u8]) -> Result<Self, MetadataError> {
        let mut cursor = MetadataCursor::new(bytes);
        let q3_presence = deserialize_presence(&mut cursor)?;
        let q3_anchors = MetadataCompressedAnchors::deserialize(&mut cursor)?;
        let word_count = cursor.u32()? as usize;
        let hashes = cursor.u32()? as usize;
        if hashes != Q45_HASHES {
            return Err(MetadataError::InvalidFormat("q45 hash count"));
        }
        let bit_count = cursor.u64()? as usize;
        let mut words = Vec::with_capacity(word_count);
        for _ in 0..word_count {
            words.push(cursor.u64()?);
        }
        if bit_count != words.len() * 64 {
            return Err(MetadataError::InvalidFormat("q45 bit count"));
        }
        let q45_anchors = MetadataCompressedAnchors::deserialize(&mut cursor)?;
        if !cursor.remaining().is_empty() {
            return Err(MetadataError::InvalidFormat("field trailing bytes"));
        }
        Ok(Self {
            q3_presence,
            q3_anchors,
            q45_bloom: MetadataBloom { words, bit_count },
            q45_anchors,
        })
    }
}

#[derive(Clone, Debug)]
struct MetadataQ45Sketch {
    counters: Vec<u16>,
}

impl MetadataQ45Sketch {
    fn new() -> Self {
        Self {
            counters: vec![0_u16; Q45_SKETCH_ROWS * Q45_SKETCH_COLUMNS],
        }
    }

    fn add(&mut self, key: u64) {
        for row in 0..Q45_SKETCH_ROWS {
            let column = metadata_sketch_column(key, row);
            let counter = &mut self.counters[row * Q45_SKETCH_COLUMNS + column];
            *counter = counter
                .saturating_add(1)
                .min((METADATA_RARE_MAX_DF + 1) as u16);
        }
    }

    fn estimate(&self, key: u64) -> u16 {
        (0..Q45_SKETCH_ROWS)
            .map(|row| self.counters[row * Q45_SKETCH_COLUMNS + metadata_sketch_column(key, row)])
            .min()
            .unwrap_or((METADATA_RARE_MAX_DF + 1) as u16)
    }
}

fn unique_q3(bytes: &[u8]) -> Vec<u32> {
    let mut values = bytes
        .windows(3)
        .map(|gram| k3(gram[0], gram[1], gram[2]))
        .collect::<Vec<_>>();
    values.sort_unstable();
    values.dedup();
    values
}

fn unique_q45(bytes: &[u8]) -> Vec<u64> {
    let mut values = Vec::with_capacity(bytes.len().saturating_mul(2));
    values.extend(bytes.windows(4).map(|gram| higher_key(4, k4(gram))));
    values.extend(bytes.windows(5).map(|gram| higher_key(5, k5(gram))));
    values.sort_unstable();
    values.dedup();
    values
}

fn select_smaller_owned(best: &mut Option<(Vec<u32>, u8)>, postings: Vec<u32>, width: u8) {
    if best
        .as_ref()
        .is_none_or(|(current, _)| postings.len() < current.len())
    {
        *best = Some((postings, width));
    }
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }
    if needle.len() == 1 {
        return haystack.contains(&needle[0]);
    }
    let last = needle.len() - 1;
    let mut shift = [needle.len(); 256];
    for (index, byte) in needle[..last].iter().copied().enumerate() {
        shift[byte as usize] = last - index;
    }
    let mut offset = 0_usize;
    while offset + needle.len() <= haystack.len() {
        let anchor = haystack[offset + last];
        if anchor == needle[last] && &haystack[offset..offset + needle.len()] == needle {
            return true;
        }
        offset += shift[anchor as usize].max(1);
    }
    false
}

fn metadata_bloom_bit(key: u64, hash_index: usize, bit_count: usize) -> usize {
    const SEEDS: [u64; Q45_HASHES] = [
        0x9e37_79b9_7f4a_7c15,
        0xbf58_476d_1ce4_e5b9,
        0x94d0_49bb_1331_11eb,
        0x243f_6a88_85a3_08d3,
    ];
    (stable_mix64(key ^ SEEDS[hash_index]) % bit_count as u64) as usize
}

fn metadata_sketch_column(key: u64, row: usize) -> usize {
    const SEEDS: [u64; Q45_SKETCH_ROWS] = [
        0x4528_21e6_38d0_1377,
        0xbe54_66cf_34e9_0c6c,
        0xc0ac_29b7_c97c_50dd,
        0x3f84_d5b5_b547_0917,
    ];
    (stable_mix64(key ^ SEEDS[row]) as usize) & (Q45_SKETCH_COLUMNS - 1)
}

fn deserialize_presence(
    cursor: &mut MetadataCursor<'_>,
) -> Result<AdaptiveGlobalPresence, MetadataError> {
    let tag = cursor.u8()?;
    cursor.skip(3)?;
    let count = cursor.u32()? as usize;
    match tag {
        1 => {
            if count != GLOBAL_TRIGRAM_WORDS {
                return Err(MetadataError::InvalidFormat("dense q3 count"));
            }
            let mut words = Vec::with_capacity(count);
            for _ in 0..count {
                words.push(cursor.u64()?);
            }
            Ok(AdaptiveGlobalPresence::Dense(words))
        }
        2 => {
            let mut values = Vec::with_capacity(count);
            for _ in 0..count {
                values.push(cursor.u24()?);
            }
            Ok(AdaptiveGlobalPresence::PackedU24(values))
        }
        3 => {
            let mut words = Vec::with_capacity(count);
            for _ in 0..count {
                words.push((cursor.u24()?, cursor.u64()?));
            }
            Ok(AdaptiveGlobalPresence::SparseWords(words))
        }
        _ => Err(MetadataError::InvalidFormat("q3 presence tag")),
    }
}

fn append_delta_varints(out: &mut Vec<u8>, postings: &[u32]) {
    let mut previous = 0_u32;
    for (index, value) in postings.iter().copied().enumerate() {
        let delta = if index == 0 { value } else { value - previous };
        append_varint(out, delta);
        previous = value;
    }
}

fn decode_delta_varints(
    bytes: &[u8],
    mut offset: usize,
    count: usize,
) -> Result<(Vec<u32>, usize), MetadataError> {
    let start = offset;
    if offset > bytes.len() {
        return Err(MetadataError::InvalidFormat("posting offset"));
    }
    let mut out = Vec::with_capacity(count);
    let mut previous = 0_u32;
    for index in 0..count {
        let mut value = 0_u32;
        let mut shift = 0_u32;
        loop {
            let byte = *bytes
                .get(offset)
                .ok_or(MetadataError::InvalidFormat("posting varint"))?;
            offset += 1;
            value |= ((byte & 0x7f) as u32) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift >= 35 {
                return Err(MetadataError::InvalidFormat("posting varint overflow"));
            }
        }
        let absolute = if index == 0 {
            value
        } else {
            previous
                .checked_add(value)
                .ok_or(MetadataError::InvalidFormat("posting overflow"))?
        };
        if index > 0 && absolute <= previous {
            return Err(MetadataError::InvalidFormat("posting order"));
        }
        out.push(absolute);
        previous = absolute;
    }
    Ok((out, offset - start))
}

fn delta_varint_len(postings: &[u32]) -> usize {
    let mut previous = 0_u32;
    postings
        .iter()
        .copied()
        .enumerate()
        .map(|(index, value)| {
            let delta = if index == 0 { value } else { value - previous };
            previous = value;
            varint_len(delta)
        })
        .sum()
}

fn append_varint(out: &mut Vec<u8>, mut value: u32) {
    while value >= 0x80 {
        out.push((value as u8 & 0x7f) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn varint_len(mut value: u32) -> usize {
    let mut bytes = 1;
    while value >= 0x80 {
        value >>= 7;
        bytes += 1;
    }
    bytes
}

struct MetadataCursor<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> MetadataCursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], MetadataError> {
        let end = self
            .position
            .checked_add(len)
            .ok_or(MetadataError::InvalidFormat("cursor overflow"))?;
        let value = self
            .bytes
            .get(self.position..end)
            .ok_or(MetadataError::InvalidFormat("truncated field"))?;
        self.position = end;
        Ok(value)
    }

    fn skip(&mut self, len: usize) -> Result<(), MetadataError> {
        self.take(len).map(|_| ())
    }

    fn u8(&mut self) -> Result<u8, MetadataError> {
        Ok(self.take(1)?[0])
    }

    fn u24(&mut self) -> Result<u32, MetadataError> {
        let bytes = self.take(3)?;
        Ok(bytes[0] as u32 | ((bytes[1] as u32) << 8) | ((bytes[2] as u32) << 16))
    }

    fn u32(&mut self) -> Result<u32, MetadataError> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, MetadataError> {
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
    }

    fn remaining(&self) -> &'a [u8] {
        &self.bytes[self.position..]
    }
}

#[cfg(unix)]
fn encode_exact_path(path: &Path) -> (u8, Vec<u8>) {
    use std::os::unix::ffi::OsStrExt;
    (PATH_ENCODING_UNIX_RAW, path.as_os_str().as_bytes().to_vec())
}

#[cfg(windows)]
fn encode_exact_path(path: &Path) -> (u8, Vec<u8>) {
    use std::os::windows::ffi::OsStrExt;
    let mut out = Vec::new();
    for value in path.as_os_str().encode_wide() {
        out.extend_from_slice(&value.to_le_bytes());
    }
    (PATH_ENCODING_WINDOWS_UTF16LE, out)
}

#[cfg(not(any(unix, windows)))]
fn encode_exact_path(path: &Path) -> (u8, Vec<u8>) {
    (
        PATH_ENCODING_UTF8_FALLBACK,
        path.to_string_lossy().as_bytes().to_vec(),
    )
}

fn decode_exact_path(encoding: u8, bytes: &[u8]) -> Result<PathBuf, MetadataError> {
    match encoding {
        PATH_ENCODING_UNIX_RAW => decode_unix_path(bytes),
        PATH_ENCODING_WINDOWS_UTF16LE => decode_windows_path(bytes),
        PATH_ENCODING_UTF8_FALLBACK => Ok(PathBuf::from(
            std::str::from_utf8(bytes).map_err(|_| MetadataError::InvalidFormat("utf8 path"))?,
        )),
        _ => Err(MetadataError::InvalidFormat("path encoding")),
    }
}

#[cfg(unix)]
fn decode_unix_path(bytes: &[u8]) -> Result<PathBuf, MetadataError> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
}

#[cfg(not(unix))]
fn decode_unix_path(_bytes: &[u8]) -> Result<PathBuf, MetadataError> {
    Err(MetadataError::InvalidFormat("unix path on non-unix"))
}

#[cfg(windows)]
fn decode_windows_path(bytes: &[u8]) -> Result<PathBuf, MetadataError> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    if !bytes.len().is_multiple_of(2) {
        return Err(MetadataError::InvalidFormat("utf16 path"));
    }
    let values = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    Ok(PathBuf::from(OsString::from_wide(&values)))
}

#[cfg(not(windows))]
fn decode_windows_path(_bytes: &[u8]) -> Result<PathBuf, MetadataError> {
    Err(MetadataError::InvalidFormat("windows path on non-windows"))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), MetadataError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), MetadataError> {
    Ok(())
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_u32_at(out: &mut [u8], offset: usize, value: u32) {
    out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64_at(out: &mut [u8], offset: usize, value: u64) {
    out[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, MetadataError> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or(MetadataError::InvalidFormat("u32"))?;
    Ok(u32::from_le_bytes(value.try_into().unwrap()))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, MetadataError> {
    let value = bytes
        .get(offset..offset + 8)
        .ok_or(MetadataError::InvalidFormat("u64"))?;
    Ok(u64::from_le_bytes(value.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_format_identity_is_separate_from_content_index() {
        assert_eq!(METADATA_INDEX_MAGIC, b"PRV2MET1");
        assert_eq!(
            METADATA_SEARCH_SEMANTIC_ID,
            super::super::persistent::PRODUCTION_SEARCH_SEMANTIC_ID
        );
    }

    #[test]
    fn separator_canonicalization_is_search_only() {
        assert_eq!(
            canonicalize_separators(r"C:\\work\\file.txt"),
            "C://work//file.txt"
        );
    }
}
