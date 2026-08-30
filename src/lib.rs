#![forbid(unsafe_op_in_unsafe_fn)]

pub mod app;
pub mod extraction;
pub mod gui;
pub mod incremental;
mod pattern;
pub mod product;
mod unicode;
mod unicode_tables;
pub mod usn;
pub mod windows_usn;
pub mod windows_watch;

pub use pattern::{PatternError, RegexPattern, WildcardPattern};
pub use unicode::{
    NormalizedText, fold_for_index, is_byte_preserving_fast_path, normalize_str, normalize_utf8,
};
pub use unicode_tables::UNICODE_VERSION;

use std::collections::{BinaryHeap, HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, UNIX_EPOCH};

pub const TARGET_BLOCK_BYTES: usize = 1024 * 1024;
pub const RARE_TRIGRAM_MAX_DF: u8 = 64;
pub const SPARSE_BUDGET_NUMERATOR: u64 = 15;
pub const SPARSE_BUDGET_DENOMINATOR: u64 = 1000;
const Q3_SPARSE_BUDGET_NUMERATOR: u64 = 10;
const HIGHER_SPARSE_BUDGET_NUMERATOR: u64 = 5;
const HIGHER_FILTER_BUDGET_NUMERATOR: u64 = 5;
const HIGHER_FILTER_BUDGET_DENOMINATOR: u64 = 1000;
const HIGHER_FILTER_MAX_BYTES: usize = 4 * 1024 * 1024;
const HIGHER_BLOOM_HASHES: usize = 4;
const HIGHER_BLOOM_HEADER_BYTES: usize = 16;
const HIGHER_SPARSE_HEADER_BYTES: usize = 16;
const HIGHER_SPARSE_RECORD_BYTES: usize = 14;
const HIGHER_SKETCH_ROWS: usize = 4;
const HIGHER_SKETCH_COLUMNS: usize = 1 << 18;
const HIGHER_CANDIDATE_POOL_MAX: usize = 131_072;
const HIGHER_KEY_CACHE_MAX_KEYS: usize = 2_000_000;
const HIGHER_KEY_CACHE_MAX_PER_BLOCK: usize = 100_000;
const TRIGRAM_UNIVERSE: usize = 1 << 24;
const GLOBAL_TRIGRAM_WORDS: usize = TRIGRAM_UNIVERSE / 64;
const PROTOTYPE_HEADER_BYTES: usize = 64;
const SPARSE_HEADER_BYTES: usize = 16;
const SPARSE_RECORD_BYTES: usize = 10;
const ADAPTIVE_HEADER_BYTES: usize = 8;
const DENSE_GLOBAL_BYTES: usize = GLOBAL_TRIGRAM_WORDS * 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrototypeVariant {
    A,
    B,
    C,
    D,
}

impl PrototypeVariant {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::B => "B",
            Self::C => "C",
            Self::D => "D",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GlobalPresenceEncoding {
    None,
    Dense,
    PackedU24,
    SparseWords,
}

impl GlobalPresenceEncoding {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Dense => "dense",
            Self::PackedU24 => "packed-u24",
            Self::SparseWords => "sparse-words",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchLimits {
    pub max_files: usize,
    pub max_matches_seen: usize,
    pub max_snippets_per_file: usize,
}

impl Default for SearchLimits {
    fn default() -> Self {
        Self {
            max_files: 100,
            max_matches_seen: 500,
            max_snippets_per_file: 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    pub file_id: u32,
    pub line_number: u32,
    pub byte_offset_in_line: u32,
}

#[derive(Clone, Debug, Default)]
pub struct SearchMetrics {
    pub candidate_blocks: usize,
    pub candidate_bytes: u64,
    pub verification_bytes: u64,
    pub filter_time: Duration,
    pub verify_time: Duration,
    pub result_assembly_time: Duration,
    pub returned_files: usize,
    pub returned_snippets: usize,
    pub matched_locations_seen: usize,
    pub global_absent_shortcut: bool,
    pub selected_anchor_df: Option<usize>,
    pub selected_anchor_width: Option<u8>,
}

#[derive(Clone, Debug)]
pub struct SearchOutcome {
    pub hits: Vec<SearchHit>,
    pub metrics: SearchMetrics,
}

#[derive(Clone, Debug)]
pub struct CapacityReport {
    pub variant: PrototypeVariant,
    pub selected_source_bytes: u64,
    pub searchable_normalized_bytes: u64,
    pub block_count: usize,
    pub file_catalog_bytes: usize,
    pub block_map_bytes: usize,
    pub unigram_bytes: usize,
    pub bigram_bytes: usize,
    pub global_trigram_presence_bytes: usize,
    pub global_trigram_encoding: GlobalPresenceEncoding,
    pub observed_trigram_count: usize,
    pub sparse_anchor_metadata_bytes: usize,
    pub sparse_anchor_posting_bytes: usize,
    pub higher_ngram_filter_bytes: usize,
    pub higher_sparse_anchor_metadata_bytes: usize,
    pub higher_sparse_anchor_posting_bytes: usize,
    pub total_persistent_bytes: usize,
}

impl CapacityReport {
    pub fn index_source_ratio(&self) -> f64 {
        ratio(
            self.total_persistent_bytes as u64,
            self.selected_source_bytes,
        )
    }

    pub fn index_searchable_ratio(&self) -> f64 {
        ratio(
            self.total_persistent_bytes as u64,
            self.searchable_normalized_bytes,
        )
    }
}

fn ratio(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

#[derive(Clone, Debug)]
struct FileEntry {
    path: String,
    exact_path: PathBuf,
    selected_bytes: u64,
    source_crc64: u64,
    source_modified_ns: u128,
}

#[derive(Clone, Debug)]
struct Unit {
    file_id: u32,
    line_number: u32,
    start: u64,
    end: u64,
    raw: Vec<u8>,
    folded: Vec<u8>,
}

#[derive(Clone, Copy, Debug)]
struct BlockSpan {
    file_id: u32,
    start: u64,
    end: u64,
    start_line: u32,
    line_count: u32,
}

#[derive(Clone, Debug, Default)]
struct SearchBlock {
    unit_ids: Vec<usize>,
    spans: Vec<BlockSpan>,
    searchable_bytes: u64,
}

#[derive(Clone, Debug)]
pub struct Corpus {
    files: Vec<FileEntry>,
    units: Vec<Unit>,
    blocks: Vec<SearchBlock>,
    selected_source_bytes: u64,
    searchable_normalized_bytes: u64,
}

impl Corpus {
    pub fn from_directory(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = root.as_ref();
        let mut paths = Vec::new();
        collect_searchable_files(root, root, &mut paths)?;
        paths.sort();
        let mut corpus = Self::from_documents(Vec::<(String, Vec<u8>)>::new());
        for path in paths {
            let bytes = fs::read(&path)?;
            if std::str::from_utf8(&bytes).is_err() {
                continue;
            }
            let relative_exact = path.strip_prefix(root).unwrap_or(&path).to_path_buf();
            let display = relative_exact.to_string_lossy().replace('\\', "/");
            let file_id = corpus.files.len() as u32;
            corpus.selected_source_bytes = corpus
                .selected_source_bytes
                .saturating_add(bytes.len() as u64);
            let source_modified_ns = fs::metadata(&path)
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos())
                .unwrap_or(0);
            corpus.files.push(FileEntry {
                path: display,
                exact_path: relative_exact,
                selected_bytes: bytes.len() as u64,
                source_crc64: crc64_ecma(&bytes),
                source_modified_ns,
            });
            append_line_units(
                file_id,
                &bytes,
                &mut corpus.units,
                &mut corpus.searchable_normalized_bytes,
            );
        }
        corpus.blocks = pack_blocks(&corpus.units);
        Ok(corpus)
    }

    pub fn from_documents<I, P, B>(documents: I) -> Self
    where
        I: IntoIterator<Item = (P, B)>,
        P: Into<String>,
        B: Into<Vec<u8>>,
    {
        let mut files = Vec::new();
        let mut units = Vec::new();
        let mut selected_source_bytes = 0_u64;
        let mut searchable_normalized_bytes = 0_u64;

        for (file_id, (path, bytes)) in documents.into_iter().enumerate() {
            let path = path.into();
            let bytes = bytes.into();
            selected_source_bytes = selected_source_bytes.saturating_add(bytes.len() as u64);
            files.push(FileEntry {
                exact_path: PathBuf::from(&path),
                path,
                selected_bytes: bytes.len() as u64,
                source_crc64: crc64_ecma(&bytes),
                source_modified_ns: 0,
            });
            append_line_units(
                file_id as u32,
                &bytes,
                &mut units,
                &mut searchable_normalized_bytes,
            );
        }

        let blocks = pack_blocks(&units);
        Self {
            files,
            units,
            blocks,
            selected_source_bytes,
            searchable_normalized_bytes,
        }
    }

    pub const fn selected_source_bytes(&self) -> u64 {
        self.selected_source_bytes
    }

    pub const fn searchable_normalized_bytes(&self) -> u64 {
        self.searchable_normalized_bytes
    }

    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }
}

fn append_line_units(
    file_id: u32,
    bytes: &[u8],
    units: &mut Vec<Unit>,
    searchable_normalized_bytes: &mut u64,
) {
    let mut start = 0_usize;
    let mut line_number = 1_u32;
    for end_with_lf in bytes
        .iter()
        .enumerate()
        .filter_map(|(index, byte)| (*byte == b'\n').then_some(index + 1))
        .chain(std::iter::once(bytes.len()))
    {
        if start > bytes.len() || end_with_lf < start {
            break;
        }
        let mut content_end = if end_with_lf > start && bytes.get(end_with_lf - 1) == Some(&b'\n') {
            end_with_lf - 1
        } else {
            end_with_lf
        };
        if content_end > start && bytes.get(content_end - 1) == Some(&b'\r') {
            content_end -= 1;
        }
        let raw = bytes[start..content_end].to_vec();
        *searchable_normalized_bytes = searchable_normalized_bytes.saturating_add(raw.len() as u64);
        let folded = fold_for_index(&raw);
        units.push(Unit {
            file_id,
            line_number,
            start: start as u64,
            end: content_end as u64,
            raw,
            folded,
        });
        line_number = line_number.saturating_add(1);
        if end_with_lf == bytes.len() {
            break;
        }
        start = end_with_lf;
    }
}

fn pack_blocks(units: &[Unit]) -> Vec<SearchBlock> {
    let mut blocks = Vec::new();
    let mut current = SearchBlock::default();
    for (unit_id, unit) in units.iter().enumerate() {
        let unit_len = unit.folded.len();
        if !current.unit_ids.is_empty()
            && current.searchable_bytes as usize + unit_len > TARGET_BLOCK_BYTES
        {
            blocks.push(current);
            current = SearchBlock::default();
        }
        current.unit_ids.push(unit_id);
        current.searchable_bytes = current.searchable_bytes.saturating_add(unit_len as u64);
        if let Some(last) = current.spans.last_mut()
            && last.file_id == unit.file_id
            && last.end <= unit.start
            && last.start_line.saturating_add(last.line_count) == unit.line_number
        {
            last.end = unit.end;
            last.line_count = last.line_count.saturating_add(1);
        } else {
            current.spans.push(BlockSpan {
                file_id: unit.file_id,
                start: unit.start,
                end: unit.end,
                start_line: unit.line_number,
                line_count: 1,
            });
        }
    }
    if !current.unit_ids.is_empty() {
        blocks.push(current);
    }
    blocks
}

fn collect_searchable_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    let mut entries = fs::read_dir(dir)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if should_skip_dir(root, &path) {
                continue;
            }
            collect_searchable_files(root, &path, out)?;
        } else if file_type.is_file() && is_searchable_path(&path) {
            out.push(path);
        }
    }
    Ok(())
}

