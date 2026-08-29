use crate::persistent::crc64_ecma;
use crate::{Corpus, FileEntry, append_line_units, is_searchable_path};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::UNIX_EPOCH;

pub const VERIFICATION_MAGIC: &[u8; 8] = b"PRV2VER1";
pub const VERIFICATION_FORMAT_VERSION: u32 = 1;
pub const VERIFICATION_SEMANTIC_ID: u32 = 0x0003_0001;
pub const EXTRACTION_FORMAT_REVISION: u32 = 1;
const VERIFICATION_FOOTER_MAGIC: &[u8; 8] = b"PRV2VE1E";
const HEADER_BYTES: usize = 64;
const RECORD_BYTES: usize = 96;
const FOOTER_BYTES: usize = 24;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum DocumentKind {
    Pdf = 1,
    Docx = 2,
    Xlsx = 3,
    Pptx = 4,
}

impl DocumentKind {
    fn from_u8(value: u8) -> Result<Self> {
        match value {
            1 => Ok(Self::Pdf),
            2 => Ok(Self::Docx),
            3 => Ok(Self::Xlsx),
            4 => Ok(Self::Pptx),
            _ => Err(ExtractionError::InvalidFormat("document kind")),
        }
    }

    fn as_tag(self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Docx => "docx",
            Self::Xlsx => "xlsx",
            Self::Pptx => "pptx",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ExtractorConfig {
    pub pdftotext: PathBuf,
    pub unzip: PathBuf,
    pub zstd: PathBuf,
}

impl Default for ExtractorConfig {
    fn default() -> Self {
        Self::discover()
    }
}

impl ExtractorConfig {
    pub fn discover() -> Self {
        Self {
            pdftotext: discover_helper("PERSONALRAG_PDFTOTEXT", "pdftotext"),
            unzip: discover_helper("PERSONALRAG_UNZIP", "unzip"),
            zstd: discover_helper("PERSONALRAG_ZSTD", "zstd"),
        }
    }

    pub fn override_pdftotext(&mut self, path: impl Into<PathBuf>) {
        self.pdftotext = path.into();
    }

    pub fn override_unzip(&mut self, path: impl Into<PathBuf>) {
        self.unzip = path.into();
    }

    pub fn override_zstd(&mut self, path: impl Into<PathBuf>) {
        self.zstd = path.into();
    }
}

fn discover_helper(env_name: &str, base_name: &str) -> PathBuf {
    if let Some(path) = std::env::var_os(env_name).filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }

    let executable_name = if cfg!(windows) {
        format!("{base_name}.exe")
    } else {
        base_name.to_string()
    };
    if let Ok(current_exe) = std::env::current_exe()
        && let Some(dir) = current_exe.parent()
    {
        for candidate in [
            dir.join("helpers").join(&executable_name),
            dir.join(&executable_name),
        ] {
            if candidate.is_file() {
                return candidate;
            }
        }
    }

    #[cfg(windows)]
    {
        if let Some(path) = discover_windows_helper(&executable_name) {
            return path;
        }
    }

    PathBuf::from(executable_name)
}

#[cfg(windows)]
fn discover_windows_helper(executable_name: &str) -> Option<PathBuf> {
    let mut direct = Vec::<PathBuf>::new();
    if let Some(program_files) = std::env::var_os("ProgramFiles") {
        direct.push(
            PathBuf::from(&program_files)
                .join("Git/usr/bin")
                .join(executable_name),
        );
    }
    if let Some(program_files_x86) = std::env::var_os("ProgramFiles(x86)") {
        direct.push(
            PathBuf::from(&program_files_x86)
                .join("Git/usr/bin")
                .join(executable_name),
        );
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        let local = PathBuf::from(local);
        direct.push(local.join("Programs/Git/usr/bin").join(executable_name));
        direct.push(local.join("Microsoft/WinGet/Links").join(executable_name));
        if let Some(found) =
            find_named_file(&local.join("Microsoft/WinGet/Packages"), executable_name, 6)
        {
            return Some(found);
        }
    }
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        let profile = PathBuf::from(profile);
        direct.push(
            profile
                .join("scoop/apps/poppler/current/Library/bin")
                .join(executable_name),
        );
        direct.push(
            profile
                .join("scoop/apps/zstd/current")
                .join(executable_name),
        );
    }
    direct.into_iter().find(|path| path.is_file())
}

#[cfg(windows)]
fn find_named_file(root: &Path, name: &str, depth: usize) -> Option<PathBuf> {
    if depth == 0 || !root.is_dir() {
        return None;
    }
    let mut entries = fs::read_dir(root)
        .ok()?
        .filter_map(|entry| entry.ok())
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in &entries {
        let path = entry.path();
        if path.is_file()
            && path
                .file_name()
                .is_some_and(|value| value.eq_ignore_ascii_case(name))
        {
            return Some(path);
        }
    }
    for entry in entries {
        let path = entry.path();
        if path.is_dir()
            && let Some(found) = find_named_file(&path, name, depth - 1)
        {
            return Some(found);
        }
    }
    None
}

