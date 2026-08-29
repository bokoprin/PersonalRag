use crate::metadata::{
    METADATA_FIRST_BATCH_DEFAULT, MetadataFileKind, MetadataIndex, MetadataRecord,
    MetadataSearchRequest,
};
use crate::persistent::{
    PersistentError, PersistentIndex, PersistentSearchOverlay, crc64_ecma, load_generation,
    publish_next_generation,
};
use crate::usn::{PendingRenameState, UsnCheckpoint};
use crate::{Corpus, PrototypeIndex, PrototypeVariant, SearchHit, normalize_str};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub const DELTA_MAGIC: &[u8; 8] = b"PRV2DEL1";
pub const DELTA_FORMAT_VERSION: u32 = 1;
pub const DELTA_SEMANTIC_ID: u32 = 0x0003_0001;
pub const BUNDLE_MAGIC: &[u8; 8] = b"PRV2BND1";
pub const INCREMENTAL_STATE_MAGIC: &[u8; 8] = b"PRV2INC1";
pub const INCREMENTAL_STATE_FORMAT_VERSION: u32 = 1;
pub const BUNDLE_FORMAT_VERSION: u32 = 1;
pub const DEFAULT_COMPACTION_MAX_CHANGES: usize = 50_000;
pub const DEFAULT_COMPACTION_PERCENT: usize = 2;
const DELTA_HEADER_BYTES: usize = 64;
const DELTA_RECORD_BYTES: usize = 64;
const BUNDLE_BYTES: usize = 72;
const STATE_HEADER_BYTES: usize = 64;
const STATE_PENDING_RECORD_BYTES: usize = 40;
const PATH_ENCODING_UNIX_RAW: u8 = 1;
const PATH_ENCODING_WINDOWS_UTF16LE: u8 = 2;
const PATH_ENCODING_UTF8_FALLBACK: u8 = 3;

#[derive(Debug)]
pub enum IncrementalError {
    Io(io::Error),
    Metadata(crate::metadata::MetadataError),
    Persistent(PersistentError),
    Extraction(crate::extraction::ExtractionError),
    Pattern(crate::PatternError),
    InvalidFormat(&'static str),
    FormatVersion(u32),
    SemanticVersion(u32),
    ChecksumMismatch,
    NoValidDelta,
    NoValidBundle,
    MissingBaseRecord(u64),
}

impl fmt::Display for IncrementalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(value) => write!(f, "I/O error: {value}"),
            Self::Metadata(value) => write!(f, "metadata error: {value}"),
            Self::Persistent(value) => write!(f, "persistent error: {value}"),
            Self::Extraction(value) => write!(f, "extraction error: {value}"),
            Self::Pattern(value) => write!(f, "pattern error: {value}"),
            Self::InvalidFormat(value) => write!(f, "invalid incremental format: {value}"),
            Self::FormatVersion(value) => {
                write!(f, "unsupported incremental format version: {value}")
            }
            Self::SemanticVersion(value) => {
                write!(f, "unsupported incremental semantic version: {value}")
            }
            Self::ChecksumMismatch => write!(f, "incremental checksum mismatch"),
            Self::NoValidDelta => write!(f, "no valid delta generation"),
            Self::NoValidBundle => write!(f, "no valid bundle generation"),
            Self::MissingBaseRecord(value) => write!(f, "missing base record for file id {value}"),
        }
    }
}

impl std::error::Error for IncrementalError {}
impl From<io::Error> for IncrementalError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<crate::metadata::MetadataError> for IncrementalError {
    fn from(value: crate::metadata::MetadataError) -> Self {
        Self::Metadata(value)
    }
}
impl From<PersistentError> for IncrementalError {
    fn from(value: PersistentError) -> Self {
        Self::Persistent(value)
    }
}

impl From<crate::extraction::ExtractionError> for IncrementalError {
    fn from(value: crate::extraction::ExtractionError) -> Self {
        Self::Extraction(value)
    }
}

