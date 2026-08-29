use super::*;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const PRODUCTION_INDEX_MAGIC: &[u8; 8] = b"PRV2IDX1";
pub const PRODUCTION_INDEX_FORMAT_VERSION: u32 = 2;
pub const PRODUCTION_SEARCH_SEMANTIC_ID: u32 = 0x0003_0001;
const FOOTER_MAGIC: &[u8; 8] = b"PRV2END1";
const SECTION_COUNT: usize = 7;
const FIXED_HEADER_BYTES: usize = 64;
const SECTION_DIR_ENTRY_BYTES: usize = 32;
const HEADER_BYTES: usize = FIXED_HEADER_BYTES + SECTION_COUNT * SECTION_DIR_ENTRY_BYTES;
const FOOTER_BYTES: usize = 24;
const SECTION_FILE_CATALOG: u32 = 1;
const SECTION_BLOCK_MAP: u32 = 2;
const SECTION_UNIGRAM: u32 = 3;
const SECTION_BIGRAM: u32 = 4;
const SECTION_Q3_GLOBAL: u32 = 5;
const SECTION_Q3_SPARSE: u32 = 6;
const SECTION_Q45: u32 = 7;
const PATH_ENCODING_UNIX_RAW: u8 = 1;
const PATH_ENCODING_WINDOWS_UTF16LE: u8 = 2;
const PATH_ENCODING_UTF8_FALLBACK: u8 = 3;

#[derive(Debug)]
pub enum PersistentError {
    Io(io::Error),
    InvalidFormat(&'static str),
    FormatVersion { found: u32 },
    SemanticVersion { found: u32 },
    ChecksumMismatch,
    SectionChecksumMismatch(u32),
    SourceDrift(PathBuf),
    CapacityExceeded(f64),
    NoValidGeneration,
    Pattern(PatternError),
    Verification(crate::extraction::ExtractionError),
}

impl fmt::Display for PersistentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::InvalidFormat(what) => write!(f, "invalid persistent index: {what}"),
            Self::FormatVersion { found } => write!(f, "unsupported format version: {found}"),
            Self::SemanticVersion { found } => write!(f, "unsupported search semantic id: {found}"),
            Self::ChecksumMismatch => write!(f, "persistent index checksum mismatch"),
            Self::SectionChecksumMismatch(section) => {
                write!(f, "persistent index section {section} checksum mismatch")
            }
            Self::SourceDrift(path) => write!(f, "indexed source changed: {}", path.display()),
            Self::CapacityExceeded(ratio) => {
                write!(
                    f,
                    "persistent index capacity {:.3}% exceeds 10% hard SLO",
                    ratio * 100.0
                )
            }
            Self::NoValidGeneration => write!(f, "no valid persistent index generation"),
            Self::Pattern(error) => write!(f, "pattern error: {error}"),
            Self::Verification(error) => write!(f, "verification error: {error}"),
        }
    }
}

impl std::error::Error for PersistentError {}

impl From<PatternError> for PersistentError {
    fn from(value: PatternError) -> Self {
        Self::Pattern(value)
    }
}

impl From<io::Error> for PersistentError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<crate::extraction::ExtractionError> for PersistentError {
    fn from(value: crate::extraction::ExtractionError) -> Self {
        Self::Verification(value)
    }
}

type Result<T> = std::result::Result<T, PersistentError>;

#[derive(Clone, Debug)]
pub struct PersistentCapacityReport {
    pub generation: u64,
    pub selected_source_bytes: u64,
    pub searchable_normalized_bytes: u64,
    pub total_index_bytes: u64,
    pub verification_bytes: u64,
    pub q45_bytes: u64,
    pub file_count: usize,
    pub block_count: usize,
}

impl PersistentCapacityReport {
    pub fn index_source_ratio(&self) -> f64 {
        ratio(self.total_index_bytes, self.selected_source_bytes)
    }

    pub fn combined_source_ratio(&self) -> f64 {
        ratio(
            self.total_index_bytes
                .saturating_add(self.verification_bytes),
            self.selected_source_bytes,
        )
    }
}

#[derive(Clone, Debug)]
pub struct PublishedGeneration {
    pub generation: u64,
    pub parent_generation: u64,
    pub path: PathBuf,
    pub capacity: PersistentCapacityReport,
}

#[derive(Clone, Debug)]
struct PersistentFileEntry {
    path: PathBuf,
    selected_bytes: u64,
    source_crc64: u64,
    source_modified_ns: u128,
}

#[derive(Clone, Debug)]
struct PersistentBlockSpan {
    file_id: u32,
    start: u64,
    end: u64,
    start_line: u32,
    line_count: u32,
}

#[derive(Clone, Debug)]
struct PersistentBlock {
    searchable_bytes: u64,
    spans: Vec<PersistentBlockSpan>,
}

#[derive(Debug)]
pub struct PersistentIndex {
    root: PathBuf,
    generation: u64,
    parent_generation: u64,
    selected_source_bytes: u64,
    searchable_normalized_bytes: u64,
    files: Vec<PersistentFileEntry>,
    blocks: Vec<PersistentBlock>,
    filter: PrototypeIndex,
    total_index_bytes: u64,
    q45_bytes: u64,
    verification: Option<crate::extraction::VerificationIndex>,
}

enum PersistentPattern<'a> {
    Regex(&'a RegexPattern),
    Wildcard(&'a WildcardPattern),
}

impl PersistentPattern<'_> {
    fn mandatory_literal(&self) -> Option<&[u8]> {
        match self {
            Self::Regex(value) => value.mandatory_literal(),
            Self::Wildcard(value) => value.mandatory_literal(),
        }
    }

    fn find_starts(&self, raw: &[u8]) -> Vec<u32> {
        match self {
            Self::Regex(value) => value.find_starts(raw),
            Self::Wildcard(value) => value.find_starts(raw),
        }
    }
}

pub struct PersistentSearchOverlay<'a> {
    pub excluded_file_ids: &'a HashSet<u32>,
    pub path_overrides: &'a HashMap<u32, PathBuf>,
}

impl PersistentIndex {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn parent_generation(&self) -> u64 {
        self.parent_generation
    }

    pub fn capacity_report(&self) -> PersistentCapacityReport {
        PersistentCapacityReport {
            generation: self.generation,
            selected_source_bytes: self.selected_source_bytes,
            searchable_normalized_bytes: self.searchable_normalized_bytes,
            total_index_bytes: self.total_index_bytes,
            verification_bytes: self
                .verification
                .as_ref()
                .map_or(0, crate::extraction::VerificationIndex::persistent_bytes),
            q45_bytes: self.q45_bytes,
            file_count: self.files.len(),
            block_count: self.blocks.len(),
        }
    }

    pub fn validate_all_sources(&self) -> Result<()> {
        for (file_id, entry) in self.files.iter().enumerate() {
            let path = self.root.join(&entry.path);
            let bytes = fs::read(&path)?;
            if bytes.len() as u64 != entry.selected_bytes
                || crc64_ecma(&bytes) != entry.source_crc64
            {
                return Err(PersistentError::SourceDrift(path));
            }
            self.validate_metadata(file_id as u32)?;
        }
        Ok(())
    }

    pub fn search_all(&self, query: &str, case_sensitive: bool) -> Result<SearchOutcome> {
        self.search_internal(query, case_sensitive, None, None)
    }

    pub fn search_first_batch(&self, query: &str, case_sensitive: bool) -> Result<SearchOutcome> {
        self.search_internal(query, case_sensitive, Some(SearchLimits::default()), None)
    }

    pub fn search_with_limits(
        &self,
        query: &str,
        case_sensitive: bool,
        limits: SearchLimits,
    ) -> Result<SearchOutcome> {
        self.search_internal(query, case_sensitive, Some(limits), None)
    }

    pub fn search_regex_all(&self, pattern: &str, case_sensitive: bool) -> Result<SearchOutcome> {
        let compiled = RegexPattern::compile(pattern, case_sensitive)?;
        self.search_pattern_internal(PersistentPattern::Regex(&compiled), None, None)
    }

