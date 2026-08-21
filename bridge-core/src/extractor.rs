use std::{fs, path::Path};

/// Bump this when extraction semantics change so persisted metadata cannot silently reuse stale
/// content produced by an older ingestion policy.
pub const INGESTION_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug)]
pub struct ExtractionBudget {
    pub max_source_bytes: u64,
    pub max_output_bytes: usize,
}

impl ExtractionBudget {
    #[must_use]
    pub fn from_max_file_bytes(max_file_bytes: u64) -> Self {
        let source = max_file_bytes.max(1);
        let output = source
            .saturating_mul(4)
            .clamp(8 * 1024 * 1024, 128 * 1024 * 1024);
        Self {
            max_source_bytes: source,
            max_output_bytes: usize::try_from(output).unwrap_or(128 * 1024 * 1024),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtractedDocument {
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreparedContent {
    /// Let the portable core read the original file body directly.
    SourceFile,
    /// Index filename/path only.
    NameOnly,
    /// Application-prepared text from a structured container.
    Extracted(ExtractedDocument),
}

pub trait ContentExtractor: Send + Sync {
    fn supports(&self, path: &Path) -> bool;
    fn prepare(&self, path: &Path, budget: ExtractionBudget) -> Result<PreparedContent, String>;
}

#[derive(Debug, Default)]
pub struct PlainTextExtractor;

impl ContentExtractor for PlainTextExtractor {
    fn supports(&self, _path: &Path) -> bool {
        true
    }

    fn prepare(&self, _path: &Path, _budget: ExtractionBudget) -> Result<PreparedContent, String> {
        Ok(PreparedContent::SourceFile)
    }
}

#[derive(Debug, Default)]
pub struct OfficeOpenXmlExtractor;

impl ContentExtractor for OfficeOpenXmlExtractor {
    fn supports(&self, path: &Path) -> bool {
        extension_is(path, &["docx", "xlsx", "pptx"])
    }

    fn prepare(&self, path: &Path, budget: ExtractionBudget) -> Result<PreparedContent, String> {
        let metadata = fs::metadata(path).map_err(|error| error.to_string())?;
        if metadata.len() > budget.max_source_bytes {
            return Err(format!(
                "structured file exceeds extraction source budget: {} bytes",
                metadata.len()
            ));
        }
        let bytes = fs::read(path).map_err(|error| error.to_string())?;
        let kind = OfficeKind::from_path(path)
            .ok_or_else(|| format!("unsupported Office container: {}", path.display()))?;
        let text = extract_office_zip(&bytes, kind, budget.max_output_bytes)?;
        Ok(PreparedContent::Extracted(ExtractedDocument { text }))
    }
}

#[derive(Debug, Default)]
pub struct ExtractorRegistry {
    office: OfficeOpenXmlExtractor,
    plain: PlainTextExtractor,
}

impl ExtractorRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn prepare(
        &self,
        path: &Path,
        budget: ExtractionBudget,
    ) -> Result<PreparedContent, String> {
        if self.office.supports(path) {
            return self.office.prepare(path, budget);
        }
        if name_only_extension(path) {
            return Ok(PreparedContent::NameOnly);
        }
        self.plain.prepare(path, budget)
    }

    pub fn read_search_text(
        &self,
        path: &Path,
        budget: ExtractionBudget,
    ) -> Result<Option<String>, String> {
        match self.prepare(path, budget)? {
            PreparedContent::SourceFile => fs::read(path)
                .map(|bytes| Some(String::from_utf8_lossy(&bytes).into_owned()))
                .map_err(|error| error.to_string()),
            PreparedContent::NameOnly => Ok(None),
            PreparedContent::Extracted(document) => Ok(Some(document.text)),
        }
    }
}

fn extension_is(path: &Path, values: &[&str]) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            values
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
}

#[must_use]
pub fn content_search_eligible(path: &Path) -> bool {
    !name_only_extension(path)
}

#[must_use]
pub fn office_open_xml_eligible(path: &Path) -> bool {
    OfficeKind::from_path(path).is_some()
}

fn name_only_extension(path: &Path) -> bool {
    extension_is(
        path,
        &[
            "jpg", "jpeg", "png", "gif", "bmp", "webp", "ico", "tif", "tiff", "heic", "heif",
            "avif", "jxl", "psd", "mp3", "wav", "flac", "aac", "ogg", "m4a", "mp4", "mkv", "avi",
            "mov", "wmv", "webm", "zip", "7z", "rar", "gz", "bz2", "xz", "zst", "exe", "dll",
            "sys", "pdb", "obj", "o", "a", "lib", "so", "dylib", "class", "jar", "war", "pyc",
            "pyo", "pdf", "doc", "xls", "ppt", "sqlite", "sqlite3", "db", "bin", "pack", "woff",
            "woff2", "ttf", "otf",
        ],
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OfficeKind {
    Docx,
    Xlsx,
    Pptx,
}

impl OfficeKind {
    pub(crate) fn from_path(path: &Path) -> Option<Self> {
        let extension = path.extension()?.to_str()?;
        if extension.eq_ignore_ascii_case("docx") {
            Some(Self::Docx)
        } else if extension.eq_ignore_ascii_case("xlsx") {
            Some(Self::Xlsx)
        } else if extension.eq_ignore_ascii_case("pptx") {
            Some(Self::Pptx)
        } else {
            None
        }
    }

    pub(crate) fn include_entry(self, name: &str) -> bool {
        match self {
            Self::Docx => {
                name == "word/document.xml"
                    || name == "word/footnotes.xml"
                    || name == "word/endnotes.xml"
                    || name == "word/comments.xml"
                    || (name.starts_with("word/header") && name.ends_with(".xml"))
                    || (name.starts_with("word/footer") && name.ends_with(".xml"))
            }
            Self::Xlsx => {
                name == "xl/sharedStrings.xml"
                    || (name.starts_with("xl/worksheets/") && name.ends_with(".xml"))
            }
            Self::Pptx => {
                (name.starts_with("ppt/slides/slide") && name.ends_with(".xml"))
                    || (name.starts_with("ppt/notesSlides/notesSlide") && name.ends_with(".xml"))
            }
        }
    }

    fn label(self, name: &str) -> String {
        match self {
            Self::Docx if name == "word/document.xml" => "[document]".to_owned(),
            Self::Docx => format!("[{name}]"),
            Self::Xlsx if name == "xl/sharedStrings.xml" => "[shared strings]".to_owned(),
            Self::Xlsx => format!("[{}]", name.trim_start_matches("xl/worksheets/")),
            Self::Pptx => format!(
                "[{}]",
                name.trim_start_matches("ppt/slides/")
                    .trim_start_matches("ppt/notesSlides/")
                    .trim_end_matches(".xml")
            ),
        }
    }
}

#[derive(Debug)]
struct ZipEntry<'a> {
    name: &'a str,
    method: u16,
    flags: u16,
    compressed_size: usize,
    uncompressed_size: usize,
    local_offset: usize,
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    let end = offset
        .checked_add(2)
        .ok_or_else(|| "ZIP offset overflow".to_owned())?;
    let slice = bytes
        .get(offset..end)
        .ok_or_else(|| "truncated ZIP field".to_owned())?;
    Ok(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| "ZIP offset overflow".to_owned())?;
    let slice = bytes
        .get(offset..end)
        .ok_or_else(|| "truncated ZIP field".to_owned())?;
    Ok(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn find_eocd(bytes: &[u8]) -> Result<usize, String> {
    if bytes.len() < 22 {
        return Err("Office container is not a ZIP file".to_owned());
    }
    let lower = bytes.len().saturating_sub(22 + 65_535);
    for offset in (lower..=bytes.len() - 22).rev() {
        if bytes.get(offset..offset + 4) == Some(&[0x50, 0x4b, 0x05, 0x06]) {
            return Ok(offset);
        }
    }
    Err("ZIP end-of-central-directory not found".to_owned())
}

fn central_entries(bytes: &[u8]) -> Result<Vec<ZipEntry<'_>>, String> {
    let eocd = find_eocd(bytes)?;
    if read_u16(bytes, eocd + 4)? != 0 || read_u16(bytes, eocd + 6)? != 0 {
        return Err("multi-disk ZIP containers are not supported".to_owned());
    }
    let entries = usize::from(read_u16(bytes, eocd + 10)?);
    let central_size = usize::try_from(read_u32(bytes, eocd + 12)?)
        .map_err(|_| "ZIP central directory too large".to_owned())?;
    let central_offset = usize::try_from(read_u32(bytes, eocd + 16)?)
        .map_err(|_| "ZIP central directory offset too large".to_owned())?;
    if central_offset
        .checked_add(central_size)
        .is_none_or(|end| end > bytes.len())
    {
        return Err("ZIP central directory is out of bounds".to_owned());
    }
    let mut out = Vec::with_capacity(entries);
    let mut offset = central_offset;
    for _ in 0..entries {
        if bytes.get(offset..offset + 4) != Some(&[0x50, 0x4b, 0x01, 0x02]) {
            return Err("bad ZIP central directory entry".to_owned());
        }
        let flags = read_u16(bytes, offset + 8)?;
        let method = read_u16(bytes, offset + 10)?;
        let compressed_size_raw = read_u32(bytes, offset + 20)?;
        let uncompressed_size_raw = read_u32(bytes, offset + 24)?;
        let local_offset_raw = read_u32(bytes, offset + 42)?;
        if [compressed_size_raw, uncompressed_size_raw, local_offset_raw].contains(&u32::MAX) {
            return Err("ZIP64 Office containers are not supported".to_owned());
        }
        let name_len = usize::from(read_u16(bytes, offset + 28)?);
        let extra_len = usize::from(read_u16(bytes, offset + 30)?);
        let comment_len = usize::from(read_u16(bytes, offset + 32)?);
        let name_start = offset + 46;
        let name_end = name_start
            .checked_add(name_len)
            .ok_or_else(|| "ZIP name overflow".to_owned())?;
        let name_bytes = bytes
            .get(name_start..name_end)
            .ok_or_else(|| "truncated ZIP entry name".to_owned())?;
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| "Office ZIP entry name is not UTF-8".to_owned())?;
        out.push(ZipEntry {
            name,
            method,
            flags,
            compressed_size: compressed_size_raw as usize,
            uncompressed_size: uncompressed_size_raw as usize,
            local_offset: local_offset_raw as usize,
        });
        offset = name_end
            .checked_add(extra_len)
            .and_then(|value| value.checked_add(comment_len))
            .ok_or_else(|| "ZIP central entry overflow".to_owned())?;
    }
    Ok(out)
}

fn entry_payload<'a>(bytes: &'a [u8], entry: &ZipEntry<'_>) -> Result<&'a [u8], String> {
    if entry.flags & 0x1 != 0 {
        return Err("encrypted Office ZIP entries are not supported".to_owned());
    }
    let offset = entry.local_offset;
    if bytes.get(offset..offset + 4) != Some(&[0x50, 0x4b, 0x03, 0x04]) {
        return Err("bad ZIP local file header".to_owned());
    }
    let name_len = usize::from(read_u16(bytes, offset + 26)?);
    let extra_len = usize::from(read_u16(bytes, offset + 28)?);
    let data_start = offset
        .checked_add(30)
        .and_then(|value| value.checked_add(name_len))
        .and_then(|value| value.checked_add(extra_len))
        .ok_or_else(|| "ZIP local header overflow".to_owned())?;
    let data_end = data_start
        .checked_add(entry.compressed_size)
        .ok_or_else(|| "ZIP payload overflow".to_owned())?;
    bytes
        .get(data_start..data_end)
        .ok_or_else(|| "truncated ZIP payload".to_owned())
}

fn extract_office_zip(
    bytes: &[u8],
    kind: OfficeKind,
    max_output_bytes: usize,
) -> Result<String, String> {
    let entries = central_entries(bytes)?;
    let mut output = String::new();
    let mut extracted_bytes = 0usize;
    for entry in entries
        .iter()
        .filter(|entry| kind.include_entry(entry.name))
    {
        extracted_bytes = extracted_bytes
            .checked_add(entry.uncompressed_size)
            .ok_or_else(|| "Office extraction size overflow".to_owned())?;
        if extracted_bytes > max_output_bytes {
            return Err("Office extracted text exceeds configured budget".to_owned());
        }
        let payload = entry_payload(bytes, entry)?;
        let xml = match entry.method {
            0 => payload.to_vec(),
            8 => inflate_raw(payload, entry.uncompressed_size, max_output_bytes)?,
            method => {
                return Err(format!(
                    "unsupported Office ZIP compression method {method}"
                ));
            }
        };
        if xml.len() > max_output_bytes {
            return Err("Office entry exceeds configured extraction budget".to_owned());
        }
        let text = xml_to_text(&xml)?;
        if text.trim().is_empty() {
            continue;
        }
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str(&kind.label(entry.name));
        output.push('\n');
        output.push_str(text.trim());
        output.push('\n');
        if output.len() > max_output_bytes {
            return Err("Office extracted text exceeds configured budget".to_owned());
        }
    }
    Ok(output)
}

fn xml_to_text(bytes: &[u8]) -> Result<String, String> {
    let xml = std::str::from_utf8(bytes).map_err(|_| "Office XML is not UTF-8".to_owned())?;
    let mut out = String::with_capacity(xml.len().min(64 * 1024));
    let mut cursor = 0usize;
    while cursor < xml.len() {
        let remaining = &xml[cursor..];
        let Some(tag_start_rel) = remaining.find('<') else {
            append_xml_text(&mut out, remaining);
            break;
        };
        let tag_start = cursor + tag_start_rel;
        append_xml_text(&mut out, &xml[cursor..tag_start]);
        let Some(tag_end_rel) = xml[tag_start..].find('>') else {
            return Err("truncated Office XML tag".to_owned());
        };
        let tag_end = tag_start + tag_end_rel;
        let tag = &xml[tag_start + 1..tag_end];
        let tag_trimmed = tag.trim();
        let lower = tag_trimmed.to_ascii_lowercase();
        if lower.starts_with("w:br")
            || lower.starts_with("a:br")
            || lower.starts_with("br")
            || lower.starts_with("/w:p")
            || lower.starts_with("/a:p")
            || lower.starts_with("/row")
            || lower.starts_with("/si")
            || lower.starts_with("/p")
            || lower.starts_with("/tr")
        {
            push_newline(&mut out);
        }
        cursor = tag_end + 1;
    }
    Ok(out)
}

fn push_newline(out: &mut String) {
    while out.ends_with(' ') || out.ends_with('\t') {
        out.pop();
    }
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
}

fn append_xml_text(out: &mut String, text: &str) {
    let mut cursor = 0usize;
    while let Some(amp_rel) = text[cursor..].find('&') {
        let amp = cursor + amp_rel;
        out.push_str(&text[cursor..amp]);
        let Some(end_rel) = text[amp..].find(';') else {
            out.push_str(&text[amp..]);
            return;
        };
        let end = amp + end_rel;
        let entity = &text[amp + 1..end];
        if let Some(decoded) = decode_entity(entity) {
            out.push(decoded);
        } else {
            out.push_str(&text[amp..=end]);
        }
        cursor = end + 1;
    }
    out.push_str(&text[cursor..]);
}

fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ if entity.starts_with("#x") => u32::from_str_radix(&entity[2..], 16)
            .ok()
            .and_then(char::from_u32),
        _ if entity.starts_with('#') => entity[1..].parse::<u32>().ok().and_then(char::from_u32),
        _ => None,
    }
}

struct BitReader<'a> {
    bytes: &'a [u8],
    bit: usize,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, bit: 0 }
    }

    fn read_bit(&mut self) -> Result<u32, String> {
        let byte = self
            .bytes
            .get(self.bit / 8)
            .copied()
            .ok_or_else(|| "truncated DEFLATE stream".to_owned())?;
        let value = u32::from((byte >> (self.bit % 8)) & 1);
        self.bit += 1;
        Ok(value)
    }

    fn read_bits(&mut self, count: usize) -> Result<u32, String> {
        let mut value = 0u32;
        for index in 0..count {
            value |= self.read_bit()? << index;
        }
        Ok(value)
    }

    fn align_byte(&mut self) {
        self.bit = (self.bit + 7) & !7;
    }
}