fn should_skip_dir(root: &Path, path: &Path) -> bool {
    if path == root {
        return false;
    }
    matches!(
        path.file_name().and_then(|value| value.to_str()),
        Some(".git" | "target" | "node_modules" | ".venv" | "venv" | "dist" | "build")
    )
}

fn is_searchable_path(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|value| value.to_str()) else {
        return matches!(
            path.file_name().and_then(|value| value.to_str()),
            Some("README" | "LICENSE" | "Makefile")
        );
    };
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "rs" | "md"
            | "txt"
            | "log"
            | "json"
            | "toml"
            | "yaml"
            | "yml"
            | "xml"
            | "html"
            | "css"
            | "csv"
            | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "c"
            | "h"
            | "cc"
            | "cpp"
            | "hpp"
            | "py"
            | "ps1"
            | "cmd"
            | "bat"
            | "sh"
    )
}

#[derive(Clone, Debug)]
struct HigherSparseAnchor {
    key: u64,
    postings: Vec<u32>,
}

#[derive(Clone, Debug)]
struct HigherNgramBloom {
    words: Vec<u64>,
    bit_count: usize,
}

impl HigherNgramBloom {
    fn new(selected_source_bytes: u64) -> Self {
        let budget = ((selected_source_bytes as u128 * HIGHER_FILTER_BUDGET_NUMERATOR as u128)
            / HIGHER_FILTER_BUDGET_DENOMINATOR as u128) as usize;
        let budget = budget.min(HIGHER_FILTER_MAX_BYTES);
        let payload_bytes = budget.saturating_sub(HIGHER_BLOOM_HEADER_BYTES) & !7;
        let words = vec![0_u64; payload_bytes / 8];
        let bit_count = words.len() * 64;
        Self { words, bit_count }
    }

    fn is_enabled(&self) -> bool {
        self.bit_count != 0
    }

    fn insert(&mut self, key: u64) {
        if self.bit_count == 0 {
            return;
        }
        for hash_index in 0..HIGHER_BLOOM_HASHES {
            let bit = higher_bloom_bit(key, hash_index, self.bit_count);
            self.words[bit / 64] |= 1_u64 << (bit % 64);
        }
    }

    fn might_contain(&self, key: u64) -> bool {
        if self.bit_count == 0 {
            return true;
        }
        (0..HIGHER_BLOOM_HASHES).all(|hash_index| {
            let bit = higher_bloom_bit(key, hash_index, self.bit_count);
            self.words[bit / 64] & (1_u64 << (bit % 64)) != 0
        })
    }

    fn persistent_bytes(&self) -> usize {
        if self.words.is_empty() {
            0
        } else {
            HIGHER_BLOOM_HEADER_BYTES + self.words.len() * 8
        }
    }

    fn serialize_into(&self, out: &mut Vec<u8>) {
        if self.words.is_empty() {
            return;
        }
        put_u32(out, self.words.len() as u32);
        put_u32(out, HIGHER_BLOOM_HASHES as u32);
        put_u64(out, self.bit_count as u64);
        for word in &self.words {
            out.extend_from_slice(&word.to_le_bytes());
        }
    }
}

#[derive(Clone, Debug)]
struct HigherDfSketch {
    counters: Vec<u8>,
}

impl HigherDfSketch {
    fn new() -> Self {
        Self {
            counters: vec![0_u8; HIGHER_SKETCH_ROWS * HIGHER_SKETCH_COLUMNS],
        }
    }