type Result<T> = std::result::Result<T, IncrementalError>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeltaUpsert {
    pub record: MetadataRecord,
    pub content_changed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeltaSnapshot {
    pub generation: u64,
    pub parent_generation: u64,
    pub upserts: Vec<DeltaUpsert>,
    pub tombstones: Vec<u64>,
}

#[derive(Clone, Debug)]
pub struct DeltaOverlay {
    generation: u64,
    parent_generation: u64,
    base_id_to_index: HashMap<u64, u32>,
    upserts: HashMap<u64, DeltaUpsert>,
    tombstones: HashSet<u64>,
    path_owners: HashMap<PathKey, u64>,
    content_mapping_cache: RefCell<Option<(u64, ContentFileMapping)>>,
    changed_content_cache: RefCell<Option<DeltaContentCache>>,
}

#[derive(Clone, Debug)]
struct ContentFileMapping {
    stable_to_internal: Vec<(u64, u32)>,
    internal_to_stable: Vec<Option<u64>>,
}

impl ContentFileMapping {
    fn internal_for_stable(&self, stable_id: u64) -> Option<u32> {
        self.stable_to_internal
            .binary_search_by_key(&stable_id, |value| value.0)
            .ok()
            .map(|index| self.stable_to_internal[index].1)
    }
}

#[derive(Clone, Debug)]
struct DeltaContentCache {
    corpus: Corpus,
    index: PrototypeIndex,
    stable_ids: Vec<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct PathKey(Vec<u8>);

impl PathKey {
    fn from_path(path: &Path) -> Self {
        Self(exact_path_bytes(path))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncrementalMetadataHit {
    pub file_id: u64,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncrementalContentHit {
    pub file_id: u64,
    pub line_number: u32,
    pub byte_offset_in_line: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentQueryKind<'a> {
    Literal(&'a str),
    Regex(&'a str),
    Wildcard(&'a str),
}

impl DeltaOverlay {
    pub fn new(base: &MetadataIndex, generation: u64, parent_generation: u64) -> Self {
        let base_id_to_index = base
            .records()
            .iter()
            .enumerate()
            .map(|(index, record)| (record.file_id, index as u32))
            .collect();
        Self {
            generation,
            parent_generation,
            base_id_to_index,
            upserts: HashMap::new(),
            tombstones: HashSet::new(),
            path_owners: HashMap::new(),
            content_mapping_cache: RefCell::new(None),
            changed_content_cache: RefCell::new(None),
        }
    }

    pub fn from_snapshot(base: &MetadataIndex, snapshot: DeltaSnapshot) -> Self {
        let mut overlay = Self::new(base, snapshot.generation, snapshot.parent_generation);
        overlay.tombstones.extend(snapshot.tombstones);
        for upsert in snapshot.upserts {
            overlay.path_owners.insert(
                PathKey::from_path(&upsert.record.path),
                upsert.record.file_id,
            );
            overlay.upserts.insert(upsert.record.file_id, upsert);
        }
        overlay
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn parent_generation(&self) -> u64 {
        self.parent_generation
    }
    pub fn upsert_count(&self) -> usize {
        self.upserts.len()
    }
    pub fn tombstone_count(&self) -> usize {
        self.tombstones.len()
    }
    pub fn change_count(&self) -> usize {
        self.upserts.len().saturating_add(self.tombstones.len())
    }

    pub fn should_compact(&self, base_count: usize) -> bool {
        let percent_threshold = base_count
            .saturating_mul(DEFAULT_COMPACTION_PERCENT)
            .div_ceil(100)
            .max(1);
        self.change_count() >= DEFAULT_COMPACTION_MAX_CHANGES
            || self.change_count() >= percent_threshold
    }

    pub fn current_record<'a>(
        &'a self,
        base: &'a MetadataIndex,
        file_id: u64,
    ) -> Option<&'a MetadataRecord> {
        if self.tombstones.contains(&file_id) {
            return None;
        }
        if let Some(value) = self.upserts.get(&file_id) {
            return Some(&value.record);
        }
        self.base_id_to_index
            .get(&file_id)
            .and_then(|index| base.records().get(*index as usize))
    }

    fn current_owner_for_path(&self, base: &MetadataIndex, path: &Path) -> Option<u64> {
        let key = PathKey::from_path(path);
        if let Some(owner) = self.path_owners.get(&key).copied() {
            return (!self.tombstones.contains(&owner)).then_some(owner);
        }
        base_owner_for_exact_path(base, path).filter(|owner| {
            if self.tombstones.contains(owner) {
                return false;
            }
            if let Some(upsert) = self.upserts.get(owner) {
                return upsert.record.path == path;
            }
            true
        })
    }

    pub fn upsert(&mut self, base: &MetadataIndex, record: MetadataRecord, content_changed: bool) {
        if content_changed {
            *self.changed_content_cache.get_mut() = None;
        }
        let file_id = record.file_id;
        if let Some(previous) = self.upserts.get(&file_id) {
            let previous_key = PathKey::from_path(&previous.record.path);
            if self.path_owners.get(&previous_key) == Some(&file_id) {
                self.path_owners.remove(&previous_key);
            }
        }
        if let Some(other) = self.current_owner_for_path(base, &record.path)
            && other != file_id
        {
            if self.base_id_to_index.contains_key(&other) {
                self.tombstones.insert(other);
            }
            if let Some(previous) = self.upserts.remove(&other) {
                let previous_key = PathKey::from_path(&previous.record.path);
                if self.path_owners.get(&previous_key) == Some(&other) {
                    self.path_owners.remove(&previous_key);
                }
            }
        }
        self.tombstones.remove(&file_id);
        self.path_owners
            .insert(PathKey::from_path(&record.path), file_id);
        let combined_changed = content_changed
            || self
                .upserts
                .get(&file_id)
                .is_some_and(|value| value.content_changed);
        self.upserts.insert(
            file_id,
            DeltaUpsert {
                record,
                content_changed: combined_changed,
            },
        );
    }

    pub fn delete(&mut self, base: &MetadataIndex, file_id: u64) {
        if let Some(previous) = self.upserts.remove(&file_id) {
            if previous.content_changed {
                *self.changed_content_cache.get_mut() = None;
            }
            let key = PathKey::from_path(&previous.record.path);
            if self.path_owners.get(&key) == Some(&file_id) {
                self.path_owners.remove(&key);
            }
        }
        if self.base_id_to_index.contains_key(&file_id)
            || base
                .records()
                .iter()
                .any(|record| record.file_id == file_id)
        {
            self.tombstones.insert(file_id);
        } else {
            self.tombstones.remove(&file_id);
        }
    }

    pub fn rename(&mut self, base: &MetadataIndex, file_id: u64, new_path: PathBuf) -> Result<()> {
        let mut record = self
            .current_record(base, file_id)
            .cloned()
            .ok_or(IncrementalError::MissingBaseRecord(file_id))?;
        let content_changed = self
            .upserts
            .get(&file_id)
            .is_some_and(|value| value.content_changed);
        record.path = new_path;
        self.upsert(base, record, content_changed);
        Ok(())
    }

    pub fn metadata_search(
        &self,
        base: &MetadataIndex,
        request: MetadataSearchRequest<'_>,
    ) -> Vec<IncrementalMetadataHit> {
        let max_results = if request.max_results == 0 {
            METADATA_FIRST_BATCH_DEFAULT
        } else {
            request.max_results
        };
        let mut result = Vec::<IncrementalMetadataHit>::with_capacity(max_results);
        let mut seen = HashSet::<u64>::new();

        let mut overlay_records = self.upserts.values().collect::<Vec<_>>();
        overlay_records.sort_unstable_by_key(|value| value.record.file_id);
        for value in overlay_records {
            if self.tombstones.contains(&value.record.file_id) {
                continue;
            }
            if metadata_record_matches(
                &value.record,
                request.filename,
                request.full_path,
                request.case_sensitive,
            ) {
                seen.insert(value.record.file_id);
                result.push(IncrementalMetadataHit {
                    file_id: value.record.file_id,
                    path: value.record.path.clone(),
                });
                if result.len() >= max_results {
                    return result;
                }
            }
        }

        let mut stage = max_results.max(128).min(base.records().len().max(1));
        loop {
            let base_outcome = base.search(MetadataSearchRequest {
                filename: request.filename,
                full_path: request.full_path,
                case_sensitive: request.case_sensitive,
                max_results: stage,
            });
            for hit in &base_outcome.hits {
                let record = &base.records()[hit.record_index as usize];
                if seen.contains(&record.file_id) || self.base_record_is_stale(record) {
                    continue;
                }
                seen.insert(record.file_id);
                result.push(IncrementalMetadataHit {
                    file_id: record.file_id,
                    path: record.path.clone(),
                });
                if result.len() >= max_results {
                    return result;
                }
            }
            if base_outcome.hits.len() < stage || stage >= base.records().len() {
                break;
            }
            stage = stage.saturating_mul(2).min(base.records().len());
        }
        result
    }

    fn base_record_is_stale(&self, record: &MetadataRecord) -> bool {
        if self.tombstones.contains(&record.file_id) || self.upserts.contains_key(&record.file_id) {
            return true;
        }
        self.path_owners
            .get(&PathKey::from_path(&record.path))
            .is_some_and(|owner| *owner != record.file_id)
    }

    pub fn materialize_records(&self, base: &MetadataIndex) -> Vec<MetadataRecord> {
        let mut records = Vec::with_capacity(base.records().len() + self.upserts.len());
        for record in base.records() {
            if !self.base_record_is_stale(record) {
                records.push(record.clone());
            }
        }
        let mut upserts = self.upserts.values().collect::<Vec<_>>();
        upserts.sort_unstable_by_key(|value| value.record.file_id);
        for value in upserts {
            if !self.tombstones.contains(&value.record.file_id) {
                records.push(value.record.clone());
            }
        }
        records
    }

    pub fn snapshot(&self) -> DeltaSnapshot {
        let mut upserts = self.upserts.values().cloned().collect::<Vec<_>>();
        upserts.sort_unstable_by_key(|value| value.record.file_id);
        let mut tombstones = self.tombstones.iter().copied().collect::<Vec<_>>();
        tombstones.sort_unstable();
        DeltaSnapshot {
            generation: self.generation,
            parent_generation: self.parent_generation,
            upserts,
            tombstones,
        }
    }

    pub fn content_search_first_batch(
        &self,
        root: &Path,
        base_metadata: &MetadataIndex,
        base_content: &PersistentIndex,
        query: ContentQueryKind<'_>,
        case_sensitive: bool,
    ) -> Result<Vec<IncrementalContentHit>> {
        self.content_search_first_batch_internal(
            root,
            base_metadata,
            base_content,
            query,
            case_sensitive,
            None,
        )
    }

    pub fn content_search_first_batch_with_extraction(
        &self,
        root: &Path,
        base_metadata: &MetadataIndex,
        base_content: &PersistentIndex,
        query: ContentQueryKind<'_>,
        case_sensitive: bool,
        extractor: &crate::extraction::ExtractorConfig,
    ) -> Result<Vec<IncrementalContentHit>> {
        self.content_search_first_batch_internal(
            root,
            base_metadata,
            base_content,
            query,
            case_sensitive,
            Some(extractor),
        )
    }

    fn content_search_first_batch_internal(
        &self,
        root: &Path,
        base_metadata: &MetadataIndex,
        base_content: &PersistentIndex,
        query: ContentQueryKind<'_>,
        case_sensitive: bool,
        extractor: Option<&crate::extraction::ExtractorConfig>,
    ) -> Result<Vec<IncrementalContentHit>> {
        let mapping_generation = base_content.generation();
        if self
            .content_mapping_cache
            .borrow()
            .as_ref()
            .is_none_or(|(generation, _)| *generation != mapping_generation)
        {
            let mapping = build_content_file_mapping(base_metadata, base_content);
            *self.content_mapping_cache.borrow_mut() = Some((mapping_generation, mapping));
        }
        let mapping_borrow = self.content_mapping_cache.borrow();
        let mapping = &mapping_borrow.as_ref().expect("content mapping cache").1;
        let mut excluded = HashSet::<u32>::new();
        let mut overrides = HashMap::<u32, PathBuf>::new();
        for stable_id in &self.tombstones {
            if let Some(internal) = mapping.internal_for_stable(*stable_id) {
                excluded.insert(internal);
            }
        }
        for (stable_id, upsert) in &self.upserts {
            if let Some(internal) = mapping.internal_for_stable(*stable_id) {
                if upsert.content_changed {
                    excluded.insert(internal);
                } else {
                    overrides.insert(internal, upsert.record.path.clone());
                }
            }
        }
        let base_overlay = PersistentSearchOverlay {
            excluded_file_ids: &excluded,
            path_overrides: &overrides,
        };
        let base_outcome = match query {
            ContentQueryKind::Literal(value) => base_content.search_first_batch_with_overlay(
                value,
                case_sensitive,
                &base_overlay,
            )?,
            ContentQueryKind::Regex(value) => base_content.search_regex_first_batch_with_overlay(
                value,
                case_sensitive,
                &base_overlay,
            )?,
            ContentQueryKind::Wildcard(value) => base_content
                .search_wildcard_first_batch_with_overlay(value, case_sensitive, &base_overlay)?,
        };
        let mut result = Vec::<IncrementalContentHit>::new();
        let mut seen = HashSet::<(u64, u32, u32)>::new();
        for hit in base_outcome.hits {
            if let Some(stable_id) = mapping
                .internal_to_stable
                .get(hit.file_id as usize)
                .and_then(|value| *value)
            {
                let key = (stable_id, hit.line_number, hit.byte_offset_in_line);
                if seen.insert(key) {
                    result.push(IncrementalContentHit {
                        file_id: stable_id,
                        line_number: hit.line_number,
                        byte_offset_in_line: hit.byte_offset_in_line,
                    });
                }
            }
        }

        if self.changed_content_cache.borrow().is_none() {
            let mut changed = self
                .upserts
                .values()
                .filter(|value| {
                    value.content_changed
                        && value.record.content_searchable
                        && !self.tombstones.contains(&value.record.file_id)
                })
                .collect::<Vec<_>>();
            changed.sort_unstable_by_key(|value| value.record.file_id);
            let mut documents = Vec::<(String, Vec<u8>)>::new();
            let mut stable_ids = Vec::<u64>::new();
            for value in changed {
                let source_path = root.join(&value.record.path);
                let bytes = if value.record.extractable
                    || crate::extraction::is_extractable_document(&value.record.path)
                {
                    let extractor = extractor.ok_or(IncrementalError::InvalidFormat(
                        "document extraction config required",
                    ))?;
                    crate::extraction::extract_document_stream(&source_path, extractor)?
                } else {
                    fs::read(&source_path)?
                };
                documents.push((value.record.path.to_string_lossy().into_owned(), bytes));
                stable_ids.push(value.record.file_id);
            }
            let cache = if documents.is_empty() {
                None
            } else {
                let corpus = Corpus::from_documents(documents);
                let index = PrototypeIndex::build(&corpus);
                Some(DeltaContentCache {
                    corpus,
                    index,
                    stable_ids,
                })
            };
            *self.changed_content_cache.borrow_mut() = cache;
        }
        if result.len() < 100 {
            let cache_borrow = self.changed_content_cache.borrow();
            if let Some(cache) = cache_borrow.as_ref() {
                let outcome = match query {
                    ContentQueryKind::Literal(value) => cache.index.search_first_batch(
                        &cache.corpus,
                        value,
                        case_sensitive,
                        PrototypeVariant::D,
                    ),
                    ContentQueryKind::Regex(value) => cache
                        .index
                        .search_regex_first_batch(
                            &cache.corpus,
                            value,
                            case_sensitive,
                            PrototypeVariant::D,
                        )
                        .map_err(|error| {
                            IncrementalError::InvalidFormat(Box::leak(
                                error.to_string().into_boxed_str(),
                            ))
                        })?,
                    ContentQueryKind::Wildcard(value) => cache
                        .index
                        .search_wildcard_first_batch(
                            &cache.corpus,
                            value,
                            case_sensitive,
                            PrototypeVariant::D,
                        )
                        .map_err(|error| {
                            IncrementalError::InvalidFormat(Box::leak(
                                error.to_string().into_boxed_str(),
                            ))
                        })?,
                };
                for hit in outcome.hits {
                    let stable_id = cache.stable_ids[hit.file_id as usize];
                    let key = (stable_id, hit.line_number, hit.byte_offset_in_line);
                    if seen.insert(key) {
                        result.push(IncrementalContentHit {
                            file_id: stable_id,
                            line_number: hit.line_number,
                            byte_offset_in_line: hit.byte_offset_in_line,
                        });
                        if result.len() >= 100 {
                            break;
                        }
                    }
                }
            }
        }
        Ok(result)
    }

    pub fn reconcile(&mut self, base: &MetadataIndex, observed: Vec<MetadataRecord>) {
        let mut observed_by_id = HashMap::<u64, MetadataRecord>::with_capacity(observed.len());
        for record in observed {
            observed_by_id.insert(record.file_id, record);
        }
        let mut current_ids = HashSet::<u64>::new();
        current_ids.extend(base.records().iter().map(|record| record.file_id));
        current_ids.extend(self.upserts.keys().copied());

        for (file_id, record) in &observed_by_id {
            let previous = self.current_record(base, *file_id).cloned();
            match previous {
                None => self.upsert(base, record.clone(), true),
                Some(previous) => {
                    let content_changed =
                        previous.size != record.size || previous.modified_ns != record.modified_ns;
                    if previous != *record || content_changed {
                        self.upsert(base, record.clone(), content_changed);
                    }
                }
            }
        }
        for file_id in current_ids {
            if !observed_by_id.contains_key(&file_id) {
                self.delete(base, file_id);
            }
        }
    }
}

fn base_owner_for_exact_path(base: &MetadataIndex, path: &Path) -> Option<u64> {
    let query = path.to_str()?;
    let mut stage = 32_usize.min(base.records().len().max(1));
    loop {
        let outcome = base.search(MetadataSearchRequest {
            filename: None,
            full_path: Some(query),
            case_sensitive: true,
            max_results: stage,
        });
        for hit in &outcome.hits {
            let record = &base.records()[hit.record_index as usize];
            if record.path == path {
                return Some(record.file_id);
            }
        }
        if outcome.hits.len() < stage || stage >= base.records().len() {
            return None;
        }
        stage = stage.saturating_mul(2).min(base.records().len());
    }
}

fn build_content_file_mapping(
    base_metadata: &MetadataIndex,
    base_content: &PersistentIndex,
) -> ContentFileMapping {
    let mut path_to_stable = HashMap::<PathKey, u64>::new();
    for record in base_metadata.records() {
        if record.content_searchable {
            path_to_stable.insert(PathKey::from_path(&record.path), record.file_id);
        }
    }
    let mut stable_to_internal = HashMap::new();
    let mut internal_to_stable = vec![None; base_content.file_count()];
    for internal in 0..base_content.file_count() as u32 {
        if let Some(path) = base_content.file_relative_path(internal)
            && let Some(stable) = path_to_stable.get(&PathKey::from_path(path)).copied()
        {
            stable_to_internal.insert(stable, internal);
            internal_to_stable[internal as usize] = Some(stable);
        }
    }
    let mut stable_to_internal = stable_to_internal.into_iter().collect::<Vec<_>>();
    stable_to_internal.sort_unstable_by_key(|value| value.0);
    ContentFileMapping {
        stable_to_internal,
        internal_to_stable,
    }
}

fn metadata_record_matches(
    record: &MetadataRecord,
    filename: Option<&str>,
    full_path: Option<&str>,
    case_sensitive: bool,
) -> bool {
    if let Some(query) = filename.filter(|value| !value.is_empty()) {
        let Some(name) = record.path.file_name().and_then(|value| value.to_str()) else {
            return false;
        };
        if !normalized_contains(name, query, case_sensitive, false) {
            return false;
        }
    }
    if let Some(query) = full_path.filter(|value| !value.is_empty()) {
        let Some(path) = record.path.to_str() else {
            return false;
        };
        if !normalized_contains(path, query, case_sensitive, true) {
            return false;
        }
    }
    filename.is_some() || full_path.is_some()
}

fn normalized_contains(value: &str, query: &str, case_sensitive: bool, path: bool) -> bool {
    let value = if path {
        value.replace('\\', "/")
    } else {
        value.to_owned()
    };
    let query = if path {
        query.replace('\\', "/")
    } else {
        query.to_owned()
    };
    let lhs = normalize_str(&value, case_sensitive);
    let rhs = normalize_str(&query, case_sensitive);
    rhs.bytes().is_empty()
        || lhs
            .bytes()
            .windows(rhs.bytes().len())
            .any(|window| window == rhs.bytes())
}

pub fn write_delta_generation(
    store_dir: impl AsRef<Path>,
    snapshot: &DeltaSnapshot,
) -> Result<PathBuf> {
    let store_dir = store_dir.as_ref();
    fs::create_dir_all(store_dir)?;
    let bytes = serialize_delta(snapshot);
    let path = store_dir.join(delta_file_name(snapshot.generation));
    atomic_write_new(&path, &bytes)?;
    write_advisory_pointer(
        store_dir,
        "DELTA_CURRENT",
        &delta_file_name(snapshot.generation),
    )?;
    Ok(path)
}

pub fn load_delta_generation(
    store_dir: impl AsRef<Path>,
    generation: u64,
) -> Result<DeltaSnapshot> {
    let bytes = fs::read(store_dir.as_ref().join(delta_file_name(generation)))?;
    deserialize_delta(&bytes)
}

pub fn load_latest_delta(store_dir: impl AsRef<Path>) -> Result<DeltaSnapshot> {
    let store = store_dir.as_ref();
    let mut candidates = Vec::<PathBuf>::new();
    if let Ok(current) = fs::read_to_string(store.join("DELTA_CURRENT")) {
        let name = current.trim();
        if name.starts_with("delta-") && name.ends_with(".prdelta") {
            candidates.push(store.join(name));
        }
    }
    let mut paths = list_numbered(store, "delta-", ".prdelta")?;
    paths.sort_unstable_by_key(|(generation, _)| std::cmp::Reverse(*generation));
    for (_, path) in paths {
        if !candidates.contains(&path) {
            candidates.push(path);
        }
    }
    for path in candidates {
        if let Ok(bytes) = fs::read(path)
            && let Ok(snapshot) = deserialize_delta(&bytes)
        {
            return Ok(snapshot);
        }
    }
    Err(IncrementalError::NoValidDelta)
}

fn serialize_delta(snapshot: &DeltaSnapshot) -> Vec<u8> {
    let mut path_blob = Vec::<u8>::new();
    let mut records = Vec::<u8>::with_capacity(snapshot.upserts.len() * DELTA_RECORD_BYTES);
    for value in &snapshot.upserts {
        let record = &value.record;
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
        if value.content_changed {
            flags |= 4;
        }
        records.push(flags);
        records.push(0);
        put_u64(&mut records, record.size);
        put_u64(&mut records, record.modified_ns as u64);
        put_u64(&mut records, (record.modified_ns >> 64) as u64);
        put_u64(&mut records, path_offset);
        put_u32(&mut records, path_bytes.len() as u32);
        put_u32(&mut records, 0);
        put_u64(&mut records, 0);
    }
    let mut tombstones = Vec::<u8>::with_capacity(snapshot.tombstones.len() * 8);
    for value in &snapshot.tombstones {
        put_u64(&mut tombstones, *value);
    }
    let mut out = vec![0_u8; DELTA_HEADER_BYTES];
    out.extend_from_slice(&records);
    out.extend_from_slice(&tombstones);
    out.extend_from_slice(&path_blob);
    out[0..8].copy_from_slice(DELTA_MAGIC);
    write_u32_at(&mut out, 8, DELTA_FORMAT_VERSION);
    write_u32_at(&mut out, 12, DELTA_SEMANTIC_ID);
    write_u64_at(&mut out, 16, snapshot.generation);
    write_u64_at(&mut out, 24, snapshot.parent_generation);
    write_u64_at(&mut out, 32, snapshot.upserts.len() as u64);
    write_u64_at(&mut out, 40, snapshot.tombstones.len() as u64);
    write_u64_at(
        &mut out,
        48,
        (DELTA_HEADER_BYTES + records.len() + tombstones.len()) as u64,
    );
    let checksum = crc64_ecma(&out[DELTA_HEADER_BYTES..]);
    write_u64_at(&mut out, 56, checksum);
    out
}

fn deserialize_delta(bytes: &[u8]) -> Result<DeltaSnapshot> {
    if bytes.len() < DELTA_HEADER_BYTES || &bytes[0..8] != DELTA_MAGIC {
        return Err(IncrementalError::InvalidFormat("delta magic"));
    }
    let version = read_u32(bytes, 8)?;
    if version != DELTA_FORMAT_VERSION {
        return Err(IncrementalError::FormatVersion(version));
    }
    let semantic = read_u32(bytes, 12)?;
    if semantic != DELTA_SEMANTIC_ID {
        return Err(IncrementalError::SemanticVersion(semantic));
    }
    if crc64_ecma(&bytes[DELTA_HEADER_BYTES..]) != read_u64(bytes, 56)? {
        return Err(IncrementalError::ChecksumMismatch);
    }
    let generation = read_u64(bytes, 16)?;
    let parent_generation = read_u64(bytes, 24)?;
    let upsert_count = read_u64(bytes, 32)? as usize;
    let tombstone_count = read_u64(bytes, 40)? as usize;
    let path_offset = read_u64(bytes, 48)? as usize;
    let records_end = DELTA_HEADER_BYTES
        .checked_add(
            upsert_count
                .checked_mul(DELTA_RECORD_BYTES)
                .ok_or(IncrementalError::InvalidFormat("delta records"))?,
        )
        .ok_or(IncrementalError::InvalidFormat("delta records"))?;
    let tombstones_end = records_end
        .checked_add(
            tombstone_count
                .checked_mul(8)
                .ok_or(IncrementalError::InvalidFormat("delta tombstones"))?,
        )
        .ok_or(IncrementalError::InvalidFormat("delta tombstones"))?;
    if tombstones_end != path_offset || path_offset > bytes.len() {
        return Err(IncrementalError::InvalidFormat("delta offsets"));
    }
    let path_blob = &bytes[path_offset..];
    let mut upserts = Vec::with_capacity(upsert_count);
    let mut seen = HashSet::new();
    for index in 0..upsert_count {
        let base = DELTA_HEADER_BYTES + index * DELTA_RECORD_BYTES;
        let file_id = read_u64(bytes, base)?;
        if !seen.insert(file_id) {
            return Err(IncrementalError::InvalidFormat("duplicate delta file id"));
        }
        let source_root = read_u32(bytes, base + 8)?;
        let kind = decode_kind(bytes[base + 12])?;
        let encoding = bytes[base + 13];
        let flags = bytes[base + 14];
        let size = read_u64(bytes, base + 16)?;
        let modified_ns =
            read_u64(bytes, base + 24)? as u128 | ((read_u64(bytes, base + 32)? as u128) << 64);
        let offset = read_u64(bytes, base + 40)? as usize;
        let length = read_u32(bytes, base + 48)? as usize;
        let end = offset
            .checked_add(length)
            .ok_or(IncrementalError::InvalidFormat("delta path"))?;
        let path = decode_exact_path(
            encoding,
            path_blob
                .get(offset..end)
                .ok_or(IncrementalError::InvalidFormat("delta path"))?,
        )?;
        upserts.push(DeltaUpsert {
            record: MetadataRecord {
                file_id,
                path,
                source_root,
                size,
                modified_ns,
                kind,
                content_searchable: flags & 1 != 0,
                extractable: flags & 2 != 0,
            },
            content_changed: flags & 4 != 0,
        });
    }
    let mut tombstones = Vec::with_capacity(tombstone_count);
    for index in 0..tombstone_count {
        tombstones.push(read_u64(bytes, records_end + index * 8)?);
    }
    Ok(DeltaSnapshot {
        generation,
        parent_generation,
        upserts,
        tombstones,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IncrementalState {
    pub generation: u64,
    pub checkpoint: UsnCheckpoint,
    pub pending_renames: Vec<PendingRenameState>,
}

pub fn write_state_generation(
    store_dir: impl AsRef<Path>,
    state: &IncrementalState,
) -> Result<PathBuf> {
    let store = store_dir.as_ref();
    fs::create_dir_all(store)?;
    let bytes = serialize_state(state);
    let path = store.join(state_file_name(state.generation));
    atomic_write_new(&path, &bytes)?;
    write_advisory_pointer(store, "STATE_CURRENT", &state_file_name(state.generation))?;
    Ok(path)
}

pub fn load_state_generation(
    store_dir: impl AsRef<Path>,
    generation: u64,
) -> Result<IncrementalState> {
    let bytes = fs::read(store_dir.as_ref().join(state_file_name(generation)))?;
    deserialize_state(&bytes)
}

fn serialize_state(state: &IncrementalState) -> Vec<u8> {
    let mut names = Vec::<u8>::new();
    let mut pending =
        Vec::<u8>::with_capacity(state.pending_renames.len() * STATE_PENDING_RECORD_BYTES);
    for value in &state.pending_renames {
        let offset = names.len() as u64;
        names.extend_from_slice(value.name.as_bytes());
        put_u64(&mut pending, value.file_id);
        put_u64(&mut pending, value.parent_id);
        put_u64(&mut pending, value.usn as u64);
        put_u64(&mut pending, offset);
        put_u32(&mut pending, value.name.len() as u32);
        put_u32(&mut pending, 0);
    }
    let mut out = vec![0_u8; STATE_HEADER_BYTES];
    out.extend_from_slice(&pending);
    out.extend_from_slice(&names);
    out[0..8].copy_from_slice(INCREMENTAL_STATE_MAGIC);
    write_u32_at(&mut out, 8, INCREMENTAL_STATE_FORMAT_VERSION);
    write_u32_at(&mut out, 12, DELTA_SEMANTIC_ID);
    write_u64_at(&mut out, 16, state.generation);
    write_u64_at(&mut out, 24, state.checkpoint.journal_id);
    write_u64_at(&mut out, 32, state.checkpoint.next_usn as u64);
    write_u64_at(&mut out, 40, state.pending_renames.len() as u64);
    write_u64_at(&mut out, 48, (STATE_HEADER_BYTES + pending.len()) as u64);
    let checksum = crc64_ecma(&out[STATE_HEADER_BYTES..]);
    write_u64_at(&mut out, 56, checksum);
    out
}

fn deserialize_state(bytes: &[u8]) -> Result<IncrementalState> {
    if bytes.len() < STATE_HEADER_BYTES || &bytes[0..8] != INCREMENTAL_STATE_MAGIC {
        return Err(IncrementalError::InvalidFormat("state magic"));
    }
    let version = read_u32(bytes, 8)?;
    if version != INCREMENTAL_STATE_FORMAT_VERSION {
        return Err(IncrementalError::FormatVersion(version));
    }
    let semantic = read_u32(bytes, 12)?;
    if semantic != DELTA_SEMANTIC_ID {
        return Err(IncrementalError::SemanticVersion(semantic));
    }
    if crc64_ecma(&bytes[STATE_HEADER_BYTES..]) != read_u64(bytes, 56)? {
        return Err(IncrementalError::ChecksumMismatch);
    }
    let generation = read_u64(bytes, 16)?;
    let checkpoint = UsnCheckpoint {
        journal_id: read_u64(bytes, 24)?,
        next_usn: read_u64(bytes, 32)? as i64,
    };
    let count = read_u64(bytes, 40)? as usize;
    let names_offset = read_u64(bytes, 48)? as usize;
    let pending_end = STATE_HEADER_BYTES
        .checked_add(
            count
                .checked_mul(STATE_PENDING_RECORD_BYTES)
                .ok_or(IncrementalError::InvalidFormat("state pending"))?,
        )
        .ok_or(IncrementalError::InvalidFormat("state pending"))?;
    if pending_end != names_offset || names_offset > bytes.len() {
        return Err(IncrementalError::InvalidFormat("state offsets"));
    }
    let names = &bytes[names_offset..];
    let mut pending_renames = Vec::with_capacity(count);
    for index in 0..count {
        let base = STATE_HEADER_BYTES + index * STATE_PENDING_RECORD_BYTES;
        let file_id = read_u64(bytes, base)?;
        let parent_id = read_u64(bytes, base + 8)?;
        let usn = read_u64(bytes, base + 16)? as i64;
        let offset = read_u64(bytes, base + 24)? as usize;
        let len = read_u32(bytes, base + 32)? as usize;
        let end = offset
            .checked_add(len)
            .ok_or(IncrementalError::InvalidFormat("state name"))?;
        let name = std::str::from_utf8(
            names
                .get(offset..end)
                .ok_or(IncrementalError::InvalidFormat("state name"))?,
        )
        .map_err(|_| IncrementalError::InvalidFormat("state name utf8"))?
        .to_owned();
        pending_renames.push(PendingRenameState {
            file_id,
            parent_id,
            name,
            usn,
        });
    }
    Ok(IncrementalState {
        generation,
        checkpoint,
        pending_renames,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BundleManifest {
    pub generation: u64,
    pub parent_generation: u64,
    pub content_generation: u64,
    pub metadata_generation: u64,
    pub delta_generation: u64,
    pub state_generation: u64,
}

pub struct LoadedBundle {
    pub manifest: BundleManifest,
    pub metadata: MetadataIndex,
    pub content: PersistentIndex,
    pub delta: DeltaOverlay,
    pub state: IncrementalState,
}

pub fn write_metadata_generation(
    store_dir: impl AsRef<Path>,
    generation: u64,
    metadata: &MetadataIndex,
) -> Result<PathBuf> {
    let path = store_dir.as_ref().join(metadata_file_name(generation));
    metadata.write_snapshot(&path)?;
    Ok(path)
}

pub fn write_bundle(store_dir: impl AsRef<Path>, manifest: BundleManifest) -> Result<PathBuf> {
    let store = store_dir.as_ref();
    fs::create_dir_all(store)?;
    let bytes = serialize_bundle(manifest);
    let path = store.join(bundle_file_name(manifest.generation));
    atomic_write_new(&path, &bytes)?;
    write_advisory_pointer(
        store,
        "BUNDLE_CURRENT",
        &bundle_file_name(manifest.generation),
    )?;
    Ok(path)
}

pub fn load_bundle(root: impl AsRef<Path>, store_dir: impl AsRef<Path>) -> Result<LoadedBundle> {
    let root = root.as_ref();
    let store = store_dir.as_ref();
    let mut candidates = Vec::<PathBuf>::new();
    if let Ok(current) = fs::read_to_string(store.join("BUNDLE_CURRENT")) {
        let name = current.trim();
        if name.starts_with("bundle-") && name.ends_with(".prbnd") {
            candidates.push(store.join(name));
        }
    }
    let mut paths = list_numbered(store, "bundle-", ".prbnd")?;
    paths.sort_unstable_by_key(|(generation, _)| std::cmp::Reverse(*generation));
    for (_, path) in paths {
        if !candidates.contains(&path) {
            candidates.push(path);
        }
    }
    for path in candidates {
        let Ok(bytes) = fs::read(path) else {
            continue;
        };
        let Ok(manifest) = deserialize_bundle(&bytes) else {
            continue;
        };
        let Ok(metadata) = MetadataIndex::load_snapshot(
            store.join(metadata_file_name(manifest.metadata_generation)),
        ) else {
            continue;
        };
        let Ok(content) = load_generation(root, store, manifest.content_generation) else {
            continue;
        };
        let Ok(snapshot) = load_delta_generation(store, manifest.delta_generation) else {
            continue;
        };
        let Ok(state) = load_state_generation(store, manifest.state_generation) else {
            continue;
        };
        let delta = DeltaOverlay::from_snapshot(&metadata, snapshot);
        return Ok(LoadedBundle {
            manifest,
            metadata,
            content,
            delta,
            state,
        });
    }
    Err(IncrementalError::NoValidBundle)
}

pub fn load_bundle_with_verification(
    root: impl AsRef<Path>,
    store_dir: impl AsRef<Path>,
    extractor: &crate::extraction::ExtractorConfig,
) -> Result<LoadedBundle> {
    let root = root.as_ref();
    let store = store_dir.as_ref();
    let mut candidates = Vec::<PathBuf>::new();
    if let Ok(current) = fs::read_to_string(store.join("BUNDLE_CURRENT")) {
        let name = current.trim();
        if parse_numbered_name(name, "bundle-", ".prbnd").is_some() {
            candidates.push(store.join(name));
        }
    }
    let mut paths = list_numbered(store, "bundle-", ".prbnd")?;
    paths.sort_unstable_by_key(|(generation, _)| std::cmp::Reverse(*generation));
    for (_, path) in paths {
        if !candidates.contains(&path) {
            candidates.push(path);
        }
    }
    for path in candidates {
        let Ok(bytes) = fs::read(path) else {
            continue;
        };
        let Ok(manifest) = deserialize_bundle(&bytes) else {
            continue;
        };
        let Ok(metadata) = MetadataIndex::load_snapshot(
            store.join(metadata_file_name(manifest.metadata_generation)),
        ) else {
            continue;
        };
        let Ok(content) = crate::persistent::load_generation_with_verification(
            root,
            store,
            manifest.content_generation,
            extractor,
        ) else {
            continue;
        };
        let Ok(snapshot) = load_delta_generation(store, manifest.delta_generation) else {
            continue;
        };
        let Ok(state) = load_state_generation(store, manifest.state_generation) else {
            continue;
        };
        let delta = DeltaOverlay::from_snapshot(&metadata, snapshot);
        return Ok(LoadedBundle {
            manifest,
            metadata,
            content,
            delta,
            state,
        });
    }
    Err(IncrementalError::NoValidBundle)
}

pub fn compact_and_commit(
    root: impl AsRef<Path>,
    store_dir: impl AsRef<Path>,
    loaded: &LoadedBundle,
) -> Result<BundleManifest> {
    let root = root.as_ref();
    let store = store_dir.as_ref();
    let next = next_generation_number(store)?;
    let materialized = MetadataIndex::build(loaded.delta.materialize_records(&loaded.metadata))?;
    write_metadata_generation(store, next, &materialized)?;
    let content = publish_next_generation(root, store)?;
    let empty = DeltaSnapshot {
        generation: next,
        parent_generation: loaded.manifest.delta_generation,
        upserts: Vec::new(),
        tombstones: Vec::new(),
    };
    write_delta_generation(store, &empty)?;
    let state = IncrementalState {
        generation: next,
        checkpoint: loaded.state.checkpoint,
        pending_renames: loaded.state.pending_renames.clone(),
    };
    write_state_generation(store, &state)?;
    let manifest = BundleManifest {
        generation: next,
        parent_generation: loaded.manifest.generation,
        content_generation: content.generation,
        metadata_generation: next,
        delta_generation: next,
        state_generation: next,
    };
    write_bundle(store, manifest)?;
    Ok(manifest)
}

pub fn compact_and_commit_with_extraction(
    root: impl AsRef<Path>,
    store_dir: impl AsRef<Path>,
    loaded: &LoadedBundle,
    extractor: &crate::extraction::ExtractorConfig,
) -> Result<BundleManifest> {
    let root = root.as_ref();
    let store = store_dir.as_ref();
    let next = next_generation_number(store)?;
    let materialized = MetadataIndex::build(loaded.delta.materialize_records(&loaded.metadata))?;
    write_metadata_generation(store, next, &materialized)?;
    let content =
        crate::persistent::publish_next_generation_with_extraction(root, store, extractor)?;
    let empty = DeltaSnapshot {
        generation: next,
        parent_generation: loaded.manifest.delta_generation,
        upserts: Vec::new(),
        tombstones: Vec::new(),
    };
    write_delta_generation(store, &empty)?;
    let state = IncrementalState {
        generation: next,
        checkpoint: loaded.state.checkpoint,
        pending_renames: loaded.state.pending_renames.clone(),
    };
    write_state_generation(store, &state)?;
    let manifest = BundleManifest {
        generation: next,
        parent_generation: loaded.manifest.generation,
        content_generation: content.generation,
        metadata_generation: next,
        delta_generation: next,
        state_generation: next,
    };
    write_bundle(store, manifest)?;
    Ok(manifest)
}

pub fn gc_bundles(
    root: impl AsRef<Path>,
    store_dir: impl AsRef<Path>,
    keep_valid: usize,
) -> Result<Vec<PathBuf>> {
    let root = root.as_ref();
    let store = store_dir.as_ref();
    let keep_valid = keep_valid.max(2);
    let mut valid = Vec::<(u64, BundleManifest, PathBuf)>::new();
    for (generation, path) in list_numbered(store, "bundle-", ".prbnd")? {
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let Ok(manifest) = deserialize_bundle(&bytes) else {
            continue;
        };
        if MetadataIndex::load_snapshot(
            store.join(metadata_file_name(manifest.metadata_generation)),
        )
        .is_err()
        {
            continue;
        }
        if load_generation(root, store, manifest.content_generation).is_err() {
            continue;
        }
        if load_delta_generation(store, manifest.delta_generation).is_err() {
            continue;
        }
        if load_state_generation(store, manifest.state_generation).is_err() {
            continue;
        }
        valid.push((generation, manifest, path));
    }
    valid.sort_unstable_by_key(|value| std::cmp::Reverse(value.0));
    let current = fs::read_to_string(store.join("BUNDLE_CURRENT"))
        .ok()
        .and_then(|value| parse_numbered_name(value.trim(), "bundle-", ".prbnd"));
    let mut keep_bundle = HashSet::<u64>::new();
    if let Some(current) = current
        && valid.iter().any(|value| value.0 == current)
    {
        keep_bundle.insert(current);
    }
    for (generation, _, _) in &valid {
        if keep_bundle.len() >= keep_valid {
            break;
        }
        keep_bundle.insert(*generation);
    }
    let mut content_keep = HashSet::new();
    let mut metadata_keep = HashSet::new();
    let mut delta_keep = HashSet::new();
    let mut state_keep = HashSet::new();
    for (generation, manifest, _) in &valid {
        if keep_bundle.contains(generation) {
            content_keep.insert(manifest.content_generation);
            metadata_keep.insert(manifest.metadata_generation);
            delta_keep.insert(manifest.delta_generation);
            state_keep.insert(manifest.state_generation);
        }
    }
    let mut removed = Vec::new();
    for (generation, _, path) in valid {
        if !keep_bundle.contains(&generation) {
            fs::remove_file(&path)?;
            removed.push(path);
        }
    }
    for (generation, path) in list_numbered(store, "gen-", ".prv2")? {
        if !content_keep.contains(&generation) {
            fs::remove_file(&path)?;
            removed.push(path);
        }
    }
    for (generation, path) in list_numbered(store, "metadata-", ".prmet")? {
        if !metadata_keep.contains(&generation) {
            fs::remove_file(&path)?;
            removed.push(path);
        }
    }
    for (generation, path) in list_numbered(store, "delta-", ".prdelta")? {
        if !delta_keep.contains(&generation) {
            fs::remove_file(&path)?;
            removed.push(path);
        }
    }
    for (generation, path) in list_numbered(store, "state-", ".princ")? {
        if !state_keep.contains(&generation) {
            fs::remove_file(&path)?;
            removed.push(path);
        }
    }
    sync_directory(store)?;
    Ok(removed)
}

pub fn gc_bundles_with_verification(
    root: impl AsRef<Path>,
    store_dir: impl AsRef<Path>,
    keep_valid: usize,
    extractor: &crate::extraction::ExtractorConfig,
) -> Result<Vec<PathBuf>> {
    let root = root.as_ref();
    let store = store_dir.as_ref();
    let keep_valid = keep_valid.max(2);
    let mut valid = Vec::<(u64, BundleManifest, PathBuf)>::new();
    for (generation, path) in list_numbered(store, "bundle-", ".prbnd")? {
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };
        let Ok(manifest) = deserialize_bundle(&bytes) else {
            continue;
        };
        if MetadataIndex::load_snapshot(
            store.join(metadata_file_name(manifest.metadata_generation)),
        )
        .is_err()
            || crate::persistent::load_generation_with_verification(
                root,
                store,
                manifest.content_generation,
                extractor,
            )
            .is_err()
            || load_delta_generation(store, manifest.delta_generation).is_err()
            || load_state_generation(store, manifest.state_generation).is_err()
        {
            continue;
        }
        valid.push((generation, manifest, path));
    }
    valid.sort_unstable_by_key(|value| std::cmp::Reverse(value.0));
    let current = fs::read_to_string(store.join("BUNDLE_CURRENT"))
        .ok()
        .and_then(|value| parse_numbered_name(value.trim(), "bundle-", ".prbnd"));
    let mut keep_bundle = HashSet::<u64>::new();
    if let Some(current) = current
        && valid.iter().any(|value| value.0 == current)
    {
        keep_bundle.insert(current);
    }
    for (generation, _, _) in &valid {
        if keep_bundle.len() >= keep_valid {
            break;
        }
        keep_bundle.insert(*generation);
    }
    let mut content_keep = HashSet::new();
    let mut metadata_keep = HashSet::new();
    let mut delta_keep = HashSet::new();
    let mut state_keep = HashSet::new();
    for (generation, manifest, _) in &valid {
        if keep_bundle.contains(generation) {
            content_keep.insert(manifest.content_generation);
            metadata_keep.insert(manifest.metadata_generation);
            delta_keep.insert(manifest.delta_generation);
            state_keep.insert(manifest.state_generation);
        }
    }
    let mut removed = Vec::new();
    for (generation, _, path) in valid {
        if !keep_bundle.contains(&generation) {
            fs::remove_file(&path)?;
            removed.push(path);
        }
    }
    for (generation, path) in list_numbered(store, "gen-", ".prv2")? {
        if !content_keep.contains(&generation) {
            fs::remove_file(&path)?;
            removed.push(path);
        }
    }
    for (generation, path) in list_numbered(store, "verify-", ".prv2ver")? {
        if !content_keep.contains(&generation) {
            fs::remove_file(&path)?;
            removed.push(path);
        }
    }
    for (generation, path) in list_numbered(store, "metadata-", ".prmet")? {
        if !metadata_keep.contains(&generation) {
            fs::remove_file(&path)?;
            removed.push(path);
        }
    }
    for (generation, path) in list_numbered(store, "delta-", ".prdelta")? {
        if !delta_keep.contains(&generation) {
            fs::remove_file(&path)?;
            removed.push(path);
        }
    }
    for (generation, path) in list_numbered(store, "state-", ".princ")? {
        if !state_keep.contains(&generation) {
            fs::remove_file(&path)?;
            removed.push(path);
        }
    }
    sync_directory(store)?;
    Ok(removed)
}

fn serialize_bundle(manifest: BundleManifest) -> [u8; BUNDLE_BYTES] {
    let mut bytes = [0_u8; BUNDLE_BYTES];
    bytes[0..8].copy_from_slice(BUNDLE_MAGIC);
    bytes[8..12].copy_from_slice(&BUNDLE_FORMAT_VERSION.to_le_bytes());
    bytes[12..16].copy_from_slice(&DELTA_SEMANTIC_ID.to_le_bytes());
    bytes[16..24].copy_from_slice(&manifest.generation.to_le_bytes());
    bytes[24..32].copy_from_slice(&manifest.parent_generation.to_le_bytes());
    bytes[32..40].copy_from_slice(&manifest.content_generation.to_le_bytes());
    bytes[40..48].copy_from_slice(&manifest.metadata_generation.to_le_bytes());
    bytes[48..56].copy_from_slice(&manifest.delta_generation.to_le_bytes());
    bytes[56..64].copy_from_slice(&manifest.state_generation.to_le_bytes());
    let crc = crc64_ecma(&bytes[..64]);
    bytes[64..72].copy_from_slice(&crc.to_le_bytes());
    bytes
}

fn deserialize_bundle(bytes: &[u8]) -> Result<BundleManifest> {
    if bytes.len() != BUNDLE_BYTES || &bytes[0..8] != BUNDLE_MAGIC {
        return Err(IncrementalError::InvalidFormat("bundle magic"));
    }
    let version = read_u32(bytes, 8)?;
    if version != BUNDLE_FORMAT_VERSION {
        return Err(IncrementalError::FormatVersion(version));
    }
    let semantic = read_u32(bytes, 12)?;
    if semantic != DELTA_SEMANTIC_ID {
        return Err(IncrementalError::SemanticVersion(semantic));
    }
    if crc64_ecma(&bytes[..64]) != read_u64(bytes, 64)? {
        return Err(IncrementalError::ChecksumMismatch);
    }
    Ok(BundleManifest {
        generation: read_u64(bytes, 16)?,
        parent_generation: read_u64(bytes, 24)?,
        content_generation: read_u64(bytes, 32)?,
        metadata_generation: read_u64(bytes, 40)?,
        delta_generation: read_u64(bytes, 48)?,
        state_generation: read_u64(bytes, 56)?,
    })
}

fn delta_file_name(generation: u64) -> String {
    format!("delta-{generation:020}.prdelta")
}
fn metadata_file_name(generation: u64) -> String {
    format!("metadata-{generation:020}.prmet")
}
fn bundle_file_name(generation: u64) -> String {
    format!("bundle-{generation:020}.prbnd")
}
fn state_file_name(generation: u64) -> String {
    format!("state-{generation:020}.princ")
}

fn next_generation_number(store: &Path) -> Result<u64> {
    let mut max = 0_u64;
    for (prefix, suffix) in [
        ("bundle-", ".prbnd"),
        ("metadata-", ".prmet"),
        ("delta-", ".prdelta"),
        ("gen-", ".prv2"),
        ("state-", ".princ"),
    ] {
        for (generation, _) in list_numbered(store, prefix, suffix)? {
            max = max.max(generation);
        }
    }
    Ok(max.saturating_add(1).max(1))
}

fn list_numbered(store: &Path, prefix: &str, suffix: &str) -> Result<Vec<(u64, PathBuf)>> {
    if !store.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(store)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if let Some(generation) = parse_numbered_name(name, prefix, suffix) {
            out.push((generation, path));
        }
    }
    Ok(out)
}
fn parse_numbered_name(name: &str, prefix: &str, suffix: &str) -> Option<u64> {
    name.strip_prefix(prefix)?
        .strip_suffix(suffix)?
        .parse()
        .ok()
}

fn atomic_write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.exists() {
        return Err(IncrementalError::InvalidFormat(
            "immutable generation exists",
        ));
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
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&temp, path)?;
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}
fn write_advisory_pointer(store: &Path, name: &str, value: &str) -> Result<()> {
    let temp = store.join(format!("{name}.tmp"));
    {
        let mut file = File::create(&temp)?;
        writeln!(file, "{value}")?;
        file.sync_all()?;
    }
    let final_path = store.join(name);
    #[cfg(windows)]
    if final_path.exists() {
        let _ = fs::remove_file(&final_path);
    }
    fs::rename(temp, final_path)?;
    sync_directory(store)?;
    Ok(())
}
fn sync_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

fn decode_kind(value: u8) -> Result<MetadataFileKind> {
    match value {
        1 => Ok(MetadataFileKind::File),
        2 => Ok(MetadataFileKind::Directory),
        3 => Ok(MetadataFileKind::Symlink),
        4 => Ok(MetadataFileKind::Other),
        _ => Err(IncrementalError::InvalidFormat("file kind")),
    }
}

#[cfg(unix)]
fn exact_path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}
#[cfg(windows)]
fn exact_path_bytes(path: &Path) -> Vec<u8> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}
#[cfg(not(any(unix, windows)))]
fn exact_path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().as_bytes().to_vec()
}

#[cfg(unix)]
fn encode_exact_path(path: &Path) -> (u8, Vec<u8>) {
    (PATH_ENCODING_UNIX_RAW, exact_path_bytes(path))
}
#[cfg(windows)]
fn encode_exact_path(path: &Path) -> (u8, Vec<u8>) {
    (PATH_ENCODING_WINDOWS_UTF16LE, exact_path_bytes(path))
}
#[cfg(not(any(unix, windows)))]
fn encode_exact_path(path: &Path) -> (u8, Vec<u8>) {
    (PATH_ENCODING_UTF8_FALLBACK, exact_path_bytes(path))
}

fn decode_exact_path(encoding: u8, bytes: &[u8]) -> Result<PathBuf> {
    match encoding {
        PATH_ENCODING_UNIX_RAW => {
            #[cfg(unix)]
            {
                use std::ffi::OsString;
                use std::os::unix::ffi::OsStringExt;
                Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
            }
            #[cfg(not(unix))]
            {
                String::from_utf8(bytes.to_vec())
                    .map(PathBuf::from)
                    .map_err(|_| IncrementalError::InvalidFormat("unix path"))
            }
        }
        PATH_ENCODING_WINDOWS_UTF16LE => {
            if !bytes.len().is_multiple_of(2) {
                return Err(IncrementalError::InvalidFormat("windows path"));
            }
            let units = bytes
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect::<Vec<_>>();
            #[cfg(windows)]
            {
                use std::ffi::OsString;
                use std::os::windows::ffi::OsStringExt;
                Ok(PathBuf::from(OsString::from_wide(&units)))
            }
            #[cfg(not(windows))]
            {
                Ok(PathBuf::from(String::from_utf16(&units).map_err(|_| {
                    IncrementalError::InvalidFormat("windows path")
                })?))
            }
        }
        PATH_ENCODING_UTF8_FALLBACK => String::from_utf8(bytes.to_vec())
            .map(PathBuf::from)
            .map_err(|_| IncrementalError::InvalidFormat("utf8 path")),
        _ => Err(IncrementalError::InvalidFormat("path encoding")),
    }
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
fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or(IncrementalError::InvalidFormat("u32"))?;
    Ok(u32::from_le_bytes(value.try_into().unwrap()))
}
fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let value = bytes
        .get(offset..offset + 8)
        .ok_or(IncrementalError::InvalidFormat("u64"))?;
    Ok(u64::from_le_bytes(value.try_into().unwrap()))
}

pub fn convert_search_hits(hits: Vec<SearchHit>, stable_ids: &[u64]) -> Vec<IncrementalContentHit> {
    hits.into_iter()
        .filter_map(|hit| {
            stable_ids
                .get(hit.file_id as usize)
                .copied()
                .map(|file_id| IncrementalContentHit {
                    file_id,
                    line_number: hit.line_number,
                    byte_offset_in_line: hit.byte_offset_in_line,
                })
        })
        .collect()
}