#[derive(Debug)]
struct Huffman {
    tables: Vec<Vec<i16>>,
    max_bits: usize,
}

impl Huffman {
    fn from_lengths(lengths: &[u8]) -> Result<Self, String> {
        let max_bits = usize::from(*lengths.iter().max().unwrap_or(&0));
        if max_bits > 15 {
            return Err("invalid DEFLATE Huffman code length".to_owned());
        }
        if max_bits == 0 {
            return Ok(Self {
                tables: vec![Vec::new()],
                max_bits: 0,
            });
        }
        let mut counts = vec![0u32; max_bits + 1];
        for &length in lengths {
            if length != 0 {
                counts[usize::from(length)] += 1;
            }
        }
        let mut next_code = vec![0u32; max_bits + 1];
        let mut code = 0u32;
        for bits in 1..=max_bits {
            code = (code + counts[bits - 1]) << 1;
            next_code[bits] = code;
        }
        let mut tables = (0..=max_bits)
            .map(|bits| vec![-1i16; if bits == 0 { 0 } else { 1usize << bits }])
            .collect::<Vec<_>>();
        for (symbol, &length_u8) in lengths.iter().enumerate() {
            let length = usize::from(length_u8);
            if length == 0 {
                continue;
            }
            let canonical = next_code[length];
            next_code[length] += 1;
            let reversed = reverse_low_bits(canonical, length) as usize;
            if tables[length][reversed] != -1 {
                return Err("oversubscribed DEFLATE Huffman tree".to_owned());
            }
            tables[length][reversed] =
                i16::try_from(symbol).map_err(|_| "DEFLATE Huffman symbol overflow".to_owned())?;
        }
        Ok(Self { tables, max_bits })
    }