    pub fn search_regex_first_batch(
        &self,
        pattern: &str,
        case_sensitive: bool,
    ) -> Result<SearchOutcome> {
        let compiled = RegexPattern::compile(pattern, case_sensitive)?;
        self.search_pattern_internal(
            PersistentPattern::Regex(&compiled),
            Some(SearchLimits::default()),
            None,
        )
    }

    pub fn search_regex_with_limits(
        &self,
        pattern: &str,
        case_sensitive: bool,
        limits: SearchLimits,
    ) -> Result<SearchOutcome> {
        let compiled = RegexPattern::compile(pattern, case_sensitive)?;
        self.search_pattern_internal(PersistentPattern::Regex(&compiled), Some(limits), None)
    }

    pub fn search_wildcard_all(
        &self,
        pattern: &str,
        case_sensitive: bool,
    ) -> Result<SearchOutcome> {
        let compiled = WildcardPattern::compile(pattern, case_sensitive)?;
        self.search_pattern_internal(PersistentPattern::Wildcard(&compiled), None, None)
    }

    pub fn search_wildcard_first_batch(
        &self,
        pattern: &str,
        case_sensitive: bool,
    ) -> Result<SearchOutcome> {
        let compiled = WildcardPattern::compile(pattern, case_sensitive)?;
        self.search_pattern_internal(
            PersistentPattern::Wildcard(&compiled),
            Some(SearchLimits::default()),
            None,
        )
    }

    pub fn search_wildcard_with_limits(
        &self,
        pattern: &str,
        case_sensitive: bool,
        limits: SearchLimits,
    ) -> Result<SearchOutcome> {
        let compiled = WildcardPattern::compile(pattern, case_sensitive)?;
        self.search_pattern_internal(PersistentPattern::Wildcard(&compiled), Some(limits), None)
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    pub fn file_relative_path(&self, file_id: u32) -> Option<&Path> {
        self.files
            .get(file_id as usize)
            .map(|entry| entry.path.as_path())
    }

    pub fn search_first_batch_with_overlay(
        &self,
        query: &str,
        case_sensitive: bool,
        overlay: &PersistentSearchOverlay<'_>,
    ) -> Result<SearchOutcome> {
        self.search_internal(
            query,
            case_sensitive,
            Some(SearchLimits::default()),
            Some(overlay),
        )
    }

    pub fn search_with_limits_and_overlay(
        &self,
        query: &str,
        case_sensitive: bool,
        limits: SearchLimits,
        overlay: &PersistentSearchOverlay<'_>,
    ) -> Result<SearchOutcome> {
        self.search_internal(query, case_sensitive, Some(limits), Some(overlay))
    }

    pub fn search_regex_first_batch_with_overlay(
        &self,
        pattern: &str,
        case_sensitive: bool,
        overlay: &PersistentSearchOverlay<'_>,
    ) -> Result<SearchOutcome> {
        let compiled = RegexPattern::compile(pattern, case_sensitive)?;
        self.search_pattern_internal(
            PersistentPattern::Regex(&compiled),
            Some(SearchLimits::default()),
            Some(overlay),
        )
    }

    pub fn search_regex_with_limits_and_overlay(
        &self,
        pattern: &str,
        case_sensitive: bool,
        limits: SearchLimits,
        overlay: &PersistentSearchOverlay<'_>,
    ) -> Result<SearchOutcome> {
        let compiled = RegexPattern::compile(pattern, case_sensitive)?;
        self.search_pattern_internal(
            PersistentPattern::Regex(&compiled),
            Some(limits),
            Some(overlay),
        )
    }

    pub fn search_wildcard_first_batch_with_overlay(
        &self,
        pattern: &str,
        case_sensitive: bool,
        overlay: &PersistentSearchOverlay<'_>,
    ) -> Result<SearchOutcome> {
        let compiled = WildcardPattern::compile(pattern, case_sensitive)?;
        self.search_pattern_internal(
            PersistentPattern::Wildcard(&compiled),
            Some(SearchLimits::default()),
            Some(overlay),
        )
    }

    pub fn search_wildcard_with_limits_and_overlay(
        &self,
        pattern: &str,
        case_sensitive: bool,
        limits: SearchLimits,
        overlay: &PersistentSearchOverlay<'_>,
    ) -> Result<SearchOutcome> {
        let compiled = WildcardPattern::compile(pattern, case_sensitive)?;
        self.search_pattern_internal(
            PersistentPattern::Wildcard(&compiled),
            Some(limits),
            Some(overlay),
        )
    }

    fn search_internal(
        &self,
        query: &str,
        case_sensitive: bool,
        limits: Option<SearchLimits>,
        overlay: Option<&PersistentSearchOverlay<'_>>,
    ) -> Result<SearchOutcome> {
        let filter_started = Instant::now();
        let folded_query = fold_for_index(query.as_bytes());
        let (candidate_words, global_absent_shortcut, selected_anchor_df, selected_anchor_width) =
            self.filter
                .candidate_bitmap(&folded_query, PrototypeVariant::D);
        let candidate_blocks = collect_set_bits(&candidate_words, self.blocks.len());
        let candidate_bytes = candidate_blocks
            .iter()
            .map(|block_id| self.blocks[*block_id].searchable_bytes)
            .sum();
        let filter_time = filter_started.elapsed();

        let verify_started = Instant::now();
        let mut assembly_time = Duration::ZERO;
        let mut hits = Vec::new();
        let mut verification_bytes = 0_u64;
        let mut seen_files = HashSet::new();
        let mut snippets_per_file = HashMap::<u32, usize>::new();
        let mut matched_locations_seen = 0_usize;
        let normalized_query = normalize_str(query, case_sensitive);
        let matcher = ExactMatcher::new(normalized_query.bytes());
        let mut stop = query.is_empty();
        let mut open_files = HashMap::<u32, File>::new();
        let mut metadata_validated = HashSet::<u32>::new();

        'blocks: for block_id in candidate_blocks.iter().copied() {
            if stop {
                break;
            }
            for span in &self.blocks[block_id].spans {
                if overlay.is_some_and(|value| value.excluded_file_ids.contains(&span.file_id)) {
                    continue;
                }
                let override_path =
                    overlay.and_then(|value| value.path_overrides.get(&span.file_id));
                if metadata_validated.insert(span.file_id) {
                    self.validate_metadata_at(span.file_id, override_path.map(PathBuf::as_path))?;
                }
                let entry = &self.files[span.file_id as usize];
                let relative_path = override_path.map(PathBuf::as_path).unwrap_or(&entry.path);
                let bytes = self.read_span_bytes(span, relative_path, &mut open_files)?;
                verification_bytes = verification_bytes.saturating_add(bytes.len() as u64);
                let mut line_number = span.start_line;
                let mut line_start = 0_usize;
                let mut lines_seen = 0_u32;
                for line_end_with_lf in bytes
                    .iter()
                    .enumerate()
                    .filter_map(|(index, byte)| (*byte == b'\n').then_some(index + 1))
                    .chain(std::iter::once(bytes.len()))
                {
                    if lines_seen >= span.line_count || line_end_with_lf < line_start {
                        break;
                    }
                    let mut content_end = if line_end_with_lf > line_start
                        && bytes.get(line_end_with_lf - 1) == Some(&b'\n')
                    {
                        line_end_with_lf - 1
                    } else {
                        line_end_with_lf
                    };
                    if content_end > line_start && bytes.get(content_end - 1) == Some(&b'\r') {
                        content_end -= 1;
                    }
                    let raw_line = &bytes[line_start..content_end];
                    let normalized = normalize_utf8(raw_line, case_sensitive);
                    let fallback_folded;
                    let haystack = if let Some(normalized) = normalized.as_ref() {
                        normalized.bytes()
                    } else if case_sensitive {
                        raw_line
                    } else {
                        fallback_folded = fold_ascii(raw_line);
                        &fallback_folded
                    };
                    let mut last_original_position = None::<u32>;
                    for position in matcher.find_all(haystack) {
                        let original_position = normalized
                            .as_ref()
                            .map(|value| value.origin_at(position))
                            .unwrap_or(position as u32);
                        if last_original_position == Some(original_position) {
                            continue;
                        }
                        last_original_position = Some(original_position);
                        matched_locations_seen = matched_locations_seen.saturating_add(1);
                        let assembly_started = Instant::now();
                        seen_files.insert(span.file_id);
                        let snippet_count = snippets_per_file.entry(span.file_id).or_default();
                        let may_store =
                            limits.is_none_or(|value| *snippet_count < value.max_snippets_per_file);
                        if may_store {
                            hits.push(SearchHit {
                                file_id: span.file_id,
                                line_number,
                                byte_offset_in_line: original_position,
                            });
                            *snippet_count += 1;
                        }
                        if let Some(value) = limits {
                            stop = seen_files.len() >= value.max_files
                                || matched_locations_seen >= value.max_matches_seen;
                        }
                        assembly_time += assembly_started.elapsed();
                        if stop {
                            break 'blocks;
                        }
                    }
                    lines_seen = lines_seen.saturating_add(1);
                    line_number = line_number.saturating_add(1);
                    if line_end_with_lf == bytes.len() {
                        break;
                    }
                    line_start = line_end_with_lf;
                }
            }
        }
        let verify_total = verify_started.elapsed();
        Ok(SearchOutcome {
            hits,
            metrics: SearchMetrics {
                candidate_blocks: candidate_blocks.len(),
                candidate_bytes,
                verification_bytes,
                filter_time,
                verify_time: verify_total.saturating_sub(assembly_time),
                result_assembly_time: assembly_time,
                returned_files: seen_files.len(),
                returned_snippets: snippets_per_file.values().sum(),
                matched_locations_seen,
                global_absent_shortcut,
                selected_anchor_df,
                selected_anchor_width,
            },
        })
    }