#[derive(Debug)]
pub enum ExtractionError {
    Io(io::Error),
    Unsupported(PathBuf),
    HelperFailed {
        helper: String,
        status: Option<i32>,
        stderr: String,
    },
    InvalidUtf8(&'static str),
    InvalidXml(&'static str),
    InvalidFormat(&'static str),
    ChecksumMismatch,
    MissingVerification(u32),
    GenerationMismatch {
        expected: u64,
        found: u64,
    },
}

impl fmt::Display for ExtractionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::Unsupported(path) => write!(f, "unsupported document type: {}", path.display()),
            Self::HelperFailed {
                helper,
                status,
                stderr,
            } => write!(
                f,
                "helper {helper} failed with status {}: {stderr}",
                status.map_or_else(|| "signal".to_string(), |value| value.to_string())
            ),
            Self::InvalidUtf8(what) => write!(f, "invalid UTF-8 in {what}"),
            Self::InvalidXml(what) => write!(f, "invalid XML: {what}"),
            Self::InvalidFormat(what) => write!(f, "invalid verification store: {what}"),
            Self::ChecksumMismatch => write!(f, "verification store checksum mismatch"),
            Self::MissingVerification(file_id) => {
                write!(f, "missing extracted verification for file id {file_id}")
            }
            Self::GenerationMismatch { expected, found } => write!(
                f,
                "verification generation mismatch: expected {expected}, found {found}"
            ),
        }
    }
}

impl std::error::Error for ExtractionError {}

impl From<io::Error> for ExtractionError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

pub type Result<T> = std::result::Result<T, ExtractionError>;

#[derive(Clone, Debug)]
pub struct ExtractedDocument {
    pub kind: DocumentKind,
    pub units: Vec<String>,
    pub extractor_fingerprint: u64,
}

impl ExtractedDocument {
    pub fn verification_stream(&self) -> Vec<u8> {
        self.units.join("\n").into_bytes()
    }
}

#[derive(Clone, Debug)]
struct DraftRecord {
    file_id: u32,
    kind: DocumentKind,
    source_size: u64,
    source_crc64: u64,
    source_modified_ns: u128,
    extractor_fingerprint: u64,
    logical_unit_count: u32,
    raw: Vec<u8>,
    compressed: Vec<u8>,
}

#[derive(Clone, Debug)]
pub(crate) struct VerificationDraft {
    generation: u64,
    parent_generation: u64,
    records: Vec<DraftRecord>,
}

impl VerificationDraft {
    pub(crate) fn serialize(&self) -> Result<Vec<u8>> {
        let payload_offset = HEADER_BYTES
            .checked_add(
                self.records
                    .len()
                    .checked_mul(RECORD_BYTES)
                    .ok_or(ExtractionError::InvalidFormat("record bytes overflow"))?,
            )
            .ok_or(ExtractionError::InvalidFormat("payload offset overflow"))?;
        let mut payload_len = 0_usize;
        for record in &self.records {
            payload_len = payload_len
                .checked_add(record.compressed.len())
                .ok_or(ExtractionError::InvalidFormat("payload length overflow"))?;
        }
        let mut out = Vec::with_capacity(
            payload_offset
                .saturating_add(payload_len)
                .saturating_add(FOOTER_BYTES),
        );
        out.extend_from_slice(VERIFICATION_MAGIC);
        put_u32(&mut out, VERIFICATION_FORMAT_VERSION);
        put_u32(&mut out, VERIFICATION_SEMANTIC_ID);
        put_u64(&mut out, self.generation);
        put_u64(&mut out, self.parent_generation);
        put_u32(&mut out, self.records.len() as u32);
        put_u32(&mut out, EXTRACTION_FORMAT_REVISION);
        put_u64(&mut out, HEADER_BYTES as u64);
        put_u64(&mut out, payload_offset as u64);
        put_u64(&mut out, 0);
        debug_assert_eq!(out.len(), HEADER_BYTES);

        let mut absolute_payload = payload_offset as u64;
        for record in &self.records {
            put_u32(&mut out, record.file_id);
            out.push(record.kind as u8);
            out.extend_from_slice(&[0_u8; 3]);
            put_u64(&mut out, record.source_size);
            put_u64(&mut out, record.source_crc64);
            put_u64(&mut out, record.source_modified_ns as u64);
            put_u64(&mut out, (record.source_modified_ns >> 64) as u64);
            put_u64(&mut out, record.extractor_fingerprint);
            put_u32(&mut out, record.logical_unit_count);
            put_u32(&mut out, 0);
            put_u64(&mut out, record.raw.len() as u64);
            put_u64(&mut out, record.compressed.len() as u64);
            put_u64(&mut out, crc64_ecma(&record.raw));
            put_u64(&mut out, crc64_ecma(&record.compressed));
            put_u64(&mut out, absolute_payload);
            absolute_payload = absolute_payload.saturating_add(record.compressed.len() as u64);
        }
        debug_assert_eq!(out.len(), payload_offset);
        for record in &self.records {
            out.extend_from_slice(&record.compressed);
        }
        let body_crc = crc64_ecma(&out);
        out.extend_from_slice(VERIFICATION_FOOTER_MAGIC);
        put_u64(&mut out, body_crc);
        let total_len = (out.len() + 8) as u64;
        put_u64(&mut out, total_len);
        Ok(out)
    }
}

#[derive(Clone, Debug)]
struct VerificationRecord {
    kind: DocumentKind,
    source_size: u64,
    source_crc64: u64,
    source_modified_ns: u128,
    extractor_fingerprint: u64,
    logical_unit_count: u32,
    raw_len: u64,
    compressed_len: u64,
    raw_crc64: u64,
    compressed_crc64: u64,
    payload_offset: u64,
}