    fn add_block_key(&mut self, key: u64) {
        for row in 0..HIGHER_SKETCH_ROWS {
            let column = higher_sketch_column(key, row);
            let index = row * HIGHER_SKETCH_COLUMNS + column;
            self.counters[index] = self.counters[index]
                .saturating_add(1)
                .min(RARE_TRIGRAM_MAX_DF + 1);
        }
    }

    fn estimate_df(&self, key: u64) -> u8 {
        (0..HIGHER_SKETCH_ROWS)
            .map(|row| self.counters[row * HIGHER_SKETCH_COLUMNS + higher_sketch_column(key, row)])
            .min()
            .unwrap_or(RARE_TRIGRAM_MAX_DF + 1)
    }
}

fn build_higher_ngram_accelerator(
    corpus: &Corpus,
) -> (
    HigherNgramBloom,
    Vec<HigherSparseAnchor>,
    HashMap<u64, usize>,
) {
    let mut filter = HigherNgramBloom::new(corpus.selected_source_bytes);
    let mut sketch = HigherDfSketch::new();
    let mut key_cache = Vec::<Option<Vec<u64>>>::with_capacity(corpus.blocks.len());
    let mut cached_keys = 0_usize;

    for block in &corpus.blocks {
        let keys = collect_block_higher_keys(corpus, block);
        for key in &keys {
            filter.insert(*key);
            sketch.add_block_key(*key);
        }
        let may_cache = keys.len() <= HIGHER_KEY_CACHE_MAX_PER_BLOCK
            && cached_keys.saturating_add(keys.len()) <= HIGHER_KEY_CACHE_MAX_KEYS;
        if may_cache {
            cached_keys += keys.len();
            key_cache.push(Some(keys));
        } else {
            key_cache.push(None);
        }
    }

    let sparse_budget = ((corpus.selected_source_bytes as u128
        * HIGHER_SPARSE_BUDGET_NUMERATOR as u128)
        / SPARSE_BUDGET_DENOMINATOR as u128) as usize;
    if sparse_budget < HIGHER_SPARSE_HEADER_BYTES + HIGHER_SPARSE_RECORD_BYTES + 1 {
        return (filter, Vec::new(), HashMap::new());
    }

    let estimated_anchor_floor = HIGHER_SPARSE_RECORD_BYTES + 1;
    let budget_anchor_capacity =
        sparse_budget.saturating_sub(HIGHER_SPARSE_HEADER_BYTES) / estimated_anchor_floor;
    let pool_limit = budget_anchor_capacity
        .saturating_mul(4)
        .clamp(1, HIGHER_CANDIDATE_POOL_MAX);

    let mut heap = BinaryHeap::<(u8, u64, u64)>::new();
    let mut selected_keys = HashSet::<u64>::with_capacity(pool_limit);
    for (block_index, block) in corpus.blocks.iter().enumerate() {
        let fallback;
        let keys = if let Some(cached) = key_cache[block_index].as_deref() {
            cached
        } else {
            fallback = collect_block_higher_keys(corpus, block);
            &fallback
        };
        for key in keys.iter().copied() {
            let estimate = sketch.estimate_df(key);
            if estimate == 0 || estimate > RARE_TRIGRAM_MAX_DF || selected_keys.contains(&key) {
                continue;
            }
            let score = (estimate, stable_mix64(key), key);
            if heap.len() < pool_limit {
                heap.push(score);
                selected_keys.insert(key);
                continue;
            }
            if heap.peek().is_some_and(|worst| score < *worst) {
                if let Some(removed) = heap.pop() {
                    selected_keys.remove(&removed.2);
                }
                heap.push(score);
                selected_keys.insert(key);
            }
        }
    }

    let mut pool = heap.into_vec();
    pool.sort_unstable();
    let pool_keys = pool.iter().map(|item| item.2).collect::<Vec<_>>();
    let mut pool_lookup = HashMap::with_capacity(pool_keys.len());
    for (index, key) in pool_keys.iter().copied().enumerate() {
        pool_lookup.insert(key, index);
    }
    let mut postings = vec![Vec::<u32>::new(); pool_keys.len()];
    for (block_index, block) in corpus.blocks.iter().enumerate() {
        let fallback;
        let keys = if let Some(cached) = key_cache[block_index].as_deref() {
            cached
        } else {
            fallback = collect_block_higher_keys(corpus, block);
            &fallback
        };
        for key in keys {
            if let Some(index) = pool_lookup.get(key).copied() {
                postings[index].push(block_index as u32);
            }
        }
    }

    let mut candidates = pool_keys
        .into_iter()
        .zip(postings)
        .filter(|(_, postings)| {
            !postings.is_empty() && postings.len() <= RARE_TRIGRAM_MAX_DF as usize
        })
        .collect::<Vec<_>>();
    candidates.sort_unstable_by_key(|(key, postings)| (postings.len(), stable_mix64(*key), *key));

    let mut selected = Vec::<HigherSparseAnchor>::new();
    let mut used = HIGHER_SPARSE_HEADER_BYTES;
    for (key, postings) in candidates {
        let posting_bytes = delta_varint_len(&postings);
        let bytes = HIGHER_SPARSE_RECORD_BYTES + posting_bytes;
        if used.saturating_add(bytes) > sparse_budget {
            continue;
        }
        used += bytes;
        selected.push(HigherSparseAnchor { key, postings });
    }
    selected.sort_unstable_by_key(|anchor| anchor.key);
    let mut lookup = HashMap::with_capacity(selected.len());
    for (index, anchor) in selected.iter().enumerate() {
        lookup.insert(anchor.key, index);
    }
    (filter, selected, lookup)
}

fn collect_block_higher_keys(corpus: &Corpus, block: &SearchBlock) -> Vec<u64> {
    let mut keys = HashSet::<u64>::new();
    for unit_id in &block.unit_ids {
        let bytes = &corpus.units[*unit_id].folded;
        for gram in bytes.windows(4) {
            keys.insert(higher_key(4, k4(gram)));
        }
        for gram in bytes.windows(5) {
            keys.insert(higher_key(5, k5(gram)));
        }
    }
    let mut keys = keys.into_iter().collect::<Vec<_>>();
    keys.sort_unstable();
    keys
}

fn higher_key(width: u8, value: u64) -> u64 {
    ((width as u64) << 56) | value
}

fn higher_key_width(key: u64) -> u8 {
    (key >> 56) as u8
}

fn stable_mix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn higher_bloom_bit(key: u64, hash_index: usize, bit_count: usize) -> usize {
    const SEEDS: [u64; HIGHER_BLOOM_HASHES] = [
        0x243f_6a88_85a3_08d3,
        0x1319_8a2e_0370_7344,
        0xa409_3822_299f_31d0,
        0x082e_fa98_ec4e_6c89,
    ];
    (stable_mix64(key ^ SEEDS[hash_index]) % bit_count as u64) as usize
}