    fn search_pattern_internal(
        &self,
        pattern: PersistentPattern<'_>,
        limits: Option<SearchLimits>,
        overlay: Option<&PersistentSearchOverlay<'_>>,
    ) -> Result<SearchOutcome> {
        let filter_started = Instant::now();
        let (candidate_words, global_absent_shortcut, selected_anchor_df, selected_anchor_width) =
            if let Some(anchor) = pattern.mandatory_literal() {
                let folded = fold_for_index(anchor);
                self.filter.candidate_bitmap(&folded, PrototypeVariant::D)
            } else {
                let mut all = vec![0_u64; self.filter.words_per_bitmap];
                for block_id in 0..self.blocks.len() {
                    set_bit(&mut all, block_id);
                }
                (all, false, None, None)
            };
        let candidate_blocks = collect_set_bits(&candidate_words, self.blocks.len());
        let candidate_bytes = candidate_blocks
            .iter()
            .map(|block_id| self.blocks[*block_id].searchable_bytes)
            .sum();
        let filter_time = filter_started.elapsed();

        let verify_started = Instant::now();
        let mut assembly_time = Duration::ZERO;
        let mut hits = Vec::new();
        let mut verification_bytes = 0_u64;
        let mut seen_files = HashSet::new();
        let mut snippets_per_file = HashMap::<u32, usize>::new();
        let mut matched_locations_seen = 0_usize;
        let mut stop = false;
        let mut open_files = HashMap::<u32, File>::new();
        let mut metadata_validated = HashSet::<u32>::new();

        'blocks: for block_id in candidate_blocks.iter().copied() {
            for span in &self.blocks[block_id].spans {
                if overlay.is_some_and(|value| value.excluded_file_ids.contains(&span.file_id)) {
                    continue;
                }
                let override_path =
                    overlay.and_then(|value| value.path_overrides.get(&span.file_id));
                if metadata_validated.insert(span.file_id) {
                    self.validate_metadata_at(span.file_id, override_path.map(PathBuf::as_path))?;
                }
                let entry = &self.files[span.file_id as usize];
                let relative_path = override_path.map(PathBuf::as_path).unwrap_or(&entry.path);
                let bytes = self.read_span_bytes(span, relative_path, &mut open_files)?;
                verification_bytes = verification_bytes.saturating_add(bytes.len() as u64);
                let mut line_number = span.start_line;
                let mut line_start = 0_usize;
                let mut lines_seen = 0_u32;
                for line_end_with_lf in bytes
                    .iter()
                    .enumerate()
                    .filter_map(|(index, byte)| (*byte == b'\n').then_some(index + 1))
                    .chain(std::iter::once(bytes.len()))
                {
                    if lines_seen >= span.line_count || line_end_with_lf < line_start {
                        break;
                    }
                    let mut content_end = if line_end_with_lf > line_start
                        && bytes.get(line_end_with_lf - 1) == Some(&b'\n')
                    {
                        line_end_with_lf - 1
                    } else {
                        line_end_with_lf
                    };
                    if content_end > line_start && bytes.get(content_end - 1) == Some(&b'\r') {
                        content_end -= 1;
                    }
                    let raw_line = &bytes[line_start..content_end];
                    for original_position in pattern.find_starts(raw_line) {
                        matched_locations_seen = matched_locations_seen.saturating_add(1);
                        let assembly_started = Instant::now();
                        seen_files.insert(span.file_id);
                        let snippet_count = snippets_per_file.entry(span.file_id).or_default();
                        if limits.is_none_or(|value| *snippet_count < value.max_snippets_per_file) {
                            hits.push(SearchHit {
                                file_id: span.file_id,
                                line_number,
                                byte_offset_in_line: original_position,
                            });
                            *snippet_count += 1;
                        }
                        if let Some(value) = limits {
                            stop = seen_files.len() >= value.max_files
                                || matched_locations_seen >= value.max_matches_seen;
                        }
                        assembly_time += assembly_started.elapsed();
                        if stop {
                            break 'blocks;
                        }
                    }
                    lines_seen = lines_seen.saturating_add(1);
                    line_number = line_number.saturating_add(1);
                    if line_end_with_lf == bytes.len() {
                        break;
                    }
                    line_start = line_end_with_lf;
                }
            }
        }
        let verify_total = verify_started.elapsed();
        Ok(SearchOutcome {
            hits,
            metrics: SearchMetrics {
                candidate_blocks: candidate_blocks.len(),
                candidate_bytes,
                verification_bytes,
                filter_time,
                verify_time: verify_total.saturating_sub(assembly_time),
                result_assembly_time: assembly_time,
                returned_files: seen_files.len(),
                returned_snippets: snippets_per_file.values().sum(),
                matched_locations_seen,
                global_absent_shortcut,
                selected_anchor_df,
                selected_anchor_width,
            },
        })
    }

    fn attach_verification(
        &mut self,
        verification: crate::extraction::VerificationIndex,
    ) -> Result<()> {
        if verification.generation() != self.generation {
            return Err(PersistentError::InvalidFormat(
                "verification/content generation mismatch",
            ));
        }
        let expected_documents = self
            .files
            .iter()
            .enumerate()
            .filter(|(_, entry)| crate::extraction::is_extractable_document(&entry.path))
            .count();
        if verification.record_count() != expected_documents {
            return Err(PersistentError::InvalidFormat(
                "verification record count mismatch",
            ));
        }
        for (file_id, entry) in self.files.iter().enumerate() {
            let file_id = file_id as u32;
            let is_document = crate::extraction::is_extractable_document(&entry.path);
            if is_document != verification.has_file(file_id) {
                return Err(PersistentError::InvalidFormat(
                    "verification file mapping mismatch",
                ));
            }
            if is_document {
                verification.validate_source_identity(
                    file_id,
                    entry.selected_bytes,
                    entry.source_crc64,
                    entry.source_modified_ns,
                )?;
            }
        }
        self.verification = Some(verification);
        Ok(())
    }

    fn read_span_bytes(
        &self,
        span: &PersistentBlockSpan,
        relative_path: &Path,
        open_files: &mut HashMap<u32, File>,
    ) -> Result<Vec<u8>> {
        if let Some(verification) = self.verification.as_ref()
            && verification.has_file(span.file_id)
        {
            return verification
                .read_range(span.file_id, span.start, span.end)
                .map_err(PersistentError::from);
        }
        let file = if let Some(file) = open_files.get_mut(&span.file_id) {
            file
        } else {
            let file = File::open(self.root.join(relative_path))?;
            open_files.entry(span.file_id).or_insert(file)
        };
        file.seek(SeekFrom::Start(span.start))?;
        let span_len = span.end.saturating_sub(span.start) as usize;
        let mut bytes = vec![0_u8; span_len];
        file.read_exact(&mut bytes)?;
        Ok(bytes)
    }

    fn validate_metadata(&self, file_id: u32) -> Result<()> {
        self.validate_metadata_at(file_id, None)
    }

    fn validate_metadata_at(&self, file_id: u32, override_path: Option<&Path>) -> Result<()> {
        let entry = self
            .files
            .get(file_id as usize)
            .ok_or(PersistentError::InvalidFormat("file id out of range"))?;
        let path = self.root.join(override_path.unwrap_or(&entry.path));
        let metadata = fs::metadata(&path)?;
        if metadata.len() != entry.selected_bytes {
            return Err(PersistentError::SourceDrift(path));
        }
        if entry.source_modified_ns != 0
            && metadata_modified_ns(&metadata) != entry.source_modified_ns
        {
            return Err(PersistentError::SourceDrift(path));
        }
        Ok(())
    }
}