#[derive(Debug)]
pub struct VerificationIndex {
    generation: u64,
    parent_generation: u64,
    bytes: Vec<u8>,
    records: HashMap<u32, VerificationRecord>,
    zstd: PathBuf,
    cache: RefCell<HashMap<u32, Vec<u8>>>,
}

impl VerificationIndex {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn parent_generation(&self) -> u64 {
        self.parent_generation
    }

    pub fn record_count(&self) -> usize {
        self.records.len()
    }

    pub fn persistent_bytes(&self) -> u64 {
        self.bytes.len() as u64
    }

    pub fn has_file(&self, file_id: u32) -> bool {
        self.records.contains_key(&file_id)
    }

    pub fn logical_unit_count(&self, file_id: u32) -> Option<u32> {
        self.records
            .get(&file_id)
            .map(|record| record.logical_unit_count)
    }

    pub(crate) fn validate_source_identity(
        &self,
        file_id: u32,
        source_size: u64,
        source_crc64: u64,
        source_modified_ns: u128,
    ) -> Result<()> {
        let Some(record) = self.records.get(&file_id) else {
            return Ok(());
        };
        if record.source_size != source_size
            || record.source_crc64 != source_crc64
            || (record.source_modified_ns != 0
                && source_modified_ns != 0
                && record.source_modified_ns != source_modified_ns)
        {
            return Err(ExtractionError::InvalidFormat(
                "verification/source identity mismatch",
            ));
        }
        Ok(())
    }

    pub(crate) fn read_range(&self, file_id: u32, start: u64, end: u64) -> Result<Vec<u8>> {
        if end < start {
            return Err(ExtractionError::InvalidFormat("verification range"));
        }
        if !self.cache.borrow().contains_key(&file_id) {
            let raw = self.decompress(file_id)?;
            self.cache.borrow_mut().insert(file_id, raw);
        }
        let cache = self.cache.borrow();
        let raw = cache
            .get(&file_id)
            .ok_or(ExtractionError::MissingVerification(file_id))?;
        let start = usize::try_from(start)
            .map_err(|_| ExtractionError::InvalidFormat("verification start overflow"))?;
        let end = usize::try_from(end)
            .map_err(|_| ExtractionError::InvalidFormat("verification end overflow"))?;
        let range = raw.get(start..end).ok_or(ExtractionError::InvalidFormat(
            "verification range out of bounds",
        ))?;
        Ok(range.to_vec())
    }

    fn decompress(&self, file_id: u32) -> Result<Vec<u8>> {
        let record = self
            .records
            .get(&file_id)
            .ok_or(ExtractionError::MissingVerification(file_id))?;
        let start = usize::try_from(record.payload_offset)
            .map_err(|_| ExtractionError::InvalidFormat("payload offset overflow"))?;
        let len = usize::try_from(record.compressed_len)
            .map_err(|_| ExtractionError::InvalidFormat("payload length overflow"))?;
        let end = start
            .checked_add(len)
            .ok_or(ExtractionError::InvalidFormat("payload range overflow"))?;
        let compressed = self
            .bytes
            .get(start..end)
            .ok_or(ExtractionError::InvalidFormat("payload range"))?;
        if crc64_ecma(compressed) != record.compressed_crc64 {
            return Err(ExtractionError::ChecksumMismatch);
        }
        let raw = run_filter(&self.zstd, &["-q", "-d", "-c"], compressed)?;
        if raw.len() as u64 != record.raw_len || crc64_ecma(&raw) != record.raw_crc64 {
            return Err(ExtractionError::ChecksumMismatch);
        }
        Ok(raw)
    }

    pub fn extractor_fingerprint(&self, file_id: u32) -> Option<u64> {
        self.records
            .get(&file_id)
            .map(|record| record.extractor_fingerprint)
    }

    pub fn document_kind(&self, file_id: u32) -> Option<DocumentKind> {
        self.records.get(&file_id).map(|record| record.kind)
    }
}

pub fn document_kind(path: &Path) -> Option<DocumentKind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "pdf" => Some(DocumentKind::Pdf),
        "docx" => Some(DocumentKind::Docx),
        "xlsx" => Some(DocumentKind::Xlsx),
        "pptx" => Some(DocumentKind::Pptx),
        _ => None,
    }
}

pub fn is_extractable_document(path: &Path) -> bool {
    document_kind(path).is_some()
}

pub fn extract_document(path: &Path, config: &ExtractorConfig) -> Result<ExtractedDocument> {
    let kind =
        document_kind(path).ok_or_else(|| ExtractionError::Unsupported(path.to_path_buf()))?;
    let units = match kind {
        DocumentKind::Pdf => extract_pdf(path, config)?,
        DocumentKind::Docx => extract_docx(path, config)?,
        DocumentKind::Xlsx => extract_xlsx(path, config)?,
        DocumentKind::Pptx => extract_pptx(path, config)?,
    };
    let helper_identity = match kind {
        DocumentKind::Pdf => helper_version(&config.pdftotext, &["-v"]),
        DocumentKind::Docx | DocumentKind::Xlsx | DocumentKind::Pptx => {
            helper_version(&config.unzip, &["-v"])
        }
    };
    let fingerprint = format!(
        "personalrag-step5-v{}:{}:{}",
        EXTRACTION_FORMAT_REVISION,
        kind.as_tag(),
        helper_identity.unwrap_or_else(|| "unknown-helper-version".to_string())
    );
    Ok(ExtractedDocument {
        kind,
        units,
        extractor_fingerprint: crc64_ecma(fingerprint.as_bytes()),
    })
}

