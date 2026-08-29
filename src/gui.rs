use crate::extraction::{ExtractorConfig, extract_document, is_extractable_document};
use crate::incremental::{
    ContentQueryKind, ContentSearchOptions, LoadedBundle, load_bundle_with_verification,
};
use crate::metadata::{MetadataFileKind, MetadataSearchRequest};
use crate::{SearchLimits, normalize_str};
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const GUI_FIRST_BATCH_FILES: usize = 100;
pub const GUI_PREVIEW_CHARS: usize = 180;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuiFileScope {
    Filename,
    FullPath,
}

impl GuiFileScope {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Filename => "filename",
            Self::FullPath => "full-path",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuiContentMode {
    Literal,
    Regex,
    Wildcard,
}

impl GuiContentMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Literal => "literal",
            Self::Regex => "regex",
            Self::Wildcard => "wildcard",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuiSearchRequest {
    pub file_query: String,
    pub content_query: String,
    pub file_scope: GuiFileScope,
    pub content_mode: GuiContentMode,
    pub case_sensitive: bool,
    pub max_files: usize,
}

impl Default for GuiSearchRequest {
    fn default() -> Self {
        Self {
            file_query: String::new(),
            content_query: String::new(),
            file_scope: GuiFileScope::Filename,
            content_mode: GuiContentMode::Literal,
            case_sensitive: false,
            max_files: GUI_FIRST_BATCH_FILES,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuiMatch {
    pub line_number: u32,
    pub byte_offset: u32,
    pub location: String,
    pub preview: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuiResultRow {
    pub file_id: u64,
    pub name: String,
    pub relative_path: PathBuf,
    pub absolute_path: PathBuf,
    pub size: u64,
    pub modified_ns: u128,
    pub kind: MetadataFileKind,
    pub matches: Vec<GuiMatch>,
}

impl GuiResultRow {
    pub fn visible_match_count(&self) -> usize {
        self.matches.len()
    }

    pub fn primary_location(&self) -> &str {
        self.matches
            .first()
            .map_or("", |value| value.location.as_str())
    }

    pub fn primary_preview(&self) -> &str {
        self.matches
            .first()
            .map_or("", |value| value.preview.as_str())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GuiSearchStats {
    pub elapsed: Duration,
    pub candidate_content_hits: usize,
    pub returned_files: usize,
    pub bundle_generation: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GuiSearchResponse {
    pub rows: Vec<GuiResultRow>,
    pub stats: GuiSearchStats,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuiIndexStatus {
    pub root: PathBuf,
    pub store: PathBuf,
    pub bundle_generation: u64,
    pub content_generation: u64,
    pub metadata_generation: u64,
    pub delta_generation: u64,
    pub state_generation: u64,
    pub metadata_records: usize,
    pub delta_changes: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuiError(String);

impl GuiError {
    fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl fmt::Display for GuiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for GuiError {}

pub struct GuiSearchSession {
    root: PathBuf,
    store: PathBuf,
    extractor: ExtractorConfig,
    bundle: LoadedBundle,
}

impl GuiSearchSession {
    pub fn load(
        root: impl AsRef<Path>,
        store: impl AsRef<Path>,
        extractor: ExtractorConfig,
    ) -> Result<Self, GuiError> {
        let root = root.as_ref().to_path_buf();
        let store = store.as_ref().to_path_buf();
        let bundle = load_bundle_with_verification(&root, &store, &extractor)
            .map_err(|error| GuiError::new(format!("failed to load Step 1-5 bundle: {error}")))?;
        Ok(Self {
            root,
            store,
            extractor,
            bundle,
        })
    }

    pub fn reload(&mut self) -> Result<GuiIndexStatus, GuiError> {
        self.bundle = load_bundle_with_verification(&self.root, &self.store, &self.extractor)
            .map_err(|error| GuiError::new(format!("failed to reload Step 1-5 bundle: {error}")))?;
        Ok(self.status())
    }

    pub fn status(&self) -> GuiIndexStatus {
        GuiIndexStatus {
            root: self.root.clone(),
            store: self.store.clone(),
            bundle_generation: self.bundle.manifest.generation,
            content_generation: self.bundle.manifest.content_generation,
            metadata_generation: self.bundle.manifest.metadata_generation,
            delta_generation: self.bundle.manifest.delta_generation,
            state_generation: self.bundle.manifest.state_generation,
            metadata_records: self.bundle.metadata.records().len(),
            delta_changes: self.bundle.delta.change_count(),
        }
    }

    pub fn search(&self, request: &GuiSearchRequest) -> Result<GuiSearchResponse, GuiError> {
        let started = Instant::now();
        let file_query = request.file_query.trim();
        let content_query = request.content_query.trim();
        let max_files = request.max_files.max(1);
        if file_query.is_empty() && content_query.is_empty() {
            return Ok(GuiSearchResponse {
                rows: Vec::new(),
                stats: GuiSearchStats {
                    elapsed: started.elapsed(),
                    candidate_content_hits: 0,
                    returned_files: 0,
                    bundle_generation: self.bundle.manifest.generation,
                },
            });
        }

        let mut preview_cache = HashMap::<u64, Vec<String>>::new();
        let (mut rows, candidate_content_hits) = if content_query.is_empty() {
            (self.search_metadata(file_query, request, max_files), 0)
        } else {
            self.search_content(
                file_query,
                content_query,
                request,
                max_files,
                &mut preview_cache,
            )?
        };

        rows.sort_by(|left, right| {
            left.name
                .to_lowercase()
                .cmp(&right.name.to_lowercase())
                .then_with(|| left.relative_path.cmp(&right.relative_path))
                .then_with(|| left.file_id.cmp(&right.file_id))
        });
        rows.truncate(max_files);
        let returned_files = rows.len();
        Ok(GuiSearchResponse {
            rows,
            stats: GuiSearchStats {
                elapsed: started.elapsed(),
                candidate_content_hits,
                returned_files,
                bundle_generation: self.bundle.manifest.generation,
            },
        })
    }

    fn search_metadata(
        &self,
        file_query: &str,
        request: &GuiSearchRequest,
        max_files: usize,
    ) -> Vec<GuiResultRow> {
        let (filename, full_path) = match request.file_scope {
            GuiFileScope::Filename => (Some(file_query), None),
            GuiFileScope::FullPath => (None, Some(file_query)),
        };
        let hits = self.bundle.delta.metadata_search(
            &self.bundle.metadata,
            MetadataSearchRequest {
                filename,
                full_path,
                case_sensitive: request.case_sensitive,
                max_results: max_files,
            },
        );
        hits.into_iter()
            .filter_map(|hit| {
                self.bundle
                    .delta
                    .current_record(&self.bundle.metadata, hit.file_id)
                    .map(|record| self.row_from_record(record, Vec::new()))
            })
            .collect()
    }

    fn search_content(
        &self,
        file_query: &str,
        content_query: &str,
        request: &GuiSearchRequest,
        max_files: usize,
        preview_cache: &mut HashMap<u64, Vec<String>>,
    ) -> Result<(Vec<GuiResultRow>, usize), GuiError> {
        let query = match request.content_mode {
            GuiContentMode::Literal => ContentQueryKind::Literal(content_query),
            GuiContentMode::Regex => ContentQueryKind::Regex(content_query),
            GuiContentMode::Wildcard => ContentQueryKind::Wildcard(content_query),
        };
        let hits = self
            .bundle
            .delta
            .content_search_with_limits_with_extraction(
                &self.root,
                &self.bundle.metadata,
                &self.bundle.content,
                ContentSearchOptions {
                    query,
                    case_sensitive: request.case_sensitive,
                    limits: SearchLimits {
                        max_files,
                        max_matches_seen: max_files.saturating_mul(5).max(500),
                        max_snippets_per_file: 3,
                    },
                },
                &self.extractor,
            )
            .map_err(|error| GuiError::new(format!("content search failed: {error}")))?;
        let candidate_content_hits = hits.len();
        let mut rows = Vec::<GuiResultRow>::new();
        let mut row_by_file = HashMap::<u64, usize>::new();
        for hit in hits {
            let Some(record) = self
                .bundle
                .delta
                .current_record(&self.bundle.metadata, hit.file_id)
            else {
                continue;
            };
            if !file_query.is_empty()
                && !gui_file_query_matches(
                    &record.path,
                    file_query,
                    request.file_scope,
                    request.case_sensitive,
                )
            {
                continue;
            }
            let matched = self.preview_match(
                record.file_id,
                &record.path,
                hit.line_number,
                hit.byte_offset_in_line,
                preview_cache,
            );
            let index = if let Some(index) = row_by_file.get(&record.file_id).copied() {
                index
            } else {
                if rows.len() >= max_files {
                    break;
                }
                let index = rows.len();
                rows.push(self.row_from_record(record, Vec::new()));
                row_by_file.insert(record.file_id, index);
                index
            };
            rows[index].matches.push(matched);
        }
        Ok((rows, candidate_content_hits))
    }

    fn row_from_record(
        &self,
        record: &crate::metadata::MetadataRecord,
        matches: Vec<GuiMatch>,
    ) -> GuiResultRow {
        let name = record
            .path
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| record.path.to_string_lossy().into_owned());
        GuiResultRow {
            file_id: record.file_id,
            name,
            relative_path: record.path.clone(),
            absolute_path: self.root.join(&record.path),
            size: record.size,
            modified_ns: record.modified_ns,
            kind: record.kind,
            matches,
        }
    }

    fn preview_match(
        &self,
        file_id: u64,
        relative_path: &Path,
        line_number: u32,
        byte_offset: u32,
        cache: &mut HashMap<u64, Vec<String>>,
    ) -> GuiMatch {
        let lines = cache.entry(file_id).or_insert_with(|| {
            self.load_logical_units(relative_path)
                .unwrap_or_else(|error| vec![format!("[preview unavailable: {error}]")])
        });
        let text = lines
            .get(line_number.saturating_sub(1) as usize)
            .map(String::as_str)
            .unwrap_or("");
        let preview = context_preview(text, byte_offset as usize, GUI_PREVIEW_CHARS);
        let location = if is_extractable_document(relative_path) {
            format!("Unit {line_number} · byte {byte_offset}")
        } else {
            format!("Line {line_number} · byte {byte_offset}")
        };
        GuiMatch {
            line_number,
            byte_offset,
            location,
            preview,
        }
    }

    fn load_logical_units(&self, relative_path: &Path) -> Result<Vec<String>, GuiError> {
        let source = self.root.join(relative_path);
        if is_extractable_document(relative_path) {
            return extract_document(&source, &self.extractor)
                .map(|document| document.units)
                .map_err(|error| GuiError::new(error.to_string()));
        }
        let bytes = fs::read(&source).map_err(|error| GuiError::new(error.to_string()))?;
        let text = String::from_utf8_lossy(&bytes);
        Ok(text.lines().map(str::to_owned).collect())
    }
}

pub fn gui_file_query_matches(
    path: &Path,
    query: &str,
    scope: GuiFileScope,
    case_sensitive: bool,
) -> bool {
    let text = match scope {
        GuiFileScope::Filename => path.file_name().and_then(|value| value.to_str()),
        GuiFileScope::FullPath => path.to_str(),
    };
    let Some(text) = text else {
        return false;
    };
    let (text, query) = if scope == GuiFileScope::FullPath {
        (
            canonicalize_separators(text),
            canonicalize_separators(query),
        )
    } else {
        (text.to_owned(), query.to_owned())
    };
    let haystack = normalize_str(&text, case_sensitive);
    let needle = normalize_str(&query, case_sensitive);
    contains_subslice(haystack.bytes(), needle.bytes())
}

fn canonicalize_separators(value: &str) -> String {
    if value.contains('\\') {
        value.replace('\\', "/")
    } else {
        value.to_owned()
    }
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    needle.is_empty()
        || needle.len() <= haystack.len()
            && haystack
                .windows(needle.len())
                .any(|window| window == needle)
}

pub fn format_file_size(bytes: u64) -> String {
    const UNITS: [(&str, u64); 4] = [
        ("GiB", 1 << 30),
        ("MiB", 1 << 20),
        ("KiB", 1 << 10),
        ("B", 1),
    ];
    for (unit, divisor) in UNITS {
        if bytes >= divisor || divisor == 1 {
            if divisor == 1 {
                return format!("{bytes} B");
            }
            return format!("{:.1} {unit}", bytes as f64 / divisor as f64);
        }
    }
    "0 B".to_string()
}

pub fn format_modified_ns_utc(modified_ns: u128) -> String {
    if modified_ns == 0 {
        return "—".to_string();
    }
    let seconds = (modified_ns / 1_000_000_000).min(i64::MAX as u128) as i64;
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = seconds_of_day % 3_600 / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    (year, month, day)
}

pub fn context_preview(text: &str, byte_offset: usize, max_chars: usize) -> String {
    if text.is_empty() || max_chars == 0 {
        return String::new();
    }
    let mut center = byte_offset.min(text.len());
    while center > 0 && !text.is_char_boundary(center) {
        center -= 1;
    }
    let chars = text.char_indices().collect::<Vec<_>>();
    let center_char = chars
        .partition_point(|(offset, _)| *offset < center)
        .min(chars.len());
    let half = max_chars / 2;
    let start_char = center_char.saturating_sub(half);
    let end_char = (start_char + max_chars).min(chars.len());
    let start = chars.get(start_char).map_or(text.len(), |value| value.0);
    let end = chars.get(end_char).map_or(text.len(), |value| value.0);
    let mut result = String::new();
    if start > 0 {
        result.push('…');
    }
    result.push_str(&text[start..end]);
    if end < text.len() {
        result.push('…');
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_query_uses_frozen_unicode_and_separator_semantics() {
        let path = Path::new("Docs\\Cafe\u{301}/Report.TXT");
        assert!(gui_file_query_matches(
            path,
            "Café/REPORT",
            GuiFileScope::FullPath,
            false
        ));
        assert!(!gui_file_query_matches(
            path,
            "report.txt",
            GuiFileScope::Filename,
            true
        ));
        assert!(gui_file_query_matches(
            path,
            "Report.TXT",
            GuiFileScope::Filename,
            true
        ));
    }

    #[test]
    fn file_size_and_modified_time_are_human_readable() {
        assert_eq!(format_file_size(0), "0 B");
        assert_eq!(format_file_size(1536), "1.5 KiB");
        assert_eq!(format_modified_ns_utc(0), "—");
        assert_eq!(
            format_modified_ns_utc(1_609_459_200_000_000_000),
            "2021-01-01 00:00:00Z"
        );
    }

    #[test]
    fn context_preview_is_utf8_safe_and_bounded() {
        let text = "0123456789日本語abcdefghij";
        let offset = text.find("日本").unwrap() + 1;
        let preview = context_preview(text, offset, 10);
        assert!(preview.contains("日本語"));
        assert!(preview.chars().count() <= 12);
    }
}

#[cfg(test)]
mod send_tests {
    use super::GuiSearchSession;

    fn assert_send<T: Send>() {}

    #[test]
    fn gui_session_can_live_on_background_search_thread() {
        assert_send::<GuiSearchSession>();
    }
}