    fn decode(&self, reader: &mut BitReader<'_>) -> Result<u16, String> {
        if self.max_bits == 0 {
            return Err("empty DEFLATE Huffman tree".to_owned());
        }
        let mut code = 0usize;
        for bits in 1..=self.max_bits {
            code |= (reader.read_bit()? as usize) << (bits - 1);
            let symbol = self.tables[bits][code];
            if symbol >= 0 {
                return Ok(symbol as u16);
            }
        }
        Err("invalid DEFLATE Huffman code".to_owned())
    }
}

fn reverse_low_bits(mut value: u32, bits: usize) -> u32 {
    let mut out = 0u32;
    for _ in 0..bits {
        out = (out << 1) | (value & 1);
        value >>= 1;
    }
    out
}

const LENGTH_BASE: [usize; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [usize; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [usize; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [usize; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

fn fixed_trees() -> Result<(Huffman, Huffman), String> {
    let mut literal = vec![0u8; 288];
    literal[..144].fill(8);
    literal[144..256].fill(9);
    literal[256..280].fill(7);
    literal[280..].fill(8);
    Ok((
        Huffman::from_lengths(&literal)?,
        Huffman::from_lengths(&[5u8; 32])?,
    ))
}

fn dynamic_trees(reader: &mut BitReader<'_>) -> Result<(Huffman, Huffman), String> {
    let hlit = reader.read_bits(5)? as usize + 257;
    let hdist = reader.read_bits(5)? as usize + 1;
    let hclen = reader.read_bits(4)? as usize + 4;
    let order = [
        16usize, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let mut code_lengths = vec![0u8; 19];
    for &symbol in &order[..hclen] {
        code_lengths[symbol] = reader.read_bits(3)? as u8;
    }
    let code_tree = Huffman::from_lengths(&code_lengths)?;
    let total = hlit + hdist;
    let mut lengths = Vec::with_capacity(total);
    while lengths.len() < total {
        let symbol = code_tree.decode(reader)?;
        match symbol {
            0..=15 => lengths.push(symbol as u8),
            16 => {
                let previous = *lengths
                    .last()
                    .ok_or_else(|| "DEFLATE repeat code has no previous length".to_owned())?;
                let repeat = reader.read_bits(2)? as usize + 3;
                if lengths.len().saturating_add(repeat) > total {
                    return Err("DEFLATE code-length repeat exceeds tree size".to_owned());
                }
                lengths.extend(std::iter::repeat_n(previous, repeat));
            }
            17 => {
                let repeat = reader.read_bits(3)? as usize + 3;
                if lengths.len().saturating_add(repeat) > total {
                    return Err("DEFLATE zero repeat exceeds tree size".to_owned());
                }
                lengths.extend(std::iter::repeat_n(0, repeat));
            }
            18 => {
                let repeat = reader.read_bits(7)? as usize + 11;
                if lengths.len().saturating_add(repeat) > total {
                    return Err("DEFLATE long zero repeat exceeds tree size".to_owned());
                }
                lengths.extend(std::iter::repeat_n(0, repeat));
            }
            _ => return Err("invalid DEFLATE code-length symbol".to_owned()),
        }
    }
    Ok((
        Huffman::from_lengths(&lengths[..hlit])?,
        Huffman::from_lengths(&lengths[hlit..])?,
    ))
}

fn inflate_compressed_block(
    reader: &mut BitReader<'_>,
    literal: &Huffman,
    distance: &Huffman,
    output: &mut Vec<u8>,
    max_output: usize,
) -> Result<(), String> {
    loop {
        let symbol = usize::from(literal.decode(reader)?);
        match symbol {
            0..=255 => {
                if output.len() >= max_output {
                    return Err("DEFLATE output exceeds extraction budget".to_owned());
                }
                output.push(symbol as u8);
            }
            256 => return Ok(()),
            257..=285 => {
                let length_index = symbol - 257;
                let extra = LENGTH_EXTRA[length_index];
                let length = LENGTH_BASE[length_index] + reader.read_bits(extra)? as usize;
                let distance_symbol = usize::from(distance.decode(reader)?);
                if distance_symbol >= DIST_BASE.len() {
                    return Err("invalid DEFLATE distance symbol".to_owned());
                }
                let dist_extra = DIST_EXTRA[distance_symbol];
                let dist = DIST_BASE[distance_symbol] + reader.read_bits(dist_extra)? as usize;
                if dist == 0 || dist > output.len() {
                    return Err("invalid DEFLATE back-reference distance".to_owned());
                }
                if output.len().saturating_add(length) > max_output {
                    return Err("DEFLATE output exceeds extraction budget".to_owned());
                }
                for _ in 0..length {
                    let value = output[output.len() - dist];
                    output.push(value);
                }
            }
            _ => return Err("invalid DEFLATE literal/length symbol".to_owned()),
        }
    }
}

fn inflate_raw(input: &[u8], expected: usize, max_output: usize) -> Result<Vec<u8>, String> {
    let mut reader = BitReader::new(input);
    let mut output = Vec::with_capacity(expected.min(max_output));
    loop {
        let final_block = reader.read_bit()? != 0;
        let block_type = reader.read_bits(2)?;
        match block_type {
            0 => {
                reader.align_byte();
                let len = reader.read_bits(16)? as usize;
                let nlen = reader.read_bits(16)? as u16;
                if (len as u16) != !nlen {
                    return Err("invalid DEFLATE stored block length".to_owned());
                }
                if output.len().saturating_add(len) > max_output {
                    return Err("DEFLATE output exceeds extraction budget".to_owned());
                }
                for _ in 0..len {
                    output.push(reader.read_bits(8)? as u8);
                }
            }
            1 => {
                let (literal, distance) = fixed_trees()?;
                inflate_compressed_block(
                    &mut reader,
                    &literal,
                    &distance,
                    &mut output,
                    max_output,
                )?;
            }
            2 => {
                let (literal, distance) = dynamic_trees(&mut reader)?;
                inflate_compressed_block(
                    &mut reader,
                    &literal,
                    &distance,
                    &mut output,
                    max_output,
                )?;
            }
            _ => return Err("reserved DEFLATE block type".to_owned()),
        }
        if final_block {
            break;
        }
    }
    if expected != 0 && output.len() != expected {
        return Err(format!(
            "DEFLATE output size mismatch: expected {expected}, got {}",
            output.len()
        ));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT: AtomicU64 = AtomicU64::new(1);

    fn temp_file(extension: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "personalrag-extractor-{}-{}.{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
            extension
        ))
    }

    fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut central = Vec::new();
        for (name, data) in entries {
            let offset = bytes.len() as u32;
            bytes.extend_from_slice(&0x04034b50u32.to_le_bytes());
            bytes.extend_from_slice(&20u16.to_le_bytes());
            bytes.extend_from_slice(&0u16.to_le_bytes());
            bytes.extend_from_slice(&0u16.to_le_bytes());
            bytes.extend_from_slice(&0u16.to_le_bytes());
            bytes.extend_from_slice(&0u16.to_le_bytes());
            bytes.extend_from_slice(&0u32.to_le_bytes());
            bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&(name.len() as u16).to_le_bytes());
            bytes.extend_from_slice(&0u16.to_le_bytes());
            bytes.extend_from_slice(name.as_bytes());
            bytes.extend_from_slice(data);

            central.extend_from_slice(&0x02014b50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let central_offset = bytes.len() as u32;
        let central_size = central.len() as u32;
        bytes.extend_from_slice(&central);
        bytes.extend_from_slice(&0x06054b50u32.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&central_size.to_le_bytes());
        bytes.extend_from_slice(&central_offset.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes
    }

    #[test]
    fn docx_registry_extracts_text_and_keeps_container_details_outside_core() {
        let path = temp_file("docx");
        let zip = stored_zip(&[(
            "word/document.xml",
            br#"<?xml version="1.0"?><w:document><w:body><w:p><w:r><w:t>Hello &amp; PersonalRag</w:t></w:r></w:p><w:p><w:r><w:t>second line</w:t></w:r></w:p></w:body></w:document>"#,
        )]);
        fs::write(&path, zip).unwrap();
        let registry = ExtractorRegistry::new();
        let prepared = registry
            .prepare(
                &path,
                ExtractionBudget {
                    max_source_bytes: 1024 * 1024,
                    max_output_bytes: 1024 * 1024,
                },
            )
            .unwrap();
        let PreparedContent::Extracted(document) = prepared else {
            panic!("expected extracted Office document");
        };
        assert!(document.text.contains("Hello & PersonalRag"));
        assert!(document.text.contains("second line"));
        assert!(document.text.contains("[document]"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn registry_routes_legacy_binary_to_name_only_and_text_to_source_file() {
        let registry = ExtractorRegistry::new();
        let budget = ExtractionBudget::from_max_file_bytes(1024);
        assert_eq!(
            registry.prepare(Path::new("legacy.xls"), budget).unwrap(),
            PreparedContent::NameOnly
        );
        assert_eq!(
            registry.prepare(Path::new("source.cpp"), budget).unwrap(),
            PreparedContent::SourceFile
        );
    }

    #[test]
    fn xlsx_registry_extracts_shared_strings_and_sheet_text() {
        let path = temp_file("xlsx");
        let zip = stored_zip(&[
            (
                "xl/sharedStrings.xml",
                br#"<?xml version="1.0"?><sst><si><t>Quarterly Revenue</t></si><si><t>Tokyo</t></si></sst>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                br#"<?xml version="1.0"?><worksheet><sheetData><row><c r="A1" t="s"><v>0</v></c><c r="A2" t="s"><v>1</v></c></row></sheetData></worksheet>"#,
            ),
        ]);
        fs::write(&path, zip).unwrap();
        let registry = ExtractorRegistry::new();
        let text = registry
            .read_search_text(
                &path,
                ExtractionBudget {
                    max_source_bytes: 1024 * 1024,
                    max_output_bytes: 1024 * 1024,
                },
            )
            .unwrap()
            .unwrap();
        assert!(text.contains("Quarterly Revenue"));
        assert!(text.contains("Tokyo"));
        assert!(text.contains("[sheet1.xml]"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn pptx_registry_extracts_slide_and_notes_text() {
        let path = temp_file("pptx");
        let zip = stored_zip(&[
            (
                "ppt/slides/slide1.xml",
                br#"<?xml version="1.0"?><p:sld><a:t>Roadmap 2027</a:t><a:t>Milestone Alpha</a:t></p:sld>"#,
            ),
            (
                "ppt/notesSlides/notesSlide1.xml",
                br#"<?xml version="1.0"?><p:notes><a:t>Presenter memo</a:t></p:notes>"#,
            ),
        ]);
        fs::write(&path, zip).unwrap();
        let registry = ExtractorRegistry::new();
        let text = registry
            .read_search_text(
                &path,
                ExtractionBudget {
                    max_source_bytes: 1024 * 1024,
                    max_output_bytes: 1024 * 1024,
                },
            )
            .unwrap()
            .unwrap();
        assert!(text.contains("Roadmap 2027"));
        assert!(text.contains("Milestone Alpha"));
        assert!(text.contains("Presenter memo"));
        assert!(text.contains("[slide1]"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn malformed_office_container_fails_closed() {
        let path = temp_file("docx");
        fs::write(&path, b"not-a-zip").unwrap();
        let registry = ExtractorRegistry::new();
        let error = registry
            .prepare(
                &path,
                ExtractionBudget {
                    max_source_bytes: 1024,
                    max_output_bytes: 1024,
                },
            )
            .unwrap_err();
        assert!(error.contains("ZIP"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn raw_deflate_supports_fixed_huffman_stream() {
        // Python zlib.compressobj(level=6, wbits=-15) over b"PersonalRag Office marker " * 4.
        let compressed: &[u8] = &[
            0x0b, 0x48, 0x2d, 0x2a, 0xce, 0xcf, 0x4b, 0xcc, 0x09, 0x4a, 0x4c, 0x57, 0xf0, 0x4f,
            0x4b, 0xcb, 0x4c, 0x4e, 0x55, 0xc8, 0x4d, 0x2c, 0xca, 0x4e, 0x2d, 0x52, 0x08, 0xa0,
            0xaa, 0x0c, 0x00,
        ];
        let expected = b"PersonalRag Office marker PersonalRag Office marker PersonalRag Office marker PersonalRag Office marker ";
        assert_eq!(
            inflate_raw(compressed, expected.len(), 1024).unwrap(),
            expected
        );
    }
    #[test]
    fn raw_deflate_supports_dynamic_huffman_stream() {
        let compressed: &[u8] = &[
            0xed, 0xca, 0x59, 0x16, 0x40, 0x20, 0x00, 0x40, 0xd1, 0x2d, 0x21, 0x92, 0xe5, 0x64,
            0x08, 0x65, 0x96, 0x21, 0xab, 0xb7, 0x8f, 0xce, 0xbb, 0xdf, 0x57, 0xd7, 0x4d, 0xdb,
            0x99, 0x7e, 0x18, 0xad, 0x9b, 0xe6, 0x65, 0xdd, 0xf6, 0xe3, 0xf4, 0xd7, 0xfd, 0xbc,
            0xe1, 0x4b, 0xd2, 0x4c, 0xe4, 0x85, 0x2c, 0x55, 0xa5, 0x39, 0x1c, 0x0e, 0x87, 0xc3,
            0xe1, 0x70, 0x38, 0x1c, 0x0e, 0x87, 0x13, 0xdd, 0xf9, 0x01,
        ];
        let expected = b"abcdefghijklmnopqrstuvwxyz0123456789".repeat(100);
        assert_eq!(
            inflate_raw(compressed, expected.len(), 8192).unwrap(),
            expected
        );
    }
}