pub fn extract_document_stream(path: &Path, config: &ExtractorConfig) -> Result<Vec<u8>> {
    Ok(extract_document(path, config)?.verification_stream())
}

pub(crate) fn build_corpus_with_documents(
    root: &Path,
    generation: u64,
    parent_generation: u64,
    config: &ExtractorConfig,
) -> Result<(Corpus, VerificationDraft)> {
    let mut paths = Vec::new();
    collect_ingest_files(root, root, &mut paths)?;
    paths.sort();

    let mut corpus = Corpus::from_documents(Vec::<(String, Vec<u8>)>::new());
    let mut draft = VerificationDraft {
        generation,
        parent_generation,
        records: Vec::new(),
    };
    for path in paths {
        let source = fs::read(&path)?;
        let relative_exact = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
        let display = relative_exact.to_string_lossy().replace('\\', "/");
        let source_modified_ns = fs::metadata(&path)
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);

        let file_id = corpus.files.len() as u32;
        let source_crc64 = crc64_ecma(&source);
        if let Some(_kind) = document_kind(&relative_exact) {
            let extracted = extract_document(&path, config)?;
            let raw = extracted.verification_stream();
            let compressed = run_filter(&config.zstd, &["-q", "-3", "-c"], &raw)?;
            corpus.selected_source_bytes = corpus
                .selected_source_bytes
                .saturating_add(source.len() as u64);
            corpus.files.push(FileEntry {
                path: display,
                exact_path: relative_exact,
                selected_bytes: source.len() as u64,
                source_crc64,
                source_modified_ns,
            });
            if !raw.is_empty() {
                append_line_units(
                    file_id,
                    &raw,
                    &mut corpus.units,
                    &mut corpus.searchable_normalized_bytes,
                );
            }
            draft.records.push(DraftRecord {
                file_id,
                kind: extracted.kind,
                source_size: source.len() as u64,
                source_crc64,
                source_modified_ns,
                extractor_fingerprint: extracted.extractor_fingerprint,
                logical_unit_count: extracted.units.len() as u32,
                raw,
                compressed,
            });
        } else if std::str::from_utf8(&source).is_ok() {
            corpus.selected_source_bytes = corpus
                .selected_source_bytes
                .saturating_add(source.len() as u64);
            corpus.files.push(FileEntry {
                path: display,
                exact_path: relative_exact,
                selected_bytes: source.len() as u64,
                source_crc64,
                source_modified_ns,
            });
            append_line_units(
                file_id,
                &source,
                &mut corpus.units,
                &mut corpus.searchable_normalized_bytes,
            );
        }
    }
    corpus.blocks = crate::pack_blocks(&corpus.units);
    Ok((corpus, draft))
}

pub(crate) fn verification_file_name(generation: u64) -> String {
    format!("verify-{generation:020}.prv2ver")
}