fn higher_sketch_column(key: u64, row: usize) -> usize {
    const SEEDS: [u64; HIGHER_SKETCH_ROWS] = [
        0x4528_21e6_38d0_1377,
        0xbe54_66cf_34e9_0c6c,
        0xc0ac_29b7_c97c_50dd,
        0x3f84_d5b5_b547_0917,
    ];
    (stable_mix64(key ^ SEEDS[row]) as usize) & (HIGHER_SKETCH_COLUMNS - 1)
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

#[derive(Clone, Debug)]
struct SparseAnchor {
    gram: u32,
    postings: Vec<u32>,
}

#[derive(Clone, Debug)]
enum AdaptiveGlobalPresence {
    Dense(Vec<u64>),
    PackedU24(Vec<u32>),
    SparseWords(Vec<(u32, u64)>),
}

impl AdaptiveGlobalPresence {
    fn from_dense(words: &[u64]) -> Self {
        let observed = collect_observed_trigrams(words);
        let sparse_words = words
            .iter()
            .copied()
            .enumerate()
            .filter_map(|(index, bits)| (bits != 0).then_some((index as u32, bits)))
            .collect::<Vec<_>>();
        let dense_bytes = ADAPTIVE_HEADER_BYTES + DENSE_GLOBAL_BYTES;
        let packed_bytes = ADAPTIVE_HEADER_BYTES + observed.len() * 3;
        let sparse_word_bytes = ADAPTIVE_HEADER_BYTES + sparse_words.len() * 11;
        if dense_bytes <= packed_bytes && dense_bytes <= sparse_word_bytes {
            Self::Dense(words.to_vec())
        } else if packed_bytes <= sparse_word_bytes {
            Self::PackedU24(observed)
        } else {
            Self::SparseWords(sparse_words)
        }
    }

    fn encoding(&self) -> GlobalPresenceEncoding {
        match self {
            Self::Dense(_) => GlobalPresenceEncoding::Dense,
            Self::PackedU24(_) => GlobalPresenceEncoding::PackedU24,
            Self::SparseWords(_) => GlobalPresenceEncoding::SparseWords,
        }
    }

    fn contains(&self, gram: u32) -> bool {
        match self {
            Self::Dense(words) => global_trigram_present(words, gram),
            Self::PackedU24(values) => values.binary_search(&gram).is_ok(),
            Self::SparseWords(words) => {
                let word_index = gram / 64;
                match words.binary_search_by_key(&word_index, |(index, _)| *index) {
                    Ok(index) => words[index].1 & (1_u64 << (gram % 64)) != 0,
                    Err(_) => false,
                }
            }
        }
    }

    fn persistent_bytes(&self) -> usize {
        match self {
            Self::Dense(_) => ADAPTIVE_HEADER_BYTES + DENSE_GLOBAL_BYTES,
            Self::PackedU24(values) => ADAPTIVE_HEADER_BYTES + values.len() * 3,
            Self::SparseWords(words) => ADAPTIVE_HEADER_BYTES + words.len() * 11,
        }
    }

    fn serialize_into(&self, out: &mut Vec<u8>) {
        let (tag, count) = match self {
            Self::Dense(words) => (1_u8, words.len()),
            Self::PackedU24(values) => (2_u8, values.len()),
            Self::SparseWords(words) => (3_u8, words.len()),
        };
        out.push(tag);
        out.extend_from_slice(&[0_u8; 3]);
        put_u32(out, count as u32);
        match self {
            Self::Dense(words) => {
                for word in words {
                    out.extend_from_slice(&word.to_le_bytes());
                }
            }
            Self::PackedU24(values) => {
                for value in values {
                    put_u24(out, *value);
                }
            }
            Self::SparseWords(words) => {
                for (word_index, bits) in words {
                    put_u24(out, *word_index);
                    out.extend_from_slice(&bits.to_le_bytes());
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct PrototypeIndex {
    block_count: usize,
    words_per_bitmap: usize,
    unigram: Vec<u64>,
    bigram: Vec<u64>,
    global_trigram_presence: Vec<u64>,
    adaptive_global_presence: AdaptiveGlobalPresence,
    observed_trigram_count: usize,
    sparse_anchors: Vec<SparseAnchor>,
    sparse_lookup: HashMap<u32, usize>,
    higher_ngram_filter: HigherNgramBloom,
    higher_sparse_anchors: Vec<HigherSparseAnchor>,
    higher_sparse_lookup: HashMap<u64, usize>,
}

impl PrototypeIndex {
    pub fn build(corpus: &Corpus) -> Self {
        let block_count = corpus.blocks.len();
        let words_per_bitmap = block_count.div_ceil(64).max(1);
        let mut unigram = vec![0_u64; 256 * words_per_bitmap];
        let mut bigram = vec![0_u64; 65_536 * words_per_bitmap];
        let mut global_trigram_presence = vec![0_u64; GLOBAL_TRIGRAM_WORDS];
        let mut trigram_df = vec![0_u8; TRIGRAM_UNIVERSE];
        let mut block_trigrams = Vec::with_capacity(block_count);

        for (block_id, block) in corpus.blocks.iter().enumerate() {
            let mut trigrams = Vec::new();
            for unit_id in &block.unit_ids {
                let unit = &corpus.units[*unit_id];
                for byte in unit.folded.iter().copied() {
                    set_row_bit(&mut unigram, words_per_bitmap, byte as usize, block_id);
                }
                for pair in unit.folded.windows(2) {
                    set_row_bit(
                        &mut bigram,
                        words_per_bitmap,
                        k2(pair[0], pair[1]) as usize,
                        block_id,
                    );
                }
                trigrams.extend(
                    unit.folded
                        .windows(3)
                        .map(|gram| k3(gram[0], gram[1], gram[2])),
                );
            }
            trigrams.sort_unstable();
            trigrams.dedup();
            for gram in &trigrams {
                let index = *gram as usize;
                trigram_df[index] = trigram_df[index]
                    .saturating_add(1)
                    .min(RARE_TRIGRAM_MAX_DF + 1);
                global_trigram_presence[index / 64] |= 1_u64 << (index % 64);
            }
            block_trigrams.push(trigrams);
        }

        let adaptive_global_presence = AdaptiveGlobalPresence::from_dense(&global_trigram_presence);
        let observed_trigram_count = global_trigram_presence
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum();

        let sparse_budget = ((corpus.selected_source_bytes as u128
            * Q3_SPARSE_BUDGET_NUMERATOR as u128)
            / SPARSE_BUDGET_DENOMINATOR as u128) as usize;
        let mut buckets = (0..=RARE_TRIGRAM_MAX_DF)
            .map(|_| Vec::<u32>::new())
            .collect::<Vec<_>>();
        for (gram, df) in trigram_df.iter().copied().enumerate() {
            if (1..=RARE_TRIGRAM_MAX_DF).contains(&df) {
                buckets[df as usize].push(gram as u32);
            }
        }

        let block_id_varint_upper = varint_len(block_count.saturating_sub(1) as u32).max(1);
        let mut selected = Vec::new();
        let mut reserved_sparse_bytes = if sparse_budget >= SPARSE_HEADER_BYTES {
            SPARSE_HEADER_BYTES
        } else {
            0
        };
        if reserved_sparse_bytes != 0 {
            'outer: for (df, bucket) in buckets
                .iter()
                .enumerate()
                .take(RARE_TRIGRAM_MAX_DF as usize + 1)
                .skip(1)
            {
                let estimated_per_anchor = SPARSE_RECORD_BYTES + df * block_id_varint_upper;
                for gram in bucket {
                    if reserved_sparse_bytes.saturating_add(estimated_per_anchor) > sparse_budget {
                        break 'outer;
                    }
                    selected.push(*gram);
                    reserved_sparse_bytes += estimated_per_anchor;
                }
            }
        }
        selected.sort_unstable();

        let mut sparse_lookup = HashMap::with_capacity(selected.len());
        for (index, gram) in selected.iter().copied().enumerate() {
            sparse_lookup.insert(gram, index);
        }
        let mut postings = vec![Vec::<u32>::new(); selected.len()];
        for (block_id, grams) in block_trigrams.iter().enumerate() {
            for gram in grams {
                if let Some(anchor_index) = sparse_lookup.get(gram).copied() {
                    postings[anchor_index].push(block_id as u32);
                }
            }
        }
        let sparse_anchors = selected
            .into_iter()
            .zip(postings)
            .map(|(gram, postings)| SparseAnchor { gram, postings })
            .collect::<Vec<_>>();

        let (higher_ngram_filter, higher_sparse_anchors, higher_sparse_lookup) =
            build_higher_ngram_accelerator(corpus);

        Self {
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
        }
    }

    pub fn sparse_anchor_count(&self) -> usize {
        self.sparse_anchors.len()
    }

    pub fn observed_trigram_count(&self) -> usize {
        self.observed_trigram_count
    }

    pub fn higher_sparse_anchor_count(&self) -> usize {
        self.higher_sparse_anchors.len()
    }

    pub fn higher_ngram_filter_bytes(&self) -> usize {
        self.higher_ngram_filter.persistent_bytes()
    }

    pub fn adaptive_global_encoding(&self) -> GlobalPresenceEncoding {
        self.adaptive_global_presence.encoding()
    }

    pub fn search_first_batch(
        &self,
        corpus: &Corpus,
        query: &str,
        case_sensitive: bool,
        variant: PrototypeVariant,
    ) -> SearchOutcome {
        self.search_internal(
            corpus,
            query.as_bytes(),
            case_sensitive,
            variant,
            Some(SearchLimits::default()),
        )
    }

    pub fn search_with_limits(
        &self,
        corpus: &Corpus,
        query: &str,
        case_sensitive: bool,
        variant: PrototypeVariant,
        limits: SearchLimits,
    ) -> SearchOutcome {
        self.search_internal(
            corpus,
            query.as_bytes(),
            case_sensitive,
            variant,
            Some(limits),
        )
    }

    pub fn search_all(
        &self,
        corpus: &Corpus,
        query: &str,
        case_sensitive: bool,
        variant: PrototypeVariant,
    ) -> SearchOutcome {
        self.search_internal(corpus, query.as_bytes(), case_sensitive, variant, None)
    }

    fn search_internal(
        &self,
        corpus: &Corpus,
        query: &[u8],
        case_sensitive: bool,
        variant: PrototypeVariant,
        limits: Option<SearchLimits>,
    ) -> SearchOutcome {
        let filter_started = Instant::now();
        let folded_query = fold_for_index(query);
        let (candidate_words, global_absent_shortcut, selected_anchor_df, selected_anchor_width) =
            self.candidate_bitmap(&folded_query, variant);
        let candidate_blocks = collect_set_bits(&candidate_words, self.block_count);
        let candidate_bytes = candidate_blocks
            .iter()
            .map(|block_id| corpus.blocks[*block_id].searchable_bytes)
            .sum();
        let filter_time = filter_started.elapsed();

        let verify_started = Instant::now();
        let mut assembly_time = Duration::ZERO;
        let mut hits = Vec::new();
        let mut verification_bytes = 0_u64;
        let mut seen_files = HashSet::new();
        let mut snippets_per_file = HashMap::<u32, usize>::new();
        let mut matched_locations_seen = 0_usize;
        let normalized_query = normalize_utf8(query, case_sensitive);
        let exact_query = normalized_query
            .as_ref()
            .map(NormalizedText::bytes)
            .unwrap_or(query);
        let exact_matcher = ExactMatcher::new(exact_query);
        let mut stop = query.is_empty();

        'blocks: for block_id in candidate_blocks.iter().copied() {
            if stop {
                break;
            }
            for unit_id in &corpus.blocks[block_id].unit_ids {
                let unit = &corpus.units[*unit_id];
                verification_bytes = verification_bytes.saturating_add(unit.raw.len() as u64);
                let sensitive_normalized = case_sensitive
                    .then(|| normalize_utf8(&unit.raw, true))
                    .flatten();
                let haystack = if case_sensitive {
                    sensitive_normalized
                        .as_ref()
                        .map(NormalizedText::bytes)
                        .unwrap_or(&unit.raw)
                } else {
                    &unit.folded
                };
                let mut last_original_position = None::<u32>;
                for position in exact_matcher.find_all(haystack) {
                    let original_position = if case_sensitive {
                        sensitive_normalized
                            .as_ref()
                            .map(|normalized| normalized.origin_at(position))
                            .unwrap_or(position as u32)
                    } else if let Some(normalized) = normalize_utf8(&unit.raw, false) {
                        normalized.origin_at(position)
                    } else {
                        position as u32
                    };
                    if last_original_position == Some(original_position) {
                        continue;
                    }
                    last_original_position = Some(original_position);
                    matched_locations_seen = matched_locations_seen.saturating_add(1);
                    let assembly_started = Instant::now();
                    seen_files.insert(unit.file_id);
                    let snippet_count = snippets_per_file.entry(unit.file_id).or_default();
                    let may_store = limits
                        .map(|value| *snippet_count < value.max_snippets_per_file)
                        .unwrap_or(true);
                    if may_store {
                        hits.push(SearchHit {
                            file_id: unit.file_id,
                            line_number: unit.line_number,
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
            }
        }
        let verify_total = verify_started.elapsed();
        let verify_time = verify_total.saturating_sub(assembly_time);

        SearchOutcome {
            metrics: SearchMetrics {
                candidate_blocks: candidate_blocks.len(),
                candidate_bytes,
                verification_bytes,
                filter_time,
                verify_time,
                result_assembly_time: assembly_time,
                returned_files: seen_files.len(),
                returned_snippets: hits.len(),
                matched_locations_seen,
                global_absent_shortcut,
                selected_anchor_df,
                selected_anchor_width,
            },
            hits,
        }
    }

    fn candidate_bitmap(
        &self,
        folded_query: &[u8],
        variant: PrototypeVariant,
    ) -> (Vec<u64>, bool, Option<usize>, Option<u8>) {
        if folded_query.is_empty() || self.block_count == 0 {
            return (vec![0_u64; self.words_per_bitmap], false, None, None);
        }
        let mut candidate = if folded_query.len() == 1 {
            self.row(&self.unigram, folded_query[0] as usize).to_vec()
        } else {
            let first = &folded_query[..2];
            self.row(&self.bigram, k2(first[0], first[1]) as usize)
                .to_vec()
        };

        if folded_query.len() >= 2 {
            for pair in folded_query.windows(2).skip(1) {
                intersect_in_place(
                    &mut candidate,
                    self.row(&self.bigram, k2(pair[0], pair[1]) as usize),
                );
            }
        }

        let mut selected_anchor_df = None::<usize>;
        let mut selected_anchor_width = None::<u8>;

        if variant != PrototypeVariant::A && folded_query.len() >= 3 {
            let mut best_anchor = None::<usize>;
            for gram_bytes in folded_query.windows(3) {
                let gram = k3(gram_bytes[0], gram_bytes[1], gram_bytes[2]);
                let present = match variant {
                    PrototypeVariant::A => true,
                    PrototypeVariant::B => {
                        global_trigram_present(&self.global_trigram_presence, gram)
                    }
                    PrototypeVariant::C | PrototypeVariant::D => {
                        self.adaptive_global_presence.contains(gram)
                    }
                };
                if !present {
                    candidate.fill(0);
                    return (candidate, true, None, None);
                }
                if let Some(anchor_index) = self.sparse_lookup.get(&gram).copied() {
                    let df = self.sparse_anchors[anchor_index].postings.len();
                    if best_anchor
                        .map(|current| df < self.sparse_anchors[current].postings.len())
                        .unwrap_or(true)
                    {
                        best_anchor = Some(anchor_index);
                    }
                }
            }
            if let Some(anchor_index) = best_anchor {
                let anchor = &self.sparse_anchors[anchor_index];
                let mut anchor_bits = vec![0_u64; self.words_per_bitmap];
                for block_id in &anchor.postings {
                    set_bit(&mut anchor_bits, *block_id as usize);
                }
                intersect_in_place(&mut candidate, &anchor_bits);
                selected_anchor_df = Some(anchor.postings.len());
                selected_anchor_width = Some(3);
            }
        }

        if variant == PrototypeVariant::D && folded_query.len() >= 4 {
            let mut best_higher = None::<usize>;
            for width in 4..=5_u8 {
                if folded_query.len() < width as usize {
                    continue;
                }
                for gram_bytes in folded_query.windows(width as usize) {
                    let value = match width {
                        4 => k4(gram_bytes),
                        5 => k5(gram_bytes),
                        _ => unreachable!(),
                    };
                    let key = higher_key(width, value);
                    if self.higher_ngram_filter.is_enabled()
                        && !self.higher_ngram_filter.might_contain(key)
                    {
                        candidate.fill(0);
                        return (candidate, true, None, None);
                    }
                    if let Some(anchor_index) = self.higher_sparse_lookup.get(&key).copied() {
                        let df = self.higher_sparse_anchors[anchor_index].postings.len();
                        if best_higher
                            .map(|current| df < self.higher_sparse_anchors[current].postings.len())
                            .unwrap_or(true)
                        {
                            best_higher = Some(anchor_index);
                        }
                    }
                }
            }
            if let Some(anchor_index) = best_higher {
                let anchor = &self.higher_sparse_anchors[anchor_index];
                let mut anchor_bits = vec![0_u64; self.words_per_bitmap];
                for block_id in &anchor.postings {
                    set_bit(&mut anchor_bits, *block_id as usize);
                }
                intersect_in_place(&mut candidate, &anchor_bits);
                let df = anchor.postings.len();
                if selected_anchor_df.is_none_or(|current| df < current) {
                    selected_anchor_df = Some(df);
                    selected_anchor_width = Some(higher_key_width(anchor.key));
                }
            }
        }
        (candidate, false, selected_anchor_df, selected_anchor_width)
    }

    fn row<'a>(&self, rows: &'a [u64], row: usize) -> &'a [u64] {
        let start = row * self.words_per_bitmap;
        &rows[start..start + self.words_per_bitmap]
    }

    pub fn write_prototype_index(
        &self,
        corpus: &Corpus,
        variant: PrototypeVariant,
        path: impl AsRef<Path>,
    ) -> io::Result<CapacityReport> {
        let (bytes, report) = self.serialize(corpus, variant);
        let mut file = fs::File::create(path)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        Ok(report)
    }

    pub fn capacity_report(&self, corpus: &Corpus, variant: PrototypeVariant) -> CapacityReport {
        self.serialize(corpus, variant).1
    }

    fn serialize(&self, corpus: &Corpus, variant: PrototypeVariant) -> (Vec<u8>, CapacityReport) {
        let mut out = Vec::new();
        out.extend_from_slice(match variant {
            PrototypeVariant::A => b"PRV2PA02",
            PrototypeVariant::B => b"PRV2PB02",
            PrototypeVariant::C => b"PRV2PC02",
            PrototypeVariant::D => b"PRV2PD03",
        });
        put_u32(&mut out, if variant == PrototypeVariant::D { 3 } else { 2 });
        put_u32(&mut out, TARGET_BLOCK_BYTES as u32);
        put_u32(&mut out, corpus.files.len() as u32);
        put_u32(&mut out, corpus.blocks.len() as u32);
        put_u64(&mut out, corpus.selected_source_bytes);
        put_u64(&mut out, corpus.searchable_normalized_bytes);
        out.resize(PROTOTYPE_HEADER_BYTES, 0);

        let catalog_start = out.len();
        put_u32(&mut out, corpus.files.len() as u32);
        for file in &corpus.files {
            put_u32(&mut out, file.path.len() as u32);
            out.extend_from_slice(file.path.as_bytes());
            put_u64(&mut out, file.selected_bytes);
        }
        let file_catalog_bytes = out.len() - catalog_start;

        let block_map_start = out.len();
        put_u32(&mut out, corpus.blocks.len() as u32);
        for block in &corpus.blocks {
            put_u64(&mut out, block.searchable_bytes);
            put_u32(&mut out, block.spans.len() as u32);
            for span in &block.spans {
                put_u32(&mut out, span.file_id);
                put_u64(&mut out, span.start);
                put_u64(&mut out, span.end);
            }
        }
        let block_map_bytes = out.len() - block_map_start;

        let unigram_start = out.len();
        append_bitpacked_rows(
            &mut out,
            &self.unigram,
            256,
            self.words_per_bitmap,
            self.block_count,
        );
        let unigram_bytes = out.len() - unigram_start;

        let bigram_start = out.len();
        append_bitpacked_rows(
            &mut out,
            &self.bigram,
            65_536,
            self.words_per_bitmap,
            self.block_count,
        );
        let bigram_bytes = out.len() - bigram_start;

        let (global_trigram_presence_bytes, global_trigram_encoding) = match variant {
            PrototypeVariant::A => (0, GlobalPresenceEncoding::None),
            PrototypeVariant::B => {
                let start = out.len();
                for word in &self.global_trigram_presence {
                    out.extend_from_slice(&word.to_le_bytes());
                }
                (out.len() - start, GlobalPresenceEncoding::Dense)
            }
            PrototypeVariant::C | PrototypeVariant::D => {
                let start = out.len();
                self.adaptive_global_presence.serialize_into(&mut out);
                debug_assert_eq!(
                    out.len() - start,
                    self.adaptive_global_presence.persistent_bytes()
                );
                (out.len() - start, self.adaptive_global_presence.encoding())
            }
        };

        let mut sparse_anchor_metadata_bytes = 0;
        let mut sparse_anchor_posting_bytes = 0;
        if variant != PrototypeVariant::A {
            let sparse_start = out.len();
            if !self.sparse_anchors.is_empty() {
                out.resize(out.len() + SPARSE_HEADER_BYTES, 0);
                let record_start = out.len();
                let mut posting_blob = Vec::new();
                for anchor in &self.sparse_anchors {
                    put_u32(&mut out, anchor.gram);
                    put_u32(&mut out, posting_blob.len() as u32);
                    out.push(anchor.postings.len() as u8);
                    out.push(0);
                    append_delta_varints(&mut posting_blob, &anchor.postings);
                }
                sparse_anchor_metadata_bytes = out.len() - record_start + SPARSE_HEADER_BYTES;
                sparse_anchor_posting_bytes = posting_blob.len();
                out.extend_from_slice(&posting_blob);
            }
            let sparse_bytes = out.len() - sparse_start;
            debug_assert_eq!(
                sparse_bytes,
                sparse_anchor_metadata_bytes + sparse_anchor_posting_bytes
            );
            let budget = ((corpus.selected_source_bytes as u128 * SPARSE_BUDGET_NUMERATOR as u128)
                / SPARSE_BUDGET_DENOMINATOR as u128) as usize;
            debug_assert!(sparse_bytes <= budget);
        }

        let mut higher_ngram_filter_bytes = 0;
        let mut higher_sparse_anchor_metadata_bytes = 0;
        let mut higher_sparse_anchor_posting_bytes = 0;
        if variant == PrototypeVariant::D {
            let filter_start = out.len();
            self.higher_ngram_filter.serialize_into(&mut out);
            higher_ngram_filter_bytes = out.len() - filter_start;
            debug_assert_eq!(
                higher_ngram_filter_bytes,
                self.higher_ngram_filter.persistent_bytes()
            );

            let sparse_start = out.len();
            if !self.higher_sparse_anchors.is_empty() {
                out.resize(out.len() + HIGHER_SPARSE_HEADER_BYTES, 0);
                let record_start = out.len();
                let mut posting_blob = Vec::new();
                for anchor in &self.higher_sparse_anchors {
                    put_u64(&mut out, anchor.key);
                    put_u32(&mut out, posting_blob.len() as u32);
                    out.push(anchor.postings.len() as u8);
                    out.push(0);
                    append_delta_varints(&mut posting_blob, &anchor.postings);
                }
                higher_sparse_anchor_metadata_bytes =
                    out.len() - record_start + HIGHER_SPARSE_HEADER_BYTES;
                higher_sparse_anchor_posting_bytes = posting_blob.len();
                out.extend_from_slice(&posting_blob);
            }
            let higher_sparse_bytes = out.len() - sparse_start;
            debug_assert_eq!(
                higher_sparse_bytes,
                higher_sparse_anchor_metadata_bytes + higher_sparse_anchor_posting_bytes
            );
            let higher_budget = ((corpus.selected_source_bytes as u128
                * HIGHER_SPARSE_BUDGET_NUMERATOR as u128)
                / SPARSE_BUDGET_DENOMINATOR as u128) as usize;
            debug_assert!(higher_sparse_bytes <= higher_budget);
            let total_sparse = sparse_anchor_metadata_bytes
                + sparse_anchor_posting_bytes
                + higher_sparse_anchor_metadata_bytes
                + higher_sparse_anchor_posting_bytes;
            let total_budget = ((corpus.selected_source_bytes as u128
                * SPARSE_BUDGET_NUMERATOR as u128)
                / SPARSE_BUDGET_DENOMINATOR as u128) as usize;
            debug_assert!(total_sparse <= total_budget);
        }

        let report = CapacityReport {
            variant,
            selected_source_bytes: corpus.selected_source_bytes,
            searchable_normalized_bytes: corpus.searchable_normalized_bytes,
            block_count: corpus.blocks.len(),
            file_catalog_bytes,
            block_map_bytes,
            unigram_bytes,
            bigram_bytes,
            global_trigram_presence_bytes,
            global_trigram_encoding,
            observed_trigram_count: self.observed_trigram_count,
            sparse_anchor_metadata_bytes,
            sparse_anchor_posting_bytes,
            higher_ngram_filter_bytes,
            higher_sparse_anchor_metadata_bytes,
            higher_sparse_anchor_posting_bytes,
            total_persistent_bytes: out.len(),
        };
        (out, report)
    }
}

pub fn fold_ascii(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().map(u8::to_ascii_lowercase).collect()
}

pub fn naive_search(corpus: &Corpus, query: &str, case_sensitive: bool) -> Vec<SearchHit> {
    if query.is_empty() {
        return Vec::new();
    }
    let normalized_query = normalize_str(query, case_sensitive);
    let needle = normalized_query.bytes();
    let mut hits = Vec::new();
    for unit in &corpus.units {
        if let Some(normalized) = normalize_utf8(&unit.raw, case_sensitive) {
            let mut last_origin = None::<u32>;
            for position in find_all_positions_naive(normalized.bytes(), needle) {
                let origin = normalized.origin_at(position);
                if last_origin == Some(origin) {
                    continue;
                }
                last_origin = Some(origin);
                hits.push(SearchHit {
                    file_id: unit.file_id,
                    line_number: unit.line_number,
                    byte_offset_in_line: origin,
                });
            }
        } else {
            let folded;
            let haystack = if case_sensitive {
                unit.raw.as_slice()
            } else {
                folded = fold_ascii(&unit.raw);
                folded.as_slice()
            };
            let fallback_query;
            let fallback_needle = if case_sensitive {
                query.as_bytes()
            } else {
                fallback_query = fold_ascii(query.as_bytes());
                fallback_query.as_slice()
            };
            hits.extend(
                find_all_positions_naive(haystack, fallback_needle).map(|position| SearchHit {
                    file_id: unit.file_id,
                    line_number: unit.line_number,
                    byte_offset_in_line: position as u32,
                }),
            );
        }
    }
    hits
}

struct ExactMatcher<'a> {
    needle: &'a [u8],
    shift: [usize; 256],
    anchor_index: usize,
}

impl<'a> ExactMatcher<'a> {
    fn new(needle: &'a [u8]) -> Self {
        let width = needle.len().max(1);
        let mut shift = [width; 256];
        if needle.len() >= 2 {
            for (index, byte) in needle[..needle.len() - 1].iter().copied().enumerate() {
                shift[byte as usize] = needle.len() - 1 - index;
            }
        }
        Self {
            needle,
            shift,
            anchor_index: needle.len().saturating_sub(1),
        }
    }

    fn find_all<'h>(&'h self, haystack: &'h [u8]) -> ExactMatchIter<'h, 'a> {
        ExactMatchIter {
            matcher: self,
            haystack,
            next: 0,
        }
    }
}

struct ExactMatchIter<'h, 'n> {
    matcher: &'h ExactMatcher<'n>,
    haystack: &'h [u8],
    next: usize,
}

impl Iterator for ExactMatchIter<'_, '_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        let needle = self.matcher.needle;
        if needle.is_empty() {
            return None;
        }
        if needle.len() == 1 {
            let relative = find_byte_wordwise(self.haystack.get(self.next..)?, needle[0])?;
            let found = self.next + relative;
            self.next = found.saturating_add(1);
            return Some(found);
        }
        if needle.len() >= 3 {
            return self.next_anchored();
        }

        let width = needle.len();
        if width > self.haystack.len() || self.next > self.haystack.len() - width {
            return None;
        }
        while self.next <= self.haystack.len() - width {
            let end = self.next + width;
            let last = self.haystack[end - 1];
            if last == needle[width - 1] && self.haystack[self.next..end - 1] == needle[..width - 1]
            {
                let found = self.next;
                self.next = self.next.saturating_add(1);
                return Some(found);
            }
            self.next = self
                .next
                .saturating_add(self.matcher.shift[last as usize].max(1));
        }
        None
    }
}

impl ExactMatchIter<'_, '_> {
    fn next_anchored(&mut self) -> Option<usize> {
        let needle = self.matcher.needle;
        let width = needle.len();
        let anchor_index = self.matcher.anchor_index;
        if width > self.haystack.len() || self.next > self.haystack.len() - width {
            return None;
        }
        let anchor = needle[anchor_index];
        let mut anchor_search = self.next + anchor_index;
        while anchor_search < self.haystack.len() {
            let relative = find_byte_wordwise(&self.haystack[anchor_search..], anchor)?;
            let anchor_position = anchor_search + relative;
            if anchor_position < anchor_index {
                return None;
            }
            let start = anchor_position - anchor_index;
            if start + width > self.haystack.len() {
                return None;
            }
            if start >= self.next && self.haystack[start..start + width] == *needle {
                self.next = start.saturating_add(1);
                return Some(start);
            }
            anchor_search = anchor_position.saturating_add(1);
        }
        None
    }
}

fn find_byte_wordwise(haystack: &[u8], needle: u8) -> Option<usize> {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGHS: u64 = 0x8080_8080_8080_8080;
    let repeated = u64::from_ne_bytes([needle; 8]);
    let mut offset = 0_usize;
    let mut chunks = haystack.chunks_exact(8);
    for chunk in &mut chunks {
        let bytes: [u8; 8] = chunk.try_into().expect("exact chunk size");
        let value = u64::from_ne_bytes(bytes) ^ repeated;
        let maybe_zero = value.wrapping_sub(ONES) & !value & HIGHS;
        if maybe_zero != 0 {
            for (index, byte) in chunk.iter().copied().enumerate() {
                if byte == needle {
                    return Some(offset + index);
                }
            }
        }
        offset += 8;
    }
    chunks
        .remainder()
        .iter()
        .position(|byte| *byte == needle)
        .map(|index| offset + index)
}

fn find_all_positions_naive<'a>(
    haystack: &'a [u8],
    needle: &'a [u8],
) -> impl Iterator<Item = usize> + 'a {
    let mut next = 0_usize;
    std::iter::from_fn(move || {
        if needle.is_empty()
            || needle.len() > haystack.len()
            || next > haystack.len() - needle.len()
        {
            return None;
        }
        let relative = haystack[next..]
            .windows(needle.len())
            .position(|window| window == needle)?;
        let found = next + relative;
        next = found.saturating_add(1);
        Some(found)
    })
}

fn set_row_bit(rows: &mut [u64], words_per_bitmap: usize, row: usize, block_id: usize) {
    let start = row * words_per_bitmap;
    rows[start + block_id / 64] |= 1_u64 << (block_id % 64);
}

fn set_bit(words: &mut [u64], bit: usize) {
    words[bit / 64] |= 1_u64 << (bit % 64);
}

fn intersect_in_place(left: &mut [u64], right: &[u64]) {
    for (lhs, rhs) in left.iter_mut().zip(right) {
        *lhs &= *rhs;
    }
}

fn collect_set_bits(words: &[u64], bit_limit: usize) -> Vec<usize> {
    let mut out = Vec::new();
    for (word_index, word) in words.iter().copied().enumerate() {
        let mut remaining = word;
        while remaining != 0 {
            let offset = remaining.trailing_zeros() as usize;
            let bit = word_index * 64 + offset;
            if bit < bit_limit {
                out.push(bit);
            }
            remaining &= remaining - 1;
        }
    }
    out
}

fn collect_observed_trigrams(words: &[u64]) -> Vec<u32> {
    let mut out = Vec::new();
    for (word_index, word) in words.iter().copied().enumerate() {
        let mut remaining = word;
        while remaining != 0 {
            let offset = remaining.trailing_zeros();
            out.push(word_index as u32 * 64 + offset);
            remaining &= remaining - 1;
        }
    }
    out
}

fn global_trigram_present(words: &[u64], gram: u32) -> bool {
    let index = gram as usize;
    words[index / 64] & (1_u64 << (index % 64)) != 0
}

const fn k2(a: u8, b: u8) -> u16 {
    (a as u16) | ((b as u16) << 8)
}

const fn k3(a: u8, b: u8, c: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16)
}