pub fn publish_generation(
    root: impl AsRef<Path>,
    store_dir: impl AsRef<Path>,
    generation: u64,
    parent_generation: u64,
) -> Result<PublishedGeneration> {
    let root = root.as_ref();
    let store_dir = store_dir.as_ref();
    fs::create_dir_all(store_dir)?;
    let corpus = Corpus::from_directory(root)?;
    let filter = PrototypeIndex::build(&corpus);
    let (bytes, q45_bytes) = serialize_production(&corpus, &filter, generation, parent_generation)?;
    let file_name = generation_file_name(generation);
    let path = store_dir.join(&file_name);
    atomic_write_new(&path, &bytes)?;
    write_advisory_current(store_dir, &file_name)?;
    sync_directory(store_dir)?;
    Ok(PublishedGeneration {
        generation,
        parent_generation,
        path,
        capacity: PersistentCapacityReport {
            generation,
            selected_source_bytes: corpus.selected_source_bytes,
            searchable_normalized_bytes: corpus.searchable_normalized_bytes,
            total_index_bytes: bytes.len() as u64,
            verification_bytes: 0,
            q45_bytes: q45_bytes as u64,
            file_count: corpus.files.len(),
            block_count: corpus.blocks.len(),
        },
    })
}

pub fn publish_generation_with_extraction(
    root: impl AsRef<Path>,
    store_dir: impl AsRef<Path>,
    generation: u64,
    parent_generation: u64,
    config: &crate::extraction::ExtractorConfig,
) -> Result<PublishedGeneration> {
    let root = root.as_ref();
    let store_dir = store_dir.as_ref();
    fs::create_dir_all(store_dir)?;
    let (corpus, verification_draft) = crate::extraction::build_corpus_with_documents(
        root,
        generation,
        parent_generation,
        config,
    )?;
    let filter = PrototypeIndex::build(&corpus);
    let (bytes, q45_bytes) = serialize_production(&corpus, &filter, generation, parent_generation)?;
    let verification_bytes = verification_draft.serialize()?;
    let combined = bytes.len().saturating_add(verification_bytes.len()) as u64;
    let combined_ratio = ratio(combined, corpus.selected_source_bytes);
    if combined_ratio > 0.10 {
        return Err(PersistentError::CapacityExceeded(combined_ratio));
    }
    let verification_path = store_dir.join(crate::extraction::verification_file_name(generation));
    atomic_write_new(&verification_path, &verification_bytes)?;
    let file_name = generation_file_name(generation);
    let path = store_dir.join(&file_name);
    if let Err(error) = atomic_write_new(&path, &bytes) {
        let _ = fs::remove_file(&verification_path);
        return Err(error);
    }
    write_advisory_current(store_dir, &file_name)?;
    sync_directory(store_dir)?;
    Ok(PublishedGeneration {
        generation,
        parent_generation,
        path,
        capacity: PersistentCapacityReport {
            generation,
            selected_source_bytes: corpus.selected_source_bytes,
            searchable_normalized_bytes: corpus.searchable_normalized_bytes,
            total_index_bytes: bytes.len() as u64,
            verification_bytes: verification_bytes.len() as u64,
            q45_bytes: q45_bytes as u64,
            file_count: corpus.files.len(),
            block_count: corpus.blocks.len(),
        },
    })
}

pub fn publish_next_generation(
    root: impl AsRef<Path>,
    store_dir: impl AsRef<Path>,
) -> Result<PublishedGeneration> {
    let store_dir = store_dir.as_ref();
    let valid = valid_generation_numbers(store_dir)?;
    let parent = current_valid_generation(store_dir)?
        .or_else(|| valid.first().copied())
        .unwrap_or(0);
    let generation = generation_paths_desc(store_dir)?
        .into_iter()
        .filter_map(|path| generation_from_path(&path))
        .max()
        .unwrap_or(0)
        .saturating_add(1)
        .max(1);
    publish_generation(root, store_dir, generation, parent)
}

pub fn publish_next_generation_with_extraction(
    root: impl AsRef<Path>,
    store_dir: impl AsRef<Path>,
    config: &crate::extraction::ExtractorConfig,
) -> Result<PublishedGeneration> {
    let store_dir = store_dir.as_ref();
    let valid = valid_generation_numbers(store_dir)?;
    let parent = current_valid_generation(store_dir)?
        .or_else(|| valid.first().copied())
        .unwrap_or(0);
    let generation = generation_paths_desc(store_dir)?
        .into_iter()
        .filter_map(|path| generation_from_path(&path))
        .max()
        .unwrap_or(0)
        .saturating_add(1)
        .max(1);
    publish_generation_with_extraction(root, store_dir, generation, parent, config)
}

pub fn compact_publish_with_extraction(
    root: impl AsRef<Path>,
    store_dir: impl AsRef<Path>,
    config: &crate::extraction::ExtractorConfig,
) -> Result<PublishedGeneration> {
    publish_next_generation_with_extraction(root, store_dir, config)
}

pub fn compact_publish(
    root: impl AsRef<Path>,
    store_dir: impl AsRef<Path>,
) -> Result<PublishedGeneration> {
    publish_next_generation(root, store_dir)
}

pub fn load_generation(
    root: impl AsRef<Path>,
    store_dir: impl AsRef<Path>,
    generation: u64,
) -> Result<PersistentIndex> {
    let path = store_dir.as_ref().join(generation_file_name(generation));
    let bytes = fs::read(path)?;
    deserialize_production(root.as_ref(), &bytes)
}

pub fn load_generation_with_verification(
    root: impl AsRef<Path>,
    store_dir: impl AsRef<Path>,
    generation: u64,
    config: &crate::extraction::ExtractorConfig,
) -> Result<PersistentIndex> {
    let root = root.as_ref();
    let store_dir = store_dir.as_ref();
    let mut index = load_generation(root, store_dir, generation)?;
    let bytes = fs::read(store_dir.join(crate::extraction::verification_file_name(generation)))?;
    let verification = crate::extraction::deserialize_verification(bytes, generation, config)?;
    index.attach_verification(verification)?;
    Ok(index)
}

pub fn load_latest(root: impl AsRef<Path>, store_dir: impl AsRef<Path>) -> Result<PersistentIndex> {
    let root = root.as_ref();
    let store_dir = store_dir.as_ref();
    let mut candidates = Vec::<PathBuf>::new();
    if let Ok(current) = fs::read_to_string(store_dir.join("CURRENT")) {
        let name = current.trim();
        if is_generation_file_name(name) {
            candidates.push(store_dir.join(name));
        }
    }
    for path in generation_paths_desc(store_dir)? {
        if !candidates.iter().any(|candidate| candidate == &path) {
            candidates.push(path);
        }
    }
    for path in candidates {
        if let Ok(bytes) = fs::read(&path)
            && let Ok(index) = deserialize_production(root, &bytes)
        {
            return Ok(index);
        }
    }
    Err(PersistentError::NoValidGeneration)
}