pub(crate) fn deserialize_verification(
    bytes: Vec<u8>,
    expected_generation: u64,
    config: &ExtractorConfig,
) -> Result<VerificationIndex> {
    if bytes.len() < HEADER_BYTES + FOOTER_BYTES || &bytes[..8] != VERIFICATION_MAGIC {
        return Err(ExtractionError::InvalidFormat("magic/length"));
    }
    let version = read_u32(&bytes, 8)?;
    if version != VERIFICATION_FORMAT_VERSION {
        return Err(ExtractionError::InvalidFormat("format version"));
    }
    let semantic = read_u32(&bytes, 12)?;
    if semantic != VERIFICATION_SEMANTIC_ID {
        return Err(ExtractionError::InvalidFormat("semantic id"));
    }
    let generation = read_u64(&bytes, 16)?;
    if generation != expected_generation {
        return Err(ExtractionError::GenerationMismatch {
            expected: expected_generation,
            found: generation,
        });
    }
    let parent_generation = read_u64(&bytes, 24)?;
    let record_count = read_u32(&bytes, 32)? as usize;
    if read_u32(&bytes, 36)? != EXTRACTION_FORMAT_REVISION {
        return Err(ExtractionError::InvalidFormat("extraction revision"));
    }
    let records_offset = read_u64(&bytes, 40)? as usize;
    let payload_offset = read_u64(&bytes, 48)? as usize;
    if records_offset != HEADER_BYTES
        || payload_offset
            != HEADER_BYTES
                .checked_add(
                    record_count
                        .checked_mul(RECORD_BYTES)
                        .ok_or(ExtractionError::InvalidFormat("record bytes overflow"))?,
                )
                .ok_or(ExtractionError::InvalidFormat("payload offset overflow"))?
    {
        return Err(ExtractionError::InvalidFormat("offsets"));
    }
    let footer_start = bytes
        .len()
        .checked_sub(FOOTER_BYTES)
        .ok_or(ExtractionError::InvalidFormat("footer"))?;
    if payload_offset > footer_start
        || &bytes[footer_start..footer_start + 8] != VERIFICATION_FOOTER_MAGIC
    {
        return Err(ExtractionError::InvalidFormat("footer magic"));
    }
    let expected_crc = read_u64(&bytes, footer_start + 8)?;
    if crc64_ecma(&bytes[..footer_start]) != expected_crc {
        return Err(ExtractionError::ChecksumMismatch);
    }
    if read_u64(&bytes, footer_start + 16)? != bytes.len() as u64 {
        return Err(ExtractionError::InvalidFormat("footer length"));
    }

    let mut records = HashMap::with_capacity(record_count);
    let mut payload_ranges = Vec::with_capacity(record_count);
    for index in 0..record_count {
        let base = HEADER_BYTES + index * RECORD_BYTES;
        let file_id = read_u32(&bytes, base)?;
        if records.contains_key(&file_id) {
            return Err(ExtractionError::InvalidFormat("duplicate file id"));
        }
        let kind = DocumentKind::from_u8(
            *bytes
                .get(base + 4)
                .ok_or(ExtractionError::InvalidFormat("record kind"))?,
        )?;
        let source_size = read_u64(&bytes, base + 8)?;
        let source_crc64 = read_u64(&bytes, base + 16)?;
        let source_modified_ns =
            read_u64(&bytes, base + 24)? as u128 | ((read_u64(&bytes, base + 32)? as u128) << 64);
        let extractor_fingerprint = read_u64(&bytes, base + 40)?;
        let logical_unit_count = read_u32(&bytes, base + 48)?;
        let raw_len = read_u64(&bytes, base + 56)?;
        let compressed_len = read_u64(&bytes, base + 64)?;
        let raw_crc64 = read_u64(&bytes, base + 72)?;
        let compressed_crc64 = read_u64(&bytes, base + 80)?;
        let record_payload_offset = read_u64(&bytes, base + 88)?;
        let start = usize::try_from(record_payload_offset)
            .map_err(|_| ExtractionError::InvalidFormat("payload offset overflow"))?;
        let len = usize::try_from(compressed_len)
            .map_err(|_| ExtractionError::InvalidFormat("payload length overflow"))?;
        let end = start
            .checked_add(len)
            .ok_or(ExtractionError::InvalidFormat("payload range overflow"))?;
        if start < payload_offset || end > footer_start {
            return Err(ExtractionError::InvalidFormat("payload range"));
        }
        let compressed = &bytes[start..end];
        if crc64_ecma(compressed) != compressed_crc64 {
            return Err(ExtractionError::ChecksumMismatch);
        }
        payload_ranges.push((start, end));
        records.insert(
            file_id,
            VerificationRecord {
                kind,
                source_size,
                source_crc64,
                source_modified_ns,
                extractor_fingerprint,
                logical_unit_count,
                raw_len,
                compressed_len,
                raw_crc64,
                compressed_crc64,
                payload_offset: record_payload_offset,
            },
        );
    }
    payload_ranges.sort_unstable();
    if payload_ranges.windows(2).any(|pair| pair[0].1 > pair[1].0) {
        return Err(ExtractionError::InvalidFormat("overlapping payloads"));
    }
    Ok(VerificationIndex {
        generation,
        parent_generation,
        bytes,
        records,
        zstd: config.zstd.clone(),
        cache: RefCell::new(HashMap::new()),
    })
}

fn collect_ingest_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if path != root
                && matches!(
                    path.file_name().and_then(|value| value.to_str()),
                    Some(".git" | "target" | "node_modules" | ".venv" | "venv" | "dist" | "build")
                )
            {
                continue;
            }
            collect_ingest_files(root, &path, out)?;
        } else if file_type.is_file()
            && (is_searchable_path(&path) || is_extractable_document(&path))
        {
            out.push(path);
        }
    }
    Ok(())
}

fn extract_pdf(path: &Path, config: &ExtractorConfig) -> Result<Vec<String>> {
    let output = Command::new(&config.pdftotext)
        .arg("-enc")
        .arg("UTF-8")
        .arg("-eol")
        .arg("unix")
        .arg(path)
        .arg("-")
        .output()
        .map_err(ExtractionError::Io)?;
    if !output.status.success() {
        return Err(helper_failure(&config.pdftotext, output));
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|_| ExtractionError::InvalidUtf8("pdftotext output"))?;
    let mut units = Vec::new();
    for page in text.split('\u{000c}') {
        let mut paragraph = String::new();
        for line in page.lines() {
            let line = normalize_unit(line);
            if line.is_empty() {
                if !paragraph.is_empty() {
                    units.push(std::mem::take(&mut paragraph));
                }
            } else {
                if !paragraph.is_empty() {
                    paragraph.push(' ');
                }
                paragraph.push_str(&line);
            }
        }
        if !paragraph.is_empty() {
            units.push(paragraph);
        }
    }
    Ok(units)
}