fn k4(bytes: &[u8]) -> u64 {
    debug_assert_eq!(bytes.len(), 4);
    (bytes[0] as u64)
        | ((bytes[1] as u64) << 8)
        | ((bytes[2] as u64) << 16)
        | ((bytes[3] as u64) << 24)
}

fn k5(bytes: &[u8]) -> u64 {
    debug_assert_eq!(bytes.len(), 5);
    k4(&bytes[..4]) | ((bytes[4] as u64) << 32)
}

fn append_bitpacked_rows(
    out: &mut Vec<u8>,
    rows: &[u64],
    row_count: usize,
    words_per_bitmap: usize,
    block_count: usize,
) {
    let bytes_per_row = block_count.div_ceil(8);
    for row in 0..row_count {
        let start = row * words_per_bitmap;
        let row_words = &rows[start..start + words_per_bitmap];
        for byte_index in 0..bytes_per_row {
            let mut byte = 0_u8;
            for bit_in_byte in 0..8 {
                let block_id = byte_index * 8 + bit_in_byte;
                if block_id >= block_count {
                    break;
                }
                if row_words[block_id / 64] & (1_u64 << (block_id % 64)) != 0 {
                    byte |= 1_u8 << bit_in_byte;
                }
            }
            out.push(byte);
        }
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
        bytes += 1;
        value >>= 7;
    }
    bytes
}