pub fn load_latest_with_verification(
    root: impl AsRef<Path>,
    store_dir: impl AsRef<Path>,
    config: &crate::extraction::ExtractorConfig,
) -> Result<PersistentIndex> {
    let root = root.as_ref();
    let store_dir = store_dir.as_ref();
    let mut candidates = Vec::<u64>::new();
    if let Ok(current) = fs::read_to_string(store_dir.join("CURRENT"))
        && let Some(generation) = generation_from_path(Path::new(current.trim()))
    {
        candidates.push(generation);
    }
    for path in generation_paths_desc(store_dir)? {
        if let Some(generation) = generation_from_path(&path)
            && !candidates.contains(&generation)
        {
            candidates.push(generation);
        }
    }
    for generation in candidates {
        if let Ok(index) = load_generation_with_verification(root, store_dir, generation, config) {
            return Ok(index);
        }
    }
    Err(PersistentError::NoValidGeneration)
}

pub fn gc_valid_generations(
    store_dir: impl AsRef<Path>,
    keep_valid: usize,
) -> Result<Vec<PathBuf>> {
    let store_dir = store_dir.as_ref();
    let keep_valid = keep_valid.max(2);
    let current = current_valid_generation(store_dir)?;
    let mut valid = Vec::<(u64, PathBuf)>::new();
    for path in generation_paths_desc(store_dir)? {
        if let Ok(bytes) = fs::read(&path)
            && deserialize_production(Path::new("."), &bytes).is_ok()
            && let Some(generation) = generation_from_path(&path)
        {
            valid.push((generation, path));
        }
    }
    valid.sort_unstable_by_key(|item| std::cmp::Reverse(item.0));
    let mut keep = HashSet::<u64>::new();
    if let Some(generation) = current {
        keep.insert(generation);
    }
    for (generation, _) in &valid {
        if keep.len() >= keep_valid {
            break;
        }
        keep.insert(*generation);
    }
    let mut removed = Vec::new();
    for (generation, path) in valid {
        if !keep.contains(&generation) {
            fs::remove_file(&path)?;
            removed.push(path);
        }
    }
    sync_directory(store_dir)?;
    Ok(removed)
}

fn serialize_production(
    corpus: &Corpus,
    filter: &PrototypeIndex,
    generation: u64,
    parent_generation: u64,
) -> Result<(Vec<u8>, usize)> {
    let sections = vec![
        (SECTION_FILE_CATALOG, serialize_file_catalog(corpus)),
        (SECTION_BLOCK_MAP, serialize_block_map(corpus)),
        (
            SECTION_UNIGRAM,
            serialize_bitmap_rows(
                &filter.unigram,
                256,
                filter.words_per_bitmap,
                filter.block_count,
            ),
        ),
        (
            SECTION_BIGRAM,
            serialize_bitmap_rows(
                &filter.bigram,
                65_536,
                filter.words_per_bitmap,
                filter.block_count,
            ),
        ),
        (
            SECTION_Q3_GLOBAL,
            serialize_q3_global(&filter.adaptive_global_presence),
        ),
        (
            SECTION_Q3_SPARSE,
            serialize_q3_sparse(&filter.sparse_anchors, corpus.selected_source_bytes),
        ),
        (
            SECTION_Q45,
            serialize_q45(
                &filter.higher_ngram_filter,
                &filter.higher_sparse_anchors,
                corpus.selected_source_bytes,
            ),
        ),
    ];
    let q45_bytes = sections[6].1.len();
    let mut out = vec![0_u8; HEADER_BYTES];
    out[0..8].copy_from_slice(PRODUCTION_INDEX_MAGIC);
    write_u32_at(&mut out, 8, PRODUCTION_INDEX_FORMAT_VERSION);
    write_u32_at(&mut out, 12, PRODUCTION_SEARCH_SEMANTIC_ID);
    write_u64_at(&mut out, 16, generation);
    write_u64_at(&mut out, 24, parent_generation);
    write_u64_at(&mut out, 32, corpus.selected_source_bytes);
    write_u64_at(&mut out, 40, corpus.searchable_normalized_bytes);
    write_u32_at(&mut out, 48, corpus.files.len() as u32);
    write_u32_at(&mut out, 52, corpus.blocks.len() as u32);
    write_u32_at(&mut out, 56, SECTION_COUNT as u32);
    write_u32_at(&mut out, 60, TARGET_BLOCK_BYTES as u32);

    for (index, (section_id, payload)) in sections.into_iter().enumerate() {
        let offset = out.len() as u64;
        let length = payload.len() as u64;
        let checksum = crc64_ecma(&payload);
        out.extend_from_slice(&payload);
        let dir = FIXED_HEADER_BYTES + index * SECTION_DIR_ENTRY_BYTES;
        write_u32_at(&mut out, dir, section_id);
        write_u32_at(&mut out, dir + 4, 0);
        write_u64_at(&mut out, dir + 8, offset);
        write_u64_at(&mut out, dir + 16, length);
        write_u64_at(&mut out, dir + 24, checksum);
    }
    let checksum = crc64_ecma(&out);
    out.extend_from_slice(FOOTER_MAGIC);
    put_u64(&mut out, checksum);
    let total_len = (out.len() + 8) as u64;
    put_u64(&mut out, total_len);
    Ok((out, q45_bytes))
}

fn deserialize_production(root: &Path, bytes: &[u8]) -> Result<PersistentIndex> {
    if bytes.len() < HEADER_BYTES + FOOTER_BYTES {
        return Err(PersistentError::InvalidFormat("truncated header"));
    }
    if &bytes[0..8] != PRODUCTION_INDEX_MAGIC {
        return Err(PersistentError::InvalidFormat("magic"));
    }
    let version = read_u32(bytes, 8)?;
    if version != PRODUCTION_INDEX_FORMAT_VERSION {
        return Err(PersistentError::FormatVersion { found: version });
    }
    let semantic = read_u32(bytes, 12)?;
    if semantic != PRODUCTION_SEARCH_SEMANTIC_ID {
        return Err(PersistentError::SemanticVersion { found: semantic });
    }
    let footer = bytes.len() - FOOTER_BYTES;
    if &bytes[footer..footer + 8] != FOOTER_MAGIC {
        return Err(PersistentError::InvalidFormat("footer magic"));
    }
    if read_u64(bytes, footer + 16)? != bytes.len() as u64 {
        return Err(PersistentError::InvalidFormat("footer length"));
    }
    if read_u64(bytes, footer + 8)? != crc64_ecma(&bytes[..footer]) {
        return Err(PersistentError::ChecksumMismatch);
    }
    let section_count = read_u32(bytes, 56)? as usize;
    if section_count != SECTION_COUNT {
        return Err(PersistentError::InvalidFormat("section count"));
    }
    let generation = read_u64(bytes, 16)?;
    let parent_generation = read_u64(bytes, 24)?;
    let selected_source_bytes = read_u64(bytes, 32)?;
    let searchable_normalized_bytes = read_u64(bytes, 40)?;
    let file_count = read_u32(bytes, 48)? as usize;
    let block_count = read_u32(bytes, 52)? as usize;
    let mut section_map = HashMap::<u32, &[u8]>::new();
    for index in 0..SECTION_COUNT {
        let dir = FIXED_HEADER_BYTES + index * SECTION_DIR_ENTRY_BYTES;
        let id = read_u32(bytes, dir)?;
        let offset = read_u64(bytes, dir + 8)? as usize;
        let length = read_u64(bytes, dir + 16)? as usize;
        let checksum = read_u64(bytes, dir + 24)?;
        let end = offset
            .checked_add(length)
            .ok_or(PersistentError::InvalidFormat("section overflow"))?;
        if offset < HEADER_BYTES || end > footer {
            return Err(PersistentError::InvalidFormat("section bounds"));
        }
        let section = &bytes[offset..end];
        if crc64_ecma(section) != checksum {
            return Err(PersistentError::SectionChecksumMismatch(id));
        }
        section_map.insert(id, section);
    }
    let files = deserialize_file_catalog(required_section(&section_map, SECTION_FILE_CATALOG)?)?;
    let blocks = deserialize_block_map(required_section(&section_map, SECTION_BLOCK_MAP)?)?;
    if files.len() != file_count || blocks.len() != block_count {
        return Err(PersistentError::InvalidFormat("header counts"));
    }
    let words_per_bitmap = block_count.div_ceil(64).max(1);
    let unigram = deserialize_bitmap_rows(
        required_section(&section_map, SECTION_UNIGRAM)?,
        256,
        words_per_bitmap,
        block_count,
    )?;
    let bigram = deserialize_bitmap_rows(
        required_section(&section_map, SECTION_BIGRAM)?,
        65_536,
        words_per_bitmap,
        block_count,
    )?;
    let adaptive_global_presence =
        deserialize_q3_global(required_section(&section_map, SECTION_Q3_GLOBAL)?)?;
    let global_trigram_presence = adaptive_to_dense(&adaptive_global_presence);
    let observed_trigram_count = global_trigram_presence
        .iter()
        .map(|word| word.count_ones() as usize)
        .sum();
    let sparse_anchors = deserialize_q3_sparse(required_section(&section_map, SECTION_Q3_SPARSE)?)?;
    let sparse_lookup = sparse_anchors
        .iter()
        .enumerate()
        .map(|(i, a)| (a.gram, i))
        .collect();
    let (higher_ngram_filter, higher_sparse_anchors) =
        deserialize_q45(required_section(&section_map, SECTION_Q45)?)?;
    let higher_sparse_lookup = higher_sparse_anchors
        .iter()
        .enumerate()
        .map(|(i, a)| (a.key, i))
        .collect();
    let q45_bytes = required_section(&section_map, SECTION_Q45)?.len() as u64;
    Ok(PersistentIndex {
        root: root.to_path_buf(),
        generation,
        parent_generation,
        selected_source_bytes,
        searchable_normalized_bytes,
        files,
        blocks,
        filter: PrototypeIndex {
            block_count,
            words_per_bitmap,
            unigram,
            bigram,
            global_trigram_presence,
            adaptive_global_presence,
            observed_trigram_count,
            sparse_anchors,
            sparse_lookup,
            higher_ngram_filter,
            higher_sparse_anchors,
            higher_sparse_lookup,
        },
        total_index_bytes: bytes.len() as u64,
        q45_bytes,
        verification: None,
    })
}