fn extract_docx(path: &Path, config: &ExtractorConfig) -> Result<Vec<String>> {
    let mut entries = list_zip_entries(path, config)?
        .into_iter()
        .filter(|name| {
            name == "word/document.xml"
                || name.starts_with("word/header") && name.ends_with(".xml")
                || name.starts_with("word/footer") && name.ends_with(".xml")
                || matches!(
                    name.as_str(),
                    "word/footnotes.xml" | "word/endnotes.xml" | "word/comments.xml"
                )
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|name| docx_part_key(name));
    let mut units = Vec::new();
    for entry in entries {
        let xml = read_zip_entry(path, &entry, config)?;
        let xml = decode_xml(&xml)?;
        units.extend(extract_paragraphs(&xml, "p", "t")?);
    }
    Ok(units)
}

fn extract_pptx(path: &Path, config: &ExtractorConfig) -> Result<Vec<String>> {
    let mut entries = list_zip_entries(path, config)?
        .into_iter()
        .filter(|name| {
            (name.starts_with("ppt/slides/slide") && name.ends_with(".xml"))
                || (name.starts_with("ppt/notesSlides/notesSlide") && name.ends_with(".xml"))
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|name| numbered_part_key(name));
    let mut units = Vec::new();
    for entry in entries {
        let xml = read_zip_entry(path, &entry, config)?;
        let xml = decode_xml(&xml)?;
        units.extend(extract_paragraphs(&xml, "p", "t")?);
    }
    Ok(units)
}

fn extract_xlsx(path: &Path, config: &ExtractorConfig) -> Result<Vec<String>> {
    let entries = list_zip_entries(path, config)?;
    let shared = if entries.iter().any(|name| name == "xl/sharedStrings.xml") {
        let xml = read_zip_entry(path, "xl/sharedStrings.xml", config)?;
        let xml = decode_xml(&xml)?;
        extract_container_texts(&xml, "si", "t")?
    } else {
        Vec::new()
    };
    let mut sheets = entries
        .into_iter()
        .filter(|name| name.starts_with("xl/worksheets/sheet") && name.ends_with(".xml"))
        .collect::<Vec<_>>();
    sheets.sort_by_key(|name| numbered_part_key(name));
    let mut units = Vec::new();
    for sheet in sheets {
        let xml = read_zip_entry(path, &sheet, config)?;
        let xml = decode_xml(&xml)?;
        units.extend(extract_cells(&xml, &shared)?);
    }
    Ok(units)
}

fn list_zip_entries(path: &Path, config: &ExtractorConfig) -> Result<Vec<String>> {
    let output = Command::new(&config.unzip)
        .arg("-Z1")
        .arg(path)
        .output()
        .map_err(ExtractionError::Io)?;
    if !output.status.success() {
        return Err(helper_failure(&config.unzip, output));
    }
    let text = String::from_utf8(output.stdout)
        .map_err(|_| ExtractionError::InvalidUtf8("zip entry list"))?;
    Ok(text.lines().map(ToOwned::to_owned).collect())
}

fn read_zip_entry(path: &Path, entry: &str, config: &ExtractorConfig) -> Result<Vec<u8>> {
    let output = Command::new(&config.unzip)
        .arg("-p")
        .arg(path)
        .arg(entry)
        .output()
        .map_err(ExtractionError::Io)?;
    if !output.status.success() {
        return Err(helper_failure(&config.unzip, output));
    }
    Ok(output.stdout)
}

fn run_filter(program: &Path, args: &[&str], input: &[u8]) -> Result<Vec<u8>> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(ExtractionError::Io)?;
    child
        .stdin
        .as_mut()
        .ok_or(ExtractionError::InvalidFormat("helper stdin"))?
        .write_all(input)?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(helper_failure(program, output));
    }
    Ok(output.stdout)
}

fn helper_failure(program: &Path, output: std::process::Output) -> ExtractionError {
    ExtractionError::HelperFailed {
        helper: program.display().to_string(),
        status: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}

fn helper_version(program: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    let bytes = if output.stderr.is_empty() {
        &output.stdout
    } else {
        &output.stderr
    };
    let line = String::from_utf8_lossy(bytes)
        .lines()
        .next()?
        .trim()
        .to_string();
    (!line.is_empty()).then_some(line)
}

fn decode_xml(bytes: &[u8]) -> Result<String> {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8(bytes[3..].to_vec())
            .map_err(|_| ExtractionError::InvalidUtf8("XML"));
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let mut units = Vec::with_capacity((bytes.len().saturating_sub(2)) / 2);
        for pair in bytes[2..].chunks_exact(2) {
            units.push(u16::from_le_bytes([pair[0], pair[1]]));
        }
        return String::from_utf16(&units).map_err(|_| ExtractionError::InvalidXml("UTF-16LE"));
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        let mut units = Vec::with_capacity((bytes.len().saturating_sub(2)) / 2);
        for pair in bytes[2..].chunks_exact(2) {
            units.push(u16::from_be_bytes([pair[0], pair[1]]));
        }
        return String::from_utf16(&units).map_err(|_| ExtractionError::InvalidXml("UTF-16BE"));
    }
    String::from_utf8(bytes.to_vec()).map_err(|_| ExtractionError::InvalidUtf8("XML"))
}

#[derive(Debug)]
enum XmlEvent<'a> {
    Start {
        name: &'a str,
        attrs: &'a str,
        empty: bool,
    },
    End(&'a str),
    Text(&'a str),
}

struct XmlScanner<'a> {
    xml: &'a str,
    pos: usize,
}

impl<'a> XmlScanner<'a> {
    fn new(xml: &'a str) -> Self {
        Self { xml, pos: 0 }
    }