fn put_u24(out: &mut Vec<u8>, value: u32) {
    debug_assert!(value < (1 << 24));
    out.push(value as u8);
    out.push((value >> 8) as u8);
    out.push((value >> 16) as u8);
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

#[derive(Clone, Debug)]
pub struct BenchmarkStats {
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
    pub max: Duration,
    pub filter_p50: Duration,
    pub verify_p50: Duration,
    pub assembly_p50: Duration,
    pub representative_metrics: SearchMetrics,
}

pub fn benchmark_query(
    index: &PrototypeIndex,
    corpus: &Corpus,
    variant: PrototypeVariant,
    query: &str,
    case_sensitive: bool,
    repeats: usize,
) -> BenchmarkStats {
    let repeats = repeats.max(1);
    let _ = index.search_first_batch(corpus, query, case_sensitive, variant);
    let mut totals = Vec::with_capacity(repeats);
    let mut filters = Vec::with_capacity(repeats);
    let mut verifies = Vec::with_capacity(repeats);
    let mut assemblies = Vec::with_capacity(repeats);
    let mut representative = SearchMetrics::default();
    for run in 0..repeats {
        let started = Instant::now();
        let outcome = index.search_first_batch(corpus, query, case_sensitive, variant);
        totals.push(started.elapsed());
        filters.push(outcome.metrics.filter_time);
        verifies.push(outcome.metrics.verify_time);
        assemblies.push(outcome.metrics.result_assembly_time);
        if run == 0 {
            representative = outcome.metrics;
        }
    }
    totals.sort_unstable();
    filters.sort_unstable();
    verifies.sort_unstable();
    assemblies.sort_unstable();
    BenchmarkStats {
        p50: percentile(&totals, 50),
        p95: percentile(&totals, 95),
        p99: percentile(&totals, 99),
        max: *totals.last().expect("repeats >= 1"),
        filter_p50: percentile(&filters, 50),
        verify_p50: percentile(&verifies, 50),
        assembly_p50: percentile(&assemblies, 50),
        representative_metrics: representative,
    }
}

fn percentile(values: &[Duration], percentile: usize) -> Duration {
    let index = ((values.len() - 1) * percentile).div_ceil(100);
    values[index.min(values.len() - 1)]
}

#[cfg(test)]
mod adaptive_tests {
    use super::*;

    #[test]
    fn adaptive_presence_chooses_smallest_exact_representation_and_preserves_membership() {
        let mut sparse = vec![0_u64; GLOBAL_TRIGRAM_WORDS];
        for gram in [1_u32, 2, 3, 0x123456, 0xabcdef] {
            sparse[gram as usize / 64] |= 1_u64 << (gram % 64);
        }
        let adaptive = AdaptiveGlobalPresence::from_dense(&sparse);
        assert_ne!(adaptive.encoding(), GlobalPresenceEncoding::Dense);
        for gram in [1_u32, 2, 3, 0x123456, 0xabcdef] {
            assert!(adaptive.contains(gram));
        }
        for gram in [0_u32, 4, 0x123457, 0xfffffe] {
            assert!(!adaptive.contains(gram));
        }

        let dense = vec![u64::MAX; GLOBAL_TRIGRAM_WORDS];
        let adaptive = AdaptiveGlobalPresence::from_dense(&dense);
        assert_eq!(adaptive.encoding(), GlobalPresenceEncoding::Dense);
        assert!(adaptive.contains(0));
        assert!(adaptive.contains(0x00ff_ffff));
    }
}

mod persistent;
pub use persistent::*;

mod metadata;
pub use metadata::*;