fn required_section<'a>(map: &'a HashMap<u32, &'a [u8]>, id: u32) -> Result<&'a [u8]> {
    map.get(&id)
        .copied()
        .ok_or(PersistentError::InvalidFormat("missing section"))
}

fn serialize_file_catalog(corpus: &Corpus) -> Vec<u8> {
    let mut out = Vec::new();
    put_u32(&mut out, corpus.files.len() as u32);
    for file in &corpus.files {
        let (encoding, path_bytes) = encode_exact_path(&file.exact_path);
        out.push(encoding);
        out.extend_from_slice(&[0_u8; 3]);
        put_u32(&mut out, path_bytes.len() as u32);
        put_u64(&mut out, file.selected_bytes);
        put_u64(&mut out, file.source_crc64);
        put_u64(&mut out, file.source_modified_ns as u64);
        put_u64(&mut out, (file.source_modified_ns >> 64) as u64);
        out.extend_from_slice(&path_bytes);
    }
    out
}

fn deserialize_file_catalog(bytes: &[u8]) -> Result<Vec<PersistentFileEntry>> {
    let mut cursor = Cursor::new(bytes);
    let count = cursor.u32()? as usize;
    let mut files = Vec::with_capacity(count);
    for _ in 0..count {
        let encoding = cursor.u8()?;
        cursor.skip(3)?;
        let path_len = cursor.u32()? as usize;
        let selected_bytes = cursor.u64()?;
        let source_crc64 = cursor.u64()?;
        let low = cursor.u64()? as u128;
        let high = cursor.u64()? as u128;
        let path = decode_exact_path(encoding, cursor.take(path_len)?)?;
        files.push(PersistentFileEntry {
            path,
            selected_bytes,
            source_crc64,
            source_modified_ns: low | (high << 64),
        });
    }
    cursor.finish()?;
    Ok(files)
}

fn serialize_block_map(corpus: &Corpus) -> Vec<u8> {
    let mut out = Vec::new();
    put_u32(&mut out, corpus.blocks.len() as u32);
    for block in &corpus.blocks {
        put_u64(&mut out, block.searchable_bytes);
        put_u32(&mut out, block.spans.len() as u32);
        for span in &block.spans {
            put_u32(&mut out, span.file_id);
            put_u64(&mut out, span.start);
            put_u64(&mut out, span.end);
            put_u32(&mut out, span.start_line);
            put_u32(&mut out, span.line_count);
        }
    }
    out
}

fn deserialize_block_map(bytes: &[u8]) -> Result<Vec<PersistentBlock>> {
    let mut cursor = Cursor::new(bytes);
    let count = cursor.u32()? as usize;
    let mut blocks = Vec::with_capacity(count);
    for _ in 0..count {
        let searchable_bytes = cursor.u64()?;
        let span_count = cursor.u32()? as usize;
        let mut spans = Vec::with_capacity(span_count);
        for _ in 0..span_count {
            spans.push(PersistentBlockSpan {
                file_id: cursor.u32()?,
                start: cursor.u64()?,
                end: cursor.u64()?,
                start_line: cursor.u32()?,
                line_count: cursor.u32()?,
            });
        }
        blocks.push(PersistentBlock {
            searchable_bytes,
            spans,
        });
    }
    cursor.finish()?;
    Ok(blocks)
}

fn serialize_bitmap_rows(
    rows: &[u64],
    row_count: usize,
    words_per_bitmap: usize,
    block_count: usize,
) -> Vec<u8> {
    let mut out = Vec::new();
    put_u32(&mut out, row_count as u32);
    put_u32(&mut out, block_count as u32);
    append_bitpacked_rows(&mut out, rows, row_count, words_per_bitmap, block_count);
    out
}

fn deserialize_bitmap_rows(
    bytes: &[u8],
    expected_rows: usize,
    words_per_bitmap: usize,
    block_count: usize,
) -> Result<Vec<u64>> {
    let mut cursor = Cursor::new(bytes);
    let rows = cursor.u32()? as usize;
    let blocks = cursor.u32()? as usize;
    if rows != expected_rows || blocks != block_count {
        return Err(PersistentError::InvalidFormat("bitmap dimensions"));
    }
    let bytes_per_row = block_count.div_ceil(8);
    let payload = cursor.take(
        rows.checked_mul(bytes_per_row)
            .ok_or(PersistentError::InvalidFormat("bitmap size"))?,
    )?;
    cursor.finish()?;
    let mut out = vec![0_u64; rows * words_per_bitmap];
    for row in 0..rows {
        for byte_index in 0..bytes_per_row {
            let value = payload[row * bytes_per_row + byte_index];
            for bit in 0..8 {
                let block_id = byte_index * 8 + bit;
                if block_id >= block_count {
                    break;
                }
                if value & (1_u8 << bit) != 0 {
                    set_row_bit(&mut out, words_per_bitmap, row, block_id);
                }
            }
        }
    }
    Ok(out)
}

fn serialize_q3_global(global: &AdaptiveGlobalPresence) -> Vec<u8> {
    let mut out = Vec::new();
    global.serialize_into(&mut out);
    out
}