    fn next(&mut self) -> Result<Option<XmlEvent<'a>>> {
        loop {
            if self.pos >= self.xml.len() {
                return Ok(None);
            }
            let rest = &self.xml[self.pos..];
            if !rest.starts_with('<') {
                let end = rest.find('<').unwrap_or(rest.len());
                let text = &rest[..end];
                self.pos += end;
                return Ok(Some(XmlEvent::Text(text)));
            }
            if rest.starts_with("<!--") {
                let end = rest
                    .find("-->")
                    .ok_or(ExtractionError::InvalidXml("unterminated comment"))?;
                self.pos += end + 3;
                continue;
            }
            if rest.starts_with("<?") {
                let end = rest
                    .find("?>")
                    .ok_or(ExtractionError::InvalidXml("unterminated declaration"))?;
                self.pos += end + 2;
                continue;
            }
            if rest.starts_with("<![CDATA[") {
                let start = self.pos + 9;
                let tail = &self.xml[start..];
                let end = tail
                    .find("]]>")
                    .ok_or(ExtractionError::InvalidXml("unterminated CDATA"))?;
                self.pos = start + end + 3;
                return Ok(Some(XmlEvent::Text(&tail[..end])));
            }
            if rest.starts_with("<!") {
                let end = rest
                    .find('>')
                    .ok_or(ExtractionError::InvalidXml("unterminated declaration"))?;
                self.pos += end + 1;
                continue;
            }
            let end = rest
                .find('>')
                .ok_or(ExtractionError::InvalidXml("unterminated tag"))?;
            let inside = rest[1..end].trim();
            self.pos += end + 1;
            if let Some(stripped) = inside.strip_prefix('/') {
                let name = stripped.split_whitespace().next().unwrap_or("");
                return Ok(Some(XmlEvent::End(name)));
            }
            let empty = inside.ends_with('/');
            let body = inside.trim_end_matches('/').trim_end();
            let name_end = body.find(char::is_whitespace).unwrap_or(body.len());
            let name = &body[..name_end];
            let attrs = body[name_end..].trim();
            return Ok(Some(XmlEvent::Start { name, attrs, empty }));
        }
    }
}