fn deserialize_q3_global(bytes: &[u8]) -> Result<AdaptiveGlobalPresence> {
    let mut cursor = Cursor::new(bytes);
    let tag = cursor.u8()?;
    cursor.skip(3)?;
    let count = cursor.u32()? as usize;
    let value = match tag {
        1 => AdaptiveGlobalPresence::Dense(
            (0..count)
                .map(|_| cursor.u64())
                .collect::<Result<Vec<_>>>()?,
        ),
        2 => {
            let mut values = Vec::with_capacity(count);
            for _ in 0..count {
                values.push(cursor.u24()?);
            }
            AdaptiveGlobalPresence::PackedU24(values)
        }
        3 => {
            let mut words = Vec::with_capacity(count);
            for _ in 0..count {
                words.push((cursor.u24()?, cursor.u64()?));
            }
            AdaptiveGlobalPresence::SparseWords(words)
        }
        _ => return Err(PersistentError::InvalidFormat("q3 global encoding")),
    };
    cursor.finish()?;
    Ok(value)
}

fn adaptive_to_dense(global: &AdaptiveGlobalPresence) -> Vec<u64> {
    match global {
        AdaptiveGlobalPresence::Dense(words) => words.clone(),
        AdaptiveGlobalPresence::PackedU24(values) => {
            let mut out = vec![0_u64; GLOBAL_TRIGRAM_WORDS];
            for gram in values {
                out[*gram as usize / 64] |= 1_u64 << (*gram % 64);
            }
            out
        }
        AdaptiveGlobalPresence::SparseWords(words) => {
            let mut out = vec![0_u64; GLOBAL_TRIGRAM_WORDS];
            for (index, bits) in words {
                out[*index as usize] = *bits;
            }
            out
        }
    }
}

fn serialize_q3_sparse(anchors: &[SparseAnchor], selected_source_bytes: u64) -> Vec<u8> {
    let budget = ((selected_source_bytes as u128 * Q3_SPARSE_BUDGET_NUMERATOR as u128)
        / SPARSE_BUDGET_DENOMINATOR as u128) as usize;
    let mut candidates = anchors.iter().collect::<Vec<_>>();
    candidates.sort_unstable_by_key(|anchor| (anchor.postings.len(), anchor.gram));
    let mut selected = Vec::new();
    let mut used = 8_usize;
    for anchor in candidates {
        let posting_bytes = delta_varint_len(&anchor.postings);
        let bytes = 16 + posting_bytes;
        if used.saturating_add(bytes) <= budget {
            selected.push(anchor);
            used += bytes;
        }
    }
    let mut out = Vec::new();
    put_u32(&mut out, selected.len() as u32);
    put_u32(&mut out, 0);
    let record_start = out.len();
    out.resize(record_start + selected.len() * 16, 0);
    let mut postings = Vec::new();
    for (index, anchor) in selected.iter().enumerate() {
        let base = record_start + index * 16;
        write_u32_at(&mut out, base, anchor.gram);
        write_u32_at(&mut out, base + 4, anchor.postings.len() as u32);
        write_u64_at(&mut out, base + 8, postings.len() as u64);
        append_delta_varints(&mut postings, &anchor.postings);
    }
    out.extend_from_slice(&postings);
    out
}

fn deserialize_q3_sparse(bytes: &[u8]) -> Result<Vec<SparseAnchor>> {
    let mut cursor = Cursor::new(bytes);
    let count = cursor.u32()? as usize;
    cursor.skip(4)?;
    let records = cursor.take(
        count
            .checked_mul(16)
            .ok_or(PersistentError::InvalidFormat("q3 sparse records"))?,
    )?;
    let posting_blob = cursor.remaining();
    let mut anchors = Vec::with_capacity(count);
    for index in 0..count {
        let base = index * 16;
        let gram = read_u32(records, base)?;
        let df = read_u32(records, base + 4)? as usize;
        let offset = read_u64(records, base + 8)? as usize;
        anchors.push(SparseAnchor {
            gram,
            postings: decode_delta_varints(posting_blob, offset, df)?,
        });
    }
    Ok(anchors)
}

fn serialize_q45(
    filter: &HigherNgramBloom,
    anchors: &[HigherSparseAnchor],
    selected_source_bytes: u64,
) -> Vec<u8> {
    let mut out = Vec::new();
    put_u32(&mut out, filter.words.len() as u32);
    put_u32(&mut out, HIGHER_BLOOM_HASHES as u32);
    put_u64(&mut out, filter.bit_count as u64);
    let sparse_budget = ((selected_source_bytes as u128 * HIGHER_SPARSE_BUDGET_NUMERATOR as u128)
        / SPARSE_BUDGET_DENOMINATOR as u128) as usize;
    let mut candidates = anchors.iter().collect::<Vec<_>>();
    candidates.sort_unstable_by_key(|anchor| {
        (anchor.postings.len(), stable_mix64(anchor.key), anchor.key)
    });
    let mut selected = Vec::new();
    let mut sparse_used = 0_usize;
    for anchor in candidates {
        let posting_bytes = delta_varint_len(&anchor.postings);
        let bytes = 24 + posting_bytes;
        if sparse_used.saturating_add(bytes) <= sparse_budget {
            selected.push(anchor);
            sparse_used += bytes;
        }
    }
    put_u32(&mut out, selected.len() as u32);
    put_u32(&mut out, 0);
    for word in &filter.words {
        put_u64(&mut out, *word);
    }
    let record_start = out.len();
    out.resize(record_start + selected.len() * 24, 0);
    let mut postings = Vec::new();
    for (index, anchor) in selected.iter().enumerate() {
        let base = record_start + index * 24;
        write_u64_at(&mut out, base, anchor.key);
        write_u32_at(&mut out, base + 8, anchor.postings.len() as u32);
        write_u32_at(&mut out, base + 12, 0);
        write_u64_at(&mut out, base + 16, postings.len() as u64);
        append_delta_varints(&mut postings, &anchor.postings);
    }
    out.extend_from_slice(&postings);
    out
}

fn deserialize_q45(bytes: &[u8]) -> Result<(HigherNgramBloom, Vec<HigherSparseAnchor>)> {
    let mut cursor = Cursor::new(bytes);
    let word_count = cursor.u32()? as usize;
    let hashes = cursor.u32()? as usize;
    if hashes != HIGHER_BLOOM_HASHES {
        return Err(PersistentError::InvalidFormat("q45 bloom hashes"));
    }
    let bit_count = cursor.u64()? as usize;
    let anchor_count = cursor.u32()? as usize;
    cursor.skip(4)?;
    let mut words = Vec::with_capacity(word_count);
    for _ in 0..word_count {
        words.push(cursor.u64()?);
    }
    if bit_count != words.len() * 64 {
        return Err(PersistentError::InvalidFormat("q45 bloom bits"));
    }
    let records = cursor.take(
        anchor_count
            .checked_mul(24)
            .ok_or(PersistentError::InvalidFormat("q45 records"))?,
    )?;
    let posting_blob = cursor.remaining();
    let mut anchors = Vec::with_capacity(anchor_count);
    for index in 0..anchor_count {
        let base = index * 24;
        let key = read_u64(records, base)?;
        let df = read_u32(records, base + 8)? as usize;
        let offset = read_u64(records, base + 16)? as usize;
        anchors.push(HigherSparseAnchor {
            key,
            postings: decode_delta_varints(posting_blob, offset, df)?,
        });
    }
    Ok((HigherNgramBloom { words, bit_count }, anchors))
}

fn decode_delta_varints(bytes: &[u8], mut offset: usize, count: usize) -> Result<Vec<u32>> {
    if offset > bytes.len() {
        return Err(PersistentError::InvalidFormat("posting offset"));
    }
    let mut out = Vec::with_capacity(count);
    let mut previous = 0_u32;
    for index in 0..count {
        let mut value = 0_u32;
        let mut shift = 0_u32;
        loop {
            let byte = *bytes
                .get(offset)
                .ok_or(PersistentError::InvalidFormat("truncated varint"))?;
            offset += 1;
            value |= ((byte & 0x7f) as u32) << shift;
            if byte & 0x80 == 0 {
                break;
            }
            shift += 7;
            if shift >= 35 {
                return Err(PersistentError::InvalidFormat("varint overflow"));
            }
        }
        let absolute = if index == 0 {
            value
        } else {
            previous
                .checked_add(value)
                .ok_or(PersistentError::InvalidFormat("posting overflow"))?
        };
        out.push(absolute);
        previous = absolute;
    }
    Ok(out)
}

fn generation_file_name(generation: u64) -> String {
    format!("gen-{generation:020}.prv2")
}

fn is_generation_file_name(name: &str) -> bool {
    name.starts_with("gen-") && name.ends_with(".prv2") && name.len() == 29
}

fn generation_from_path(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    if !is_generation_file_name(name) {
        return None;
    }
    name[4..24].parse().ok()
}

fn generation_paths_desc(store_dir: &Path) -> Result<Vec<PathBuf>> {
    if !store_dir.exists() {
        return Ok(Vec::new());
    }
    let mut paths = fs::read_dir(store_dir)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| generation_from_path(path).is_some())
        .collect::<Vec<_>>();
    paths.sort_unstable_by_key(|path| std::cmp::Reverse(generation_from_path(path).unwrap_or(0)));
    Ok(paths)
}

fn valid_generation_numbers(store_dir: &Path) -> Result<Vec<u64>> {
    let mut valid = Vec::new();
    for path in generation_paths_desc(store_dir)? {
        if let Ok(bytes) = fs::read(&path)
            && deserialize_production(Path::new("."), &bytes).is_ok()
            && let Some(generation) = generation_from_path(&path)
        {
            valid.push(generation);
        }
    }
    valid.sort_unstable_by(|a, b| b.cmp(a));
    Ok(valid)
}

fn atomic_write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or(PersistentError::InvalidFormat("path has no parent"))?;
    fs::create_dir_all(parent)?;
    if path.exists() {
        return Err(PersistentError::Io(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "immutable generation already exists",
        )));
    }
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("index");
    let temp = parent.join(format!(".{name}.tmp-{}", std::process::id()));
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&temp, path)?;
    Ok(())
}

fn write_advisory_current(store_dir: &Path, generation_file: &str) -> Result<()> {
    let path = store_dir.join("CURRENT");
    let temp = store_dir.join(format!(".CURRENT.tmp-{}", std::process::id()));
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)?;
        writeln!(file, "{generation_file}")?;
        file.sync_all()?;
    }
    #[cfg(windows)]
    if path.exists() {
        // CURRENT is advisory. If the process dies between remove and rename,
        // load_latest() safely falls back to scanning immutable generations.
        fs::remove_file(&path)?;
    }
    fs::rename(&temp, &path)?;
    Ok(())
}

fn current_valid_generation(store_dir: &Path) -> Result<Option<u64>> {
    let Ok(current) = fs::read_to_string(store_dir.join("CURRENT")) else {
        return Ok(None);
    };
    let name = current.trim();
    if !is_generation_file_name(name) {
        return Ok(None);
    }
    let path = store_dir.join(name);
    let Ok(bytes) = fs::read(&path) else {
        return Ok(None);
    };
    if deserialize_production(Path::new("."), &bytes).is_err() {
        return Ok(None);
    }
    Ok(generation_from_path(&path))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

pub(crate) fn metadata_modified_ns(metadata: &fs::Metadata) -> u128 {
    metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
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

fn decode_exact_path(encoding: u8, bytes: &[u8]) -> Result<PathBuf> {
    match encoding {
        PATH_ENCODING_UNIX_RAW => decode_unix_path(bytes),
        PATH_ENCODING_WINDOWS_UTF16LE => decode_windows_path(bytes),
        PATH_ENCODING_UTF8_FALLBACK => {
            Ok(PathBuf::from(std::str::from_utf8(bytes).map_err(|_| {
                PersistentError::InvalidFormat("utf8 path")
            })?))
        }
        _ => Err(PersistentError::InvalidFormat("path encoding")),
    }
}

#[cfg(unix)]
fn decode_unix_path(bytes: &[u8]) -> Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
}
#[cfg(not(unix))]
fn decode_unix_path(_bytes: &[u8]) -> Result<PathBuf> {
    Err(PersistentError::InvalidFormat("unix path on non-unix"))
}

#[cfg(windows)]
fn decode_windows_path(bytes: &[u8]) -> Result<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    if !bytes.len().is_multiple_of(2) {
        return Err(PersistentError::InvalidFormat("utf16 path"));
    }
    let values = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    Ok(PathBuf::from(OsString::from_wide(&values)))
}
#[cfg(not(windows))]
fn decode_windows_path(_bytes: &[u8]) -> Result<PathBuf> {
    Err(PersistentError::InvalidFormat(
        "windows path on non-windows",
    ))
}

pub(crate) fn crc64_ecma(bytes: &[u8]) -> u64 {
    use std::sync::OnceLock;
    static TABLE: OnceLock<[u64; 256]> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut table = [0_u64; 256];
        for (value, slot) in table.iter_mut().enumerate() {
            let mut crc = (value as u64) << 56;
            for _ in 0..8 {
                crc = if crc & 0x8000_0000_0000_0000 != 0 {
                    (crc << 1) ^ 0x42F0_E1EB_A9EA_3693
                } else {
                    crc << 1
                };
            }
            *slot = crc;
        }
        table
    });
    let mut crc = 0_u64;
    for byte in bytes {
        let index = ((crc >> 56) as u8 ^ *byte) as usize;
        crc = table[index] ^ (crc << 8);
    }
    crc
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}
impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }
    fn u8(&mut self) -> Result<u8> {
        let value = *self
            .bytes
            .get(self.pos)
            .ok_or(PersistentError::InvalidFormat("truncated u8"))?;
        self.pos += 1;
        Ok(value)
    }
    fn u24(&mut self) -> Result<u32> {
        let bytes = self.take(3)?;
        Ok(bytes[0] as u32 | ((bytes[1] as u32) << 8) | ((bytes[2] as u32) << 16))
    }
    fn u32(&mut self) -> Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes(bytes.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64> {
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes(bytes.try_into().unwrap()))
    }
    fn skip(&mut self, len: usize) -> Result<()> {
        self.take(len).map(|_| ())
    }
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or(PersistentError::InvalidFormat("cursor overflow"))?;
        let out = self
            .bytes
            .get(self.pos..end)
            .ok_or(PersistentError::InvalidFormat("truncated section"))?;
        self.pos = end;
        Ok(out)
    }
    fn remaining(&self) -> &'a [u8] {
        &self.bytes[self.pos..]
    }
    fn finish(&self) -> Result<()> {
        if self.pos == self.bytes.len() {
            Ok(())
        } else {
            Err(PersistentError::InvalidFormat("trailing section bytes"))
        }
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let end = offset
        .checked_add(4)
        .ok_or(PersistentError::InvalidFormat("u32 overflow"))?;
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..end)
            .ok_or(PersistentError::InvalidFormat("truncated u32"))?
            .try_into()
            .unwrap(),
    ))
}
fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let end = offset
        .checked_add(8)
        .ok_or(PersistentError::InvalidFormat("u64 overflow"))?;
    Ok(u64::from_le_bytes(
        bytes
            .get(offset..end)
            .ok_or(PersistentError::InvalidFormat("truncated u64"))?
            .try_into()
            .unwrap(),
    ))
}
fn write_u32_at(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn write_u64_at(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

#[allow(dead_code)]
fn _system_time_ns(value: SystemTime) -> u128 {
    value
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

#[cfg(test)]
mod crc_tests {
    use super::crc64_ecma;

    #[test]
    fn crc64_ecma_matches_standard_check_vector() {
        assert_eq!(crc64_ecma(b"123456789"), 0x6c40_df5f_0b49_7347);
    }
}

#[cfg(test)]
mod format_version_tests {
    use super::*;

    #[test]
    fn frozen_unicode_semantics_reject_prior_format_v1() {
        let corpus = Corpus::from_documents([("a.txt", "Straße\n".as_bytes().to_vec())]);
        let filter = PrototypeIndex::build(&corpus);
        let (mut bytes, _) = serialize_production(&corpus, &filter, 1, 0).unwrap();
        write_u32_at(&mut bytes, 8, 1);
        let error = deserialize_production(Path::new("."), &bytes).unwrap_err();
        assert!(matches!(error, PersistentError::FormatVersion { found: 1 }));
    }
}