fn local_name(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

fn extract_paragraphs(xml: &str, container: &str, text_tag: &str) -> Result<Vec<String>> {
    extract_container_texts(xml, container, text_tag)
}

fn extract_container_texts(xml: &str, container: &str, text_tag: &str) -> Result<Vec<String>> {
    let mut scanner = XmlScanner::new(xml);
    let mut current = None::<String>;
    let mut in_text = 0_usize;
    let mut out = Vec::new();
    while let Some(event) = scanner.next()? {
        match event {
            XmlEvent::Start { name, empty, .. } => {
                let local = local_name(name);
                if local == container {
                    if let Some(value) = current.take() {
                        push_unit(&mut out, value);
                    }
                    current = Some(String::new());
                } else if current.is_some() && local == text_tag {
                    in_text += 1;
                } else if current.is_some()
                    && matches!(local, "tab" | "br" | "cr")
                    && let Some(value) = current.as_mut()
                {
                    value.push(' ');
                }
                if empty && local == text_tag {
                    in_text = in_text.saturating_sub(1);
                }
            }
            XmlEvent::End(name) => {
                let local = local_name(name);
                if local == text_tag {
                    in_text = in_text.saturating_sub(1);
                } else if local == container
                    && let Some(value) = current.take()
                {
                    push_unit(&mut out, value);
                }
            }
            XmlEvent::Text(text) if current.is_some() && in_text > 0 => {
                if let Some(value) = current.as_mut() {
                    value.push_str(&xml_unescape(text)?);
                }
            }
            XmlEvent::Text(_) => {}
        }
    }
    if let Some(value) = current {
        push_unit(&mut out, value);
    }
    Ok(out)
}

#[derive(Default)]
struct CellState {
    cell_type: String,
    value: String,
    formula: String,
    inline: String,
    in_value: usize,
    in_formula: usize,
    in_text: usize,
}

fn extract_cells(xml: &str, shared: &[String]) -> Result<Vec<String>> {
    let mut scanner = XmlScanner::new(xml);
    let mut cell = None::<CellState>;
    let mut out = Vec::new();
    while let Some(event) = scanner.next()? {
        match event {
            XmlEvent::Start { name, attrs, empty } => {
                let local = local_name(name);
                if local == "c" {
                    cell = Some(CellState {
                        cell_type: attr_value(attrs, "t").unwrap_or_default(),
                        ..CellState::default()
                    });
                } else if let Some(state) = cell.as_mut() {
                    match local {
                        "v" => state.in_value += 1,
                        "f" => state.in_formula += 1,
                        "t" => state.in_text += 1,
                        _ => {}
                    }
                    if empty {
                        match local {
                            "v" => state.in_value = state.in_value.saturating_sub(1),
                            "f" => state.in_formula = state.in_formula.saturating_sub(1),
                            "t" => state.in_text = state.in_text.saturating_sub(1),
                            _ => {}
                        }
                    }
                }
            }
            XmlEvent::End(name) => {
                let local = local_name(name);
                if local == "c" {
                    if let Some(state) = cell.take()
                        && let Some(unit) = finalize_cell(state, shared)
                    {
                        out.push(unit);
                    }
                } else if let Some(state) = cell.as_mut() {
                    match local {
                        "v" => state.in_value = state.in_value.saturating_sub(1),
                        "f" => state.in_formula = state.in_formula.saturating_sub(1),
                        "t" => state.in_text = state.in_text.saturating_sub(1),
                        _ => {}
                    }
                }
            }
            XmlEvent::Text(text) => {
                if let Some(state) = cell.as_mut() {
                    let decoded = xml_unescape(text)?;
                    if state.in_value > 0 {
                        state.value.push_str(&decoded);
                    }
                    if state.in_formula > 0 {
                        state.formula.push_str(&decoded);
                    }
                    if state.in_text > 0 {
                        state.inline.push_str(&decoded);
                    }
                }
            }
        }
    }
    Ok(out)
}

fn finalize_cell(state: CellState, shared: &[String]) -> Option<String> {
    let displayed = match state.cell_type.as_str() {
        "s" => state
            .value
            .trim()
            .parse::<usize>()
            .ok()
            .and_then(|index| shared.get(index).cloned())
            .unwrap_or_default(),
        "inlineStr" => state.inline,
        "b" => match state.value.trim() {
            "1" => "TRUE".to_string(),
            "0" => "FALSE".to_string(),
            value => value.to_string(),
        },
        _ => state.value,
    };
    let formula = normalize_unit(&state.formula);
    let displayed = normalize_unit(&displayed);
    let unit = match (formula.is_empty(), displayed.is_empty()) {
        (true, true) => return None,
        (false, true) => formula,
        (true, false) => displayed,
        (false, false) => format!("{formula} {displayed}"),
    };
    Some(unit)
}

fn attr_value(attrs: &str, wanted: &str) -> Option<String> {
    let mut rest = attrs;
    while !rest.is_empty() {
        rest = rest.trim_start();
        if rest.is_empty() {
            break;
        }
        let eq = rest.find('=')?;
        let name = rest[..eq].trim();
        rest = rest[eq + 1..].trim_start();
        let quote = rest.chars().next()?;
        if quote != '"' && quote != '\'' {
            return None;
        }
        let tail = &rest[quote.len_utf8()..];
        let end = tail.find(quote)?;
        let value = &tail[..end];
        if local_name(name) == wanted {
            return xml_unescape(value).ok();
        }
        rest = &tail[end + quote.len_utf8()..];
    }
    None
}

fn xml_unescape(text: &str) -> Result<String> {
    if !text.contains('&') {
        return Ok(text.to_string());
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(index) = rest.find('&') {
        out.push_str(&rest[..index]);
        let tail = &rest[index + 1..];
        let semi = tail
            .find(';')
            .ok_or(ExtractionError::InvalidXml("unterminated entity"))?;
        let entity = &tail[..semi];
        match entity {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                let value = u32::from_str_radix(&entity[2..], 16)
                    .ok()
                    .and_then(char::from_u32)
                    .ok_or(ExtractionError::InvalidXml("hex entity"))?;
                out.push(value);
            }
            _ if entity.starts_with('#') => {
                let value = entity[1..]
                    .parse::<u32>()
                    .ok()
                    .and_then(char::from_u32)
                    .ok_or(ExtractionError::InvalidXml("numeric entity"))?;
                out.push(value);
            }
            _ => return Err(ExtractionError::InvalidXml("unknown entity")),
        }
        rest = &tail[semi + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn normalize_unit(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut pending_space = false;
    for ch in value.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(ch);
        }
    }
    out
}

fn push_unit(out: &mut Vec<String>, value: String) {
    let normalized = normalize_unit(&value);
    if !normalized.is_empty() {
        out.push(normalized);
    }
}

fn docx_part_key(name: &str) -> (u8, u64, String) {
    let rank = if name == "word/document.xml" {
        0
    } else if name.starts_with("word/header") {
        1
    } else if name.starts_with("word/footer") {
        2
    } else if name == "word/footnotes.xml" {
        3
    } else if name == "word/endnotes.xml" {
        4
    } else {
        5
    };
    let number = trailing_number(name).unwrap_or(0);
    (rank, number, name.to_string())
}

fn numbered_part_key(name: &str) -> (String, u64, String) {
    let prefix = name
        .trim_end_matches(".xml")
        .trim_end_matches(|ch: char| ch.is_ascii_digit())
        .to_string();
    (prefix, trailing_number(name).unwrap_or(0), name.to_string())
}

fn trailing_number(name: &str) -> Option<u64> {
    let stem = name.strip_suffix(".xml").unwrap_or(name);
    let digits = stem
        .chars()
        .rev()
        .take_while(|ch| ch.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    (!digits.is_empty()).then(|| digits.parse().ok()).flatten()
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or(ExtractionError::InvalidFormat("u32 range"))?;
    Ok(u32::from_le_bytes(
        slice.try_into().expect("length checked"),
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    let slice = bytes
        .get(offset..offset + 8)
        .ok_or(ExtractionError::InvalidFormat("u64 range"))?;
    Ok(u64::from_le_bytes(
        slice.try_into().expect("length checked"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_entities_and_paragraph_boundaries_are_deterministic() {
        let xml = r#"<w:document><w:p><w:r><w:t>A&amp;B</w:t></w:r><w:r><w:t> Café</w:t></w:r></w:p><w:p><w:r><w:t>Second</w:t></w:r></w:p></w:document>"#;
        assert_eq!(
            extract_paragraphs(xml, "p", "t").unwrap(),
            vec!["A&B Café", "Second"]
        );
    }

    #[test]
    fn xlsx_shared_inline_formula_and_cell_boundaries_are_exact() {
        let xml = r#"<worksheet><sheetData><row><c r="A1" t="s"><v>0</v></c><c r="B1" t="inlineStr"><is><t>Inline</t></is></c><c r="C1"><f>SUM(A1:B1)</f><v>42</v></c></row></sheetData></worksheet>"#;
        assert_eq!(
            extract_cells(xml, &["Shared".to_string()]).unwrap(),
            vec!["Shared", "Inline", "SUM(A1:B1) 42"]
        );
    }

    #[test]
    fn utf16_xml_is_supported() {
        let source = "<w:p><w:t>日本語</w:t></w:p>";
        let mut bytes = vec![0xFF, 0xFE];
        for unit in source.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let decoded = decode_xml(&bytes).unwrap();
        assert_eq!(
            extract_paragraphs(&decoded, "p", "t").unwrap(),
            vec!["日本語"]
        );
    }
}
