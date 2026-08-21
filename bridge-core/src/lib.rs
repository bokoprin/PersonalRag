mod build_order;
mod change_tracker;
mod contract_v1;
mod engine;
mod extractor;
mod incremental;
mod office_cache;
mod progress_rate;
mod windows_native_scanner;

pub use change_tracker::*;
pub use contract_v1::*;
pub use engine::*;
pub use extractor::*;
pub use incremental::*;
pub use office_cache::*;
pub use progress_rate::*;

use std::{
    cmp::Ordering,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering as AtomicOrdering},
        Arc, Mutex,
    },
};

use ignore::{overrides::OverrideBuilder, WalkBuilder, WalkState};
use personalrag_portable_search::{
    LazyPersistentIndex, MergedIndex, MergedSearchSession, PersistentIndex, VNextGenerationIndex,
};
use regex::{Regex, RegexBuilder};

#[derive(Debug, Clone, Default)]
pub struct ScanExclusions {
    pub dev_caches: bool,
    pub virtual_envs: bool,
    pub node_modules: bool,
    pub build_artifacts: bool,
    pub vcs: bool,
    pub use_gitignore: bool,
    pub custom_directory_names: Vec<String>,
    pub custom_relative_paths: Vec<String>,
    pub custom_globs: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScannerMode {
    Auto,
    WalkDir,
    WindowsNative,
}

impl ScannerMode {
    #[must_use]
    pub fn parse(value: &str) -> Self {
        match value {
            "walk_dir" => Self::WalkDir,
            "windows_native" => Self::WindowsNative,
            _ => Self::Auto,
        }
    }

    #[must_use]
    pub fn traversal_threads(self) -> usize {
        let cpus = std::thread::available_parallelism().map_or(1, usize::from);
        match self {
            Self::WalkDir => 1,
            Self::WindowsNative => cpus.clamp(1, 8),
            Self::Auto => {
                if cfg!(windows) {
                    cpus.clamp(1, 8)
                } else {
                    cpus.clamp(1, 4)
                }
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ScanProgress {
    pub discovered_entries: usize,
    pub selected_files: usize,
    pub pruned_entries: usize,
    pub error_entries: usize,
    pub selected_bytes: u64,
    pub current_path: Option<PathBuf>,
}

#[derive(Debug)]
pub struct ScanReport {
    pub files: Vec<ScannedFile>,
    pub progress: ScanProgress,
    pub directory_tracking: Option<DirectoryTrackingSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedFile {
    pub path: PathBuf,
    pub display_path: String,
    pub size_bytes: u64,
    pub modified_ns: u64,
    /// `false` keeps filename/path searchable while skipping file-body hydration.
    pub index_content: bool,
}

struct ScanPathBatch {
    local: Vec<ScannedFile>,
    shared: Arc<Mutex<Vec<ScannedFile>>>,
}

impl ScanPathBatch {
    fn new(shared: Arc<Mutex<Vec<ScannedFile>>>) -> Self {
        Self {
            local: Vec::with_capacity(4_096),
            shared,
        }
    }

    fn push(&mut self, file: ScannedFile) -> Result<(), ()> {
        self.local.push(file);
        if self.local.len() >= 4_096 {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), ()> {
        if self.local.is_empty() {
            return Ok(());
        }
        let mut shared = self.shared.lock().map_err(|_| ())?;
        shared.append(&mut self.local);
        Ok(())
    }
}

impl Drop for ScanPathBatch {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

fn normalized_relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

fn ascii_eq(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

fn env_variant(name: &str, prefix: &str) -> bool {
    let Some(suffix) = name.get(prefix.len()..) else {
        return false;
    };
    name.get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        && !suffix.is_empty()
        && suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.'))
}

fn build_variant(name: &str) -> bool {
    ["cmake-build-", "build-", "out-"].iter().any(|prefix| {
        name.get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    })
}

fn content_index_eligible(path: &Path) -> bool {
    content_search_eligible(path)
}

fn standard_pruned_dir(name: &str, config: &ScanExclusions) -> bool {
    (config.node_modules && ascii_eq(name, "node_modules"))
        || (config.vcs
            && [".git", ".hg", ".svn"]
                .iter()
                .any(|candidate| ascii_eq(name, candidate)))
        || (config.virtual_envs
            && ([".venv", "venv", "env", ".tox", ".nox"]
                .iter()
                .any(|candidate| ascii_eq(name, candidate))
                || env_variant(name, ".venv")
                || env_variant(name, "venv")))
        || (config.dev_caches
            && [
                "__pycache__",
                ".pytest_cache",
                ".mypy_cache",
                ".ruff_cache",
                ".cache",
                "cache",
                "caches",
            ]
            .iter()
            .any(|candidate| ascii_eq(name, candidate)))
        || (config.build_artifacts
            && ([
                "build", "dist", "out", "target", ".next", ".nuxt", "coverage", ".gradle", "bin",
                "obj", ".vs", "debug", "release",
            ]
            .iter()
            .any(|candidate| ascii_eq(name, candidate))
                || build_variant(name)))
        || config
            .custom_directory_names
            .iter()
            .any(|candidate| ascii_eq(name, candidate))
}

fn relative_path_pruned(relative: &str, config: &ScanExclusions) -> bool {
    config.custom_relative_paths.iter().any(|candidate| {
        let candidate = candidate.trim_matches(['/', '\\']);
        relative.eq_ignore_ascii_case(candidate)
            || relative
                .get(..candidate.len())
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case(candidate))
                && relative
                    .as_bytes()
                    .get(candidate.len())
                    .is_some_and(|byte| *byte == b'/')
    })
}

fn build_override(
    root: &Path,
    config: &ScanExclusions,
) -> Result<Option<ignore::overrides::Override>, String> {
    if config.custom_globs.is_empty() {
        return Ok(None);
    }
    let mut builder = OverrideBuilder::new(root);
    for pattern in &config.custom_globs {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            continue;
        }
        let ignore_pattern = if pattern.starts_with('!') {
            pattern.to_owned()
        } else {
            format!("!{pattern}")
        };
        builder
            .add(&ignore_pattern)
            .map_err(|error| format!("invalid custom glob {pattern:?}: {error}"))?;
    }
    builder.build().map(Some).map_err(|error| error.to_string())
}

fn configure_walker(
    root: &Path,
    mode: ScannerMode,
    config: &ScanExclusions,
    pruned: Arc<AtomicUsize>,
) -> Result<WalkBuilder, String> {
    let mut builder = WalkBuilder::new(root);
    builder
        .follow_links(false)
        .hidden(false)
        .ignore(false)
        .git_ignore(config.use_gitignore)
        .git_global(config.use_gitignore)
        .git_exclude(config.use_gitignore)
        .parents(config.use_gitignore)
        .require_git(false)
        .threads(mode.traversal_threads());
    if let Some(overrides) = build_override(root, config)? {
        builder.overrides(overrides);
    }
    let root = root.to_path_buf();
    let config = config.clone();
    let has_relative_pruning = !config.custom_relative_paths.is_empty();
    builder.filter_entry(move |entry| {
        let path = entry.path();
        if path == root {
            return true;
        }
        if has_relative_pruning {
            let relative = normalized_relative(&root, path);
            if relative_path_pruned(&relative, &config) {
                pruned.fetch_add(1, AtomicOrdering::Relaxed);
                return false;
            }
        }
        if entry.file_type().is_some_and(|kind| kind.is_dir()) {
            let name = entry.file_name().to_string_lossy();
            if standard_pruned_dir(&name, &config) {
                pruned.fetch_add(1, AtomicOrdering::Relaxed);
                return false;
            }
        }
        true
    });
    Ok(builder)
}

/// Traverse a root with GUI exclusion settings and return only regular files eligible for indexing.
///
/// The walker never follows directory symlinks/reparse points. The parallel modes only retain
/// paths and metadata; file contents are hydrated later by the bounded portable build pipeline.
pub fn scan_files(
    root: &Path,
    max_file_bytes: u64,
    mode: ScannerMode,
    config: &ScanExclusions,
    cancel: Arc<AtomicBool>,
    on_progress: Arc<dyn Fn(ScanProgress) + Send + Sync>,
) -> Result<ScanReport, String> {
    if !root.is_dir() {
        return Err(format!("root is not a directory: {}", root.display()));
    }
    if windows_native_scanner::native_batch_compatible(mode, config) {
        if let Some(report) = windows_native_scanner::scan_files_native(
            root,
            max_file_bytes,
            mode,
            config,
            Arc::clone(&cancel),
            Arc::clone(&on_progress),
        )? {
            return Ok(report);
        }
    }
    let files = Arc::new(Mutex::new(Vec::<ScannedFile>::new()));
    let discovered = Arc::new(AtomicUsize::new(0));
    let selected = Arc::new(AtomicUsize::new(0));
    let pruned = Arc::new(AtomicUsize::new(0));
    let errors = Arc::new(AtomicUsize::new(0));
    let selected_bytes = Arc::new(AtomicU64::new(0));
    let current_path = Arc::new(Mutex::new(None::<PathBuf>));
    let tracked_directories = Arc::new(Mutex::new(Vec::<TrackedDirectory>::new()));
    let directory_tracking_complete = Arc::new(AtomicBool::new(cfg!(windows)));
    #[cfg(windows)]
    match directory_file_id(root) {
        Some(file_id) => tracked_directories
            .lock()
            .map_err(|_| "scan directory tracking lock poisoned".to_owned())?
            .push(TrackedDirectory {
                file_id,
                relative_path: String::new(),
            }),
        None => directory_tracking_complete.store(false, AtomicOrdering::Release),
    }
    let walker = configure_walker(root, mode, config, Arc::clone(&pruned))?;

    let progress_snapshot = || ScanProgress {
        discovered_entries: discovered.load(AtomicOrdering::Relaxed),
        selected_files: selected.load(AtomicOrdering::Relaxed),
        pruned_entries: pruned.load(AtomicOrdering::Relaxed),
        error_entries: errors.load(AtomicOrdering::Relaxed),
        selected_bytes: selected_bytes.load(AtomicOrdering::Relaxed),
        current_path: current_path.lock().ok().and_then(|value| value.clone()),
    };

    if mode.traversal_threads() <= 1 {
        for item in walker.build() {
            if cancel.load(AtomicOrdering::Acquire) {
                return Err("cancelled".to_owned());
            }
            let entry = match item {
                Ok(entry) => entry,
                Err(_) => {
                    errors.fetch_add(1, AtomicOrdering::Relaxed);
                    continue;
                }
            };
            let count = discovered.fetch_add(1, AtomicOrdering::Relaxed) + 1;
            if count.is_multiple_of(256) {
                if let Ok(mut path) = current_path.lock() {
                    *path = Some(entry.path().to_path_buf());
                }
            }
            #[cfg(windows)]
            if entry.path() != root && entry.file_type().is_some_and(|kind| kind.is_dir()) {
                match directory_file_id(entry.path()) {
                    Some(file_id) => tracked_directories
                        .lock()
                        .map_err(|_| "scan directory tracking lock poisoned".to_owned())?
                        .push(TrackedDirectory {
                            file_id,
                            relative_path: normalized_relative(root, entry.path()),
                        }),
                    None => directory_tracking_complete.store(false, AtomicOrdering::Release),
                }
            }
            if entry.file_type().is_some_and(|kind| kind.is_file()) {
                match entry.metadata() {
                    Ok(metadata) if max_file_bytes == 0 || metadata.len() <= max_file_bytes => {
                        selected.fetch_add(1, AtomicOrdering::Relaxed);
                        selected_bytes.fetch_add(metadata.len(), AtomicOrdering::Relaxed);
                        let path = entry.into_path();
                        let index_content = content_index_eligible(&path);
                        files
                            .lock()
                            .map_err(|_| "scan file list lock poisoned".to_owned())?
                            .push(ScannedFile {
                                display_path: normalized_relative(root, &path),
                                path,
                                size_bytes: metadata.len(),
                                modified_ns: modified_ns(&metadata),
                                index_content,
                            });
                    }
                    Ok(_) => {
                        pruned.fetch_add(1, AtomicOrdering::Relaxed);
                    }
                    Err(_) => {
                        errors.fetch_add(1, AtomicOrdering::Relaxed);
                    }
                }
            }
            if count.is_multiple_of(512) {
                on_progress(progress_snapshot());
            }
        }
    } else {
        let files_for_walk = Arc::clone(&files);
        let discovered_for_walk = Arc::clone(&discovered);
        let selected_for_walk = Arc::clone(&selected);
        let pruned_for_walk = Arc::clone(&pruned);
        let errors_for_walk = Arc::clone(&errors);
        let selected_bytes_for_walk = Arc::clone(&selected_bytes);
        let current_for_walk = Arc::clone(&current_path);
        let cancel_for_walk = Arc::clone(&cancel);
        let progress_for_walk = Arc::clone(&on_progress);
        #[cfg(windows)]
        let tracked_directories_for_walk = Arc::clone(&tracked_directories);
        #[cfg(windows)]
        let directory_tracking_complete_for_walk = Arc::clone(&directory_tracking_complete);
        let root_for_walk = root.to_path_buf();
        walker.build_parallel().run(|| {
            let files = Arc::clone(&files_for_walk);
            let discovered = Arc::clone(&discovered_for_walk);
            let selected = Arc::clone(&selected_for_walk);
            let pruned = Arc::clone(&pruned_for_walk);
            let errors = Arc::clone(&errors_for_walk);
            let selected_bytes = Arc::clone(&selected_bytes_for_walk);
            let current_path = Arc::clone(&current_for_walk);
            let cancel = Arc::clone(&cancel_for_walk);
            let on_progress = Arc::clone(&progress_for_walk);
            #[cfg(windows)]
            let tracked_directories = Arc::clone(&tracked_directories_for_walk);
            #[cfg(windows)]
            let directory_tracking_complete = Arc::clone(&directory_tracking_complete_for_walk);
            let root = root_for_walk.clone();
            let mut path_batch = ScanPathBatch::new(files);
            Box::new(move |item| {
                if cancel.load(AtomicOrdering::Acquire) {
                    return WalkState::Quit;
                }
                let entry = match item {
                    Ok(entry) => entry,
                    Err(_) => {
                        errors.fetch_add(1, AtomicOrdering::Relaxed);
                        return WalkState::Continue;
                    }
                };
                let count = discovered.fetch_add(1, AtomicOrdering::Relaxed) + 1;
                let report_path = count
                    .is_multiple_of(1024)
                    .then(|| entry.path().to_path_buf());
                #[cfg(windows)]
                if entry.path() != root && entry.file_type().is_some_and(|kind| kind.is_dir()) {
                    match directory_file_id(entry.path()) {
                        Some(file_id) => match tracked_directories.lock() {
                            Ok(mut tracked) => tracked.push(TrackedDirectory {
                                file_id,
                                relative_path: normalized_relative(&root, entry.path()),
                            }),
                            Err(_) => {
                                errors.fetch_add(1, AtomicOrdering::Relaxed);
                                return WalkState::Quit;
                            }
                        },
                        None => directory_tracking_complete.store(false, AtomicOrdering::Release),
                    }
                }
                if entry.file_type().is_some_and(|kind| kind.is_file()) {
                    match entry.metadata() {
                        Ok(metadata) if max_file_bytes == 0 || metadata.len() <= max_file_bytes => {
                            selected.fetch_add(1, AtomicOrdering::Relaxed);
                            selected_bytes.fetch_add(metadata.len(), AtomicOrdering::Relaxed);
                            let path = entry.into_path();
                            let index_content = content_index_eligible(&path);
                            if path_batch
                                .push(ScannedFile {
                                    display_path: normalized_relative(&root, &path),
                                    path,
                                    size_bytes: metadata.len(),
                                    modified_ns: modified_ns(&metadata),
                                    index_content,
                                })
                                .is_err()
                            {
                                errors.fetch_add(1, AtomicOrdering::Relaxed);
                                return WalkState::Quit;
                            }
                        }
                        Ok(_) => {
                            pruned.fetch_add(1, AtomicOrdering::Relaxed);
                        }
                        Err(_) => {
                            errors.fetch_add(1, AtomicOrdering::Relaxed);
                        }
                    }
                }
                if count.is_multiple_of(1024) {
                    if let Ok(mut path) = current_path.lock() {
                        *path = report_path;
                    }
                    on_progress(ScanProgress {
                        discovered_entries: count,
                        selected_files: selected.load(AtomicOrdering::Relaxed),
                        pruned_entries: pruned.load(AtomicOrdering::Relaxed),
                        error_entries: errors.load(AtomicOrdering::Relaxed),
                        selected_bytes: selected_bytes.load(AtomicOrdering::Relaxed),
                        current_path: current_path.lock().ok().and_then(|value| value.clone()),
                    });
                }
                WalkState::Continue
            })
        });
        if cancel.load(AtomicOrdering::Acquire) {
            return Err("cancelled".to_owned());
        }
    }

    let final_progress = progress_snapshot();
    on_progress(final_progress.clone());
    let files = Arc::try_unwrap(files)
        .map_err(|_| "scan file list still shared".to_owned())?
        .into_inner()
        .map_err(|_| "scan file list lock poisoned".to_owned())?;
    let directory_tracking = if directory_tracking_complete.load(AtomicOrdering::Acquire) {
        let mut directories = Arc::try_unwrap(tracked_directories)
            .map_err(|_| "scan directory tracking still shared".to_owned())?
            .into_inner()
            .map_err(|_| "scan directory tracking lock poisoned".to_owned())?;
        directories.sort_by_key(|directory| directory.file_id);
        directories.dedup_by_key(|directory| directory.file_id);
        directories.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        Some(DirectoryTrackingSnapshot::new(true, directories))
    } else {
        None
    };
    Ok(ScanReport {
        files,
        progress: final_progress,
        directory_tracking,
    })
}

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub file_query: Option<String>,
    pub include_path: bool,
    pub content_query: Option<String>,
    pub extensions: Vec<String>,
    pub path_scope: Option<String>,
    pub match_case: bool,
    pub whole_words: bool,
    pub regex: bool,
    pub sort_field: String,
    pub sort_direction: String,
    pub limit: usize,
    pub backend: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub file_id: u32,
    pub path: String,
    pub name: String,
    pub extension: String,
    pub size_bytes: u64,
    pub modified_ns: u64,
    pub content_state: String,
}

fn ascii_case_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
}

fn contains_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    needle.len() <= haystack.len()
        && haystack
            .windows(needle.len())
            .any(|window| ascii_case_eq(window, needle))
}

fn contains_plain(haystack: &str, needle: &str, match_case: bool) -> bool {
    if match_case {
        haystack.contains(needle)
    } else {
        contains_ascii_case_insensitive(haystack.as_bytes(), needle.as_bytes())
    }
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn contains_whole_word(haystack: &str, needle: &str, match_case: bool) -> bool {
    if needle.is_empty() {
        return true;
    }
    let bytes = haystack.as_bytes();
    let needle_bytes = needle.as_bytes();
    if needle_bytes.len() > bytes.len() {
        return false;
    }
    for start in 0..=bytes.len() - needle_bytes.len() {
        let matched = if match_case {
            bytes[start..start + needle_bytes.len()] == *needle_bytes
        } else {
            ascii_case_eq(&bytes[start..start + needle_bytes.len()], needle_bytes)
        };
        if matched {
            let end = start + needle_bytes.len();
            let left_ok = start == 0 || !is_word_byte(bytes[start - 1]);
            let right_ok = end >= bytes.len() || !is_word_byte(bytes[end]);
            if left_ok && right_ok {
                return true;
            }
        }
    }
    false
}

fn regex_for(pattern: &str, match_case: bool, whole_words: bool) -> Result<Regex, String> {
    let pattern = if whole_words {
        format!(r"\b(?:{pattern})\b")
    } else {
        pattern.to_owned()
    };
    RegexBuilder::new(&pattern)
        .case_insensitive(!match_case)
        .build()
        .map_err(|error| format!("invalid regex: {error}"))
}

fn required_regex_literal_prefix(pattern: &str) -> Option<String> {
    if pattern.is_empty() {
        return None;
    }
    let mut escaped = false;
    let mut in_class = false;
    for ch in pattern.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '[' => in_class = true,
            ']' if in_class => in_class = false,
            '|' if !in_class => return None,
            _ => {}
        }
    }

    let mut chars = pattern.char_indices().peekable();
    if matches!(chars.peek(), Some((_, '^'))) {
        chars.next();
    }
    let mut literal = String::new();
    let mut last_literal_start = None::<usize>;
    while let Some((_, ch)) = chars.next() {
        match ch {
            '\\' => {
                let Some((_, escaped_ch)) = chars.next() else {
                    break;
                };
                if escaped_ch.is_ascii_alphanumeric() {
                    // \d, \w, \b, backreferences and similar escapes are semantic, not literals.
                    break;
                }
                last_literal_start = Some(literal.len());
                literal.push(escaped_ch);
            }
            '.' | '[' | '(' | ')' | '$' | '^' | '+' => break,
            '?' | '*' | '{' => {
                if let Some(start) = last_literal_start {
                    literal.truncate(start);
                }
                break;
            }
            _ => {
                last_literal_start = Some(literal.len());
                literal.push(ch);
            }
        }
    }
    (literal.len() >= 2).then_some(literal)
}

fn plain_or_regex_match(
    value: &str,
    query: &str,
    match_case: bool,
    whole_words: bool,
    regex: Option<&Regex>,
) -> bool {
    if let Some(regex) = regex {
        regex.is_match(value)
    } else if whole_words {
        contains_whole_word(value, query, match_case)
    } else {
        contains_plain(value, query, match_case)
    }
}

fn intersect_sorted(left: Vec<u32>, right: Vec<u32>) -> Vec<u32> {
    let mut output = Vec::with_capacity(left.len().min(right.len()));
    let (mut i, mut j) = (0usize, 0usize);
    while i < left.len() && j < right.len() {
        match left[i].cmp(&right[j]) {
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
            Ordering::Equal => {
                output.push(left[i]);
                i += 1;
                j += 1;
            }
        }
    }
    output
}

#[derive(Debug, Clone, Copy)]
pub struct GenerationSearchCatalog<'a> {
    pub root: &'a Path,
    pub paths: &'a [String],
    pub size_bytes: &'a [u64],
    pub modified_ns: &'a [u64],
    pub logical_ids: &'a [u64],
    pub logical_to_row: &'a [u32],
    pub generation: u64,
    /// Optional logical-ID order matching the requested GUI sort. When supplied, first-N
    /// candidate search can terminate in final presentation order instead of materializing all hits.
    pub first_n_logical_order: Option<&'a [u64]>,
    /// Rank for each GUI row in `first_n_logical_order`, used when an index-driven candidate
    /// path returns hits in posting order rather than presentation order.
    pub first_n_rank_by_row: Option<&'a [u32]>,
}

#[derive(Clone, Copy)]
pub(crate) enum GenerationCandidateReader<'a> {
    Perf12(&'a MergedSearchSession),
    VNext(&'a VNextGenerationIndex),
}

struct CandidateSearchRequest<'a> {
    index_dir: &'a Path,
    backend: &'a str,
    file_query: Option<&'a str>,
    content_query: Option<&'a str>,
    first_n: Option<(bool, usize)>,
    first_n_conjunctive: Option<usize>,
    first_n_logical_order: Option<&'a [u64]>,
    first_n_rank_by_row: Option<&'a [u32]>,
    logical_ids: &'a [u64],
    logical_to_row: &'a [u32],
    catalog_generation: u64,
    generation_reader: Option<GenerationCandidateReader<'a>>,
}

fn smallest_rows_for_logical_hits(
    logical_hits: Vec<u64>,
    logical_to_row: &[u32],
    limit: usize,
) -> Result<Vec<u32>, String> {
    use std::collections::BinaryHeap;

    let mut rows = BinaryHeap::with_capacity(limit.saturating_add(1));
    for logical_id in logical_hits {
        let logical_index = usize::try_from(logical_id)
            .map_err(|_| "logical document ID exceeds address space".to_owned())?;
        let row = logical_to_row
            .get(logical_index)
            .copied()
            .unwrap_or(u32::MAX);
        if row == u32::MAX {
            return Err(
                "generation hit is missing from GUI logical catalog; rebuild index".to_owned(),
            );
        }
        if rows.len() < limit {
            rows.push(row);
        } else if rows.peek().is_some_and(|largest| row < *largest) {
            rows.pop();
            rows.push(row);
        }
    }
    let mut out = rows.into_vec();
    out.sort_unstable();
    Ok(out)
}

fn map_logical_hits_preserving_order(
    logical_hits: Vec<u64>,
    logical_to_row: &[u32],
) -> Result<Vec<u32>, String> {
    logical_hits
        .into_iter()
        .map(|logical_id| {
            let logical_index = usize::try_from(logical_id)
                .map_err(|_| "logical document ID exceeds address space".to_owned())?;
            let row = logical_to_row
                .get(logical_index)
                .copied()
                .unwrap_or(u32::MAX);
            if row == u32::MAX {
                return Err(
                    "generation hit is missing from GUI logical catalog; rebuild index".to_owned(),
                );
            }
            Ok(row)
        })
        .collect()
}

fn rank_rows(mut rows: Vec<u32>, rank_by_row: Option<&[u32]>, limit: usize) -> Vec<u32> {
    if let Some(ranks) = rank_by_row {
        rows.sort_unstable_by_key(|row| ranks.get(*row as usize).copied().unwrap_or(u32::MAX));
    } else {
        rows.sort_unstable();
    }
    rows.truncate(limit);
    rows
}

fn search_candidates(request: CandidateSearchRequest<'_>) -> Result<(usize, Vec<u32>), String> {
    let CandidateSearchRequest {
        index_dir,
        backend,
        file_query,
        content_query,
        first_n,
        first_n_conjunctive,
        first_n_logical_order,
        first_n_rank_by_row,
        logical_ids,
        logical_to_row,
        catalog_generation,
        generation_reader,
    } = request;
    if index_dir.join("CURRENT").exists() || generation_reader.is_some() {
        let owned_index = if generation_reader.is_none() {
            Some(MergedIndex::open(index_dir, false).map_err(|error| error.to_string())?)
        } else {
            None
        };

        let generation = match generation_reader {
            Some(GenerationCandidateReader::Perf12(session)) => session.index().generation(),
            Some(GenerationCandidateReader::VNext(index)) => index.generation(),
            None => owned_index
                .as_ref()
                .ok_or_else(|| "generation index handle is unavailable".to_owned())?
                .generation(),
        };
        let live_docs = match generation_reader {
            Some(GenerationCandidateReader::Perf12(session)) => session.index().live_docs(),
            Some(GenerationCandidateReader::VNext(index)) => index.live_docs(),
            None => owned_index
                .as_ref()
                .ok_or_else(|| "generation index handle is unavailable".to_owned())?
                .live_docs(),
        };
        if generation != catalog_generation {
            return Err(
                "generation index and GUI catalog generations differ; rebuild index".to_owned(),
            );
        }
        if logical_ids.len() != live_docs {
            return Err(
                "generation index and GUI logical catalog counts differ; rebuild index".to_owned(),
            );
        }
        let ordered_logical_ids = first_n_logical_order.unwrap_or(logical_ids);
        if ordered_logical_ids.len() != live_docs {
            return Err(
                "sort-aware first-N order and GUI logical catalog counts differ; rebuild index"
                    .to_owned(),
            );
        }

        let search_name = |query: &[u8]| -> Result<Vec<u64>, String> {
            match generation_reader {
                Some(GenerationCandidateReader::Perf12(session)) => session
                    .search_name(query)
                    .map_err(|error| error.to_string()),
                Some(GenerationCandidateReader::VNext(index)) => {
                    index.search_name(query).map_err(|error| error.to_string())
                }
                None => owned_index
                    .as_ref()
                    .ok_or_else(|| "generation index handle is unavailable".to_owned())?
                    .search_name(query)
                    .map_err(|error| error.to_string()),
            }
        };
        let search_content = |query: &[u8]| -> Result<Vec<u64>, String> {
            match generation_reader {
                Some(GenerationCandidateReader::Perf12(session)) => session
                    .search_content(query)
                    .map_err(|error| error.to_string()),
                Some(GenerationCandidateReader::VNext(index)) => index
                    .search_content(query)
                    .map_err(|error| error.to_string()),
                None => owned_index
                    .as_ref()
                    .ok_or_else(|| "generation index handle is unavailable".to_owned())?
                    .search_content(query)
                    .map_err(|error| error.to_string()),
            }
        };
        if let Some(limit) = first_n_conjunctive {
            if let Some(GenerationCandidateReader::VNext(index)) = generation_reader {
                let logical_hits = index
                    .first_n_conjunctive_in_order(
                        file_query.unwrap_or_default().as_bytes(),
                        content_query.unwrap_or_default().as_bytes(),
                        ordered_logical_ids,
                        limit,
                    )
                    .map_err(|error| error.to_string())?;
                let rows = map_logical_hits_preserving_order(logical_hits, logical_to_row)?;
                return Ok((live_docs, rows));
            }
        }
        if let Some((names, limit)) = first_n {
            let query = if names {
                file_query.unwrap_or_default()
            } else {
                content_query.unwrap_or_default()
            };
            if let Some(GenerationCandidateReader::VNext(index)) = generation_reader {
                let logical_hits = index
                    .first_n_in_order(query.as_bytes(), names, ordered_logical_ids, limit)
                    .map_err(|error| error.to_string())?;
                let rows = map_logical_hits_preserving_order(logical_hits, logical_to_row)?;
                return Ok((live_docs, rows));
            }
            let index = match generation_reader {
                Some(GenerationCandidateReader::Perf12(session)) => session.index(),
                None => owned_index
                    .as_ref()
                    .ok_or_else(|| "generation index handle is unavailable".to_owned())?,
                Some(GenerationCandidateReader::VNext(_)) => unreachable!(),
            };
            if index
                .prefers_index_driven_first_n(query.as_bytes(), names, limit)
                .map_err(|error| error.to_string())?
            {
                let logical_hits = if names {
                    search_name(query.as_bytes())?
                } else {
                    search_content(query.as_bytes())?
                };
                let rows = map_logical_hits_preserving_order(logical_hits, logical_to_row)?;
                return Ok((live_docs, rank_rows(rows, first_n_rank_by_row, limit)));
            }
            let mut rows = Vec::with_capacity(limit.min(ordered_logical_ids.len()));
            for &logical_id in ordered_logical_ids {
                if index
                    .logical_document_contains(logical_id, query.as_bytes(), names)
                    .map_err(|error| error.to_string())?
                {
                    let logical_index = usize::try_from(logical_id)
                        .map_err(|_| "logical document ID exceeds address space".to_owned())?;
                    let row = logical_to_row
                        .get(logical_index)
                        .copied()
                        .unwrap_or(u32::MAX);
                    if row == u32::MAX {
                        return Err(
                            "generation hit is missing from GUI logical catalog; rebuild index"
                                .to_owned(),
                        );
                    }
                    rows.push(row);
                    if rows.len() == limit {
                        break;
                    }
                }
            }
            return Ok((live_docs, rows));
        }
        let logical_hits = match (file_query, content_query) {
            (Some(file), Some(content)) => {
                let names = search_name(file.as_bytes())?;
                let content = search_content(content.as_bytes())?;
                let content_set = content
                    .into_iter()
                    .collect::<std::collections::HashSet<_>>();
                names
                    .into_iter()
                    .filter(|id| content_set.contains(id))
                    .collect::<Vec<_>>()
            }
            (Some(file), None) => search_name(file.as_bytes())?,
            (None, Some(content)) => search_content(content.as_bytes())?,
            (None, None) => Vec::new(),
        };
        let mut rows = Vec::with_capacity(logical_hits.len());
        for logical_id in logical_hits {
            let logical_index = usize::try_from(logical_id)
                .map_err(|_| "logical document ID exceeds address space".to_owned())?;
            let row = logical_to_row
                .get(logical_index)
                .copied()
                .unwrap_or(u32::MAX);
            if row == u32::MAX {
                return Err(
                    "generation hit is missing from GUI logical catalog; rebuild index".to_owned(),
                );
            }
            rows.push(row);
        }
        return Ok((live_docs, rows));
    }
    if backend == "v1" {
        let index = PersistentIndex::open(index_dir, false).map_err(|error| error.to_string())?;
        let ids = if let Some((names, limit)) = first_n {
            let query = if names {
                file_query.unwrap_or_default()
            } else {
                content_query.unwrap_or_default()
            };
            index
                .first_n(query.as_bytes(), names, limit)
                .map_err(|error| error.to_string())?
        } else {
            match (file_query, content_query) {
                (Some(file), Some(content)) => intersect_sorted(
                    index
                        .search_name(file.as_bytes())
                        .map_err(|error| error.to_string())?,
                    index
                        .search_content(content.as_bytes())
                        .map_err(|error| error.to_string())?,
                ),
                (Some(file), None) => index
                    .search_name(file.as_bytes())
                    .map_err(|error| error.to_string())?,
                (None, Some(content)) => index
                    .search_content(content.as_bytes())
                    .map_err(|error| error.to_string())?,
                (None, None) => Vec::new(),
            }
        };
        Ok((index.docs() as usize, ids))
    } else {
        let index = LazyPersistentIndex::open(index_dir).map_err(|error| error.to_string())?;
        let ids = if let Some((names, limit)) = first_n {
            let query = if names {
                file_query.unwrap_or_default()
            } else {
                content_query.unwrap_or_default()
            };
            index
                .first_n(query.as_bytes(), names, limit)
                .map_err(|error| error.to_string())?
        } else {
            match (file_query, content_query) {
                (Some(file), Some(content)) => intersect_sorted(
                    index
                        .search_name(file.as_bytes())
                        .map_err(|error| error.to_string())?,
                    index
                        .search_content(content.as_bytes())
                        .map_err(|error| error.to_string())?,
                ),
                (Some(file), None) => index
                    .search_name(file.as_bytes())
                    .map_err(|error| error.to_string())?,
                (None, Some(content)) => index
                    .search_content(content.as_bytes())
                    .map_err(|error| error.to_string())?,
                (None, None) => Vec::new(),
            }
        };
        Ok((index.docs() as usize, ids))
    }
}

fn modified_ns(metadata: &fs::Metadata) -> u64 {
    use std::time::UNIX_EPOCH;
    metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn scope_matches(root: &Path, relative: &str, scope: Option<&str>) -> bool {
    let Some(scope) = scope.filter(|value| !value.is_empty()) else {
        return true;
    };
    let scope_path = Path::new(scope);
    if scope_path.is_absolute() {
        root.join(relative).starts_with(scope_path)
    } else {
        Path::new(relative).starts_with(scope_path)
    }
}

fn read_text(path: &Path) -> Option<String> {
    fs::read(path)
        .ok()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// Execute all GUI search controls against the portable index and catalog.
pub fn search_catalog<F>(
    index_dir: &Path,
    root: &Path,
    paths: &[String],
    options: &SearchOptions,
    is_cancelled: F,
) -> Result<Vec<SearchResult>, String>
where
    F: Fn() -> bool,
{
    search_catalog_with_metadata(index_dir, root, paths, &[], &[], options, is_cancelled)
}

/// Execute GUI search controls while reusing scan-time metadata aligned with `paths`.
///
/// When both metadata arrays have the same length as `paths`, result construction and
/// size/modified sorting do not issue filesystem metadata calls for every candidate hit.
fn search_catalog_impl<F, R>(
    index_dir: &Path,
    catalog: GenerationSearchCatalog<'_>,
    options: &SearchOptions,
    generation_reader: Option<GenerationCandidateReader<'_>>,
    read_content: R,
    is_cancelled: F,
) -> Result<Vec<SearchResult>, String>
where
    F: Fn() -> bool,
    R: Fn(&Path) -> Option<String>,
{
    let GenerationSearchCatalog {
        root,
        paths,
        size_bytes: catalog_sizes,
        modified_ns: catalog_modified_ns,
        logical_ids,
        logical_to_row,
        generation: catalog_generation,
        first_n_logical_order,
        first_n_rank_by_row,
    } = catalog;
    let file_query = options
        .file_query
        .as_deref()
        .filter(|value| !value.is_empty());
    let content_query = options
        .content_query
        .as_deref()
        .filter(|value| !value.is_empty());
    if file_query.is_none() && content_query.is_none() {
        return Ok(Vec::new());
    }
    let file_regex = if options.regex {
        file_query
            .map(|query| regex_for(query, options.match_case, options.whole_words))
            .transpose()?
    } else {
        None
    };
    let content_regex = if options.regex {
        content_query
            .map(|query| regex_for(query, options.match_case, options.whole_words))
            .transpose()?
    } else {
        None
    };
    let file_regex_prefilter = options
        .regex
        .then(|| file_query.and_then(required_regex_literal_prefix))
        .flatten();
    let content_regex_prefilter = options
        .regex
        .then(|| content_query.and_then(required_regex_literal_prefix))
        .flatten();

    let limit = options.limit.max(1);
    let fast_first_n = (!options.regex
        && !options.match_case
        && !options.whole_words
        && options.extensions.is_empty()
        && options.path_scope.as_deref().is_none_or(str::is_empty)
        && ((options.sort_field == "path" && options.sort_direction != "descending")
            || first_n_logical_order.is_some()))
    .then_some(match (file_query, content_query) {
        (Some(_), None) if options.include_path => Some((true, limit)),
        (None, Some(_)) => Some((false, limit)),
        _ => None,
    })
    .flatten();

    let fast_first_n_conjunctive = (!options.regex
        && !options.match_case
        && !options.whole_words
        && options.extensions.is_empty()
        && options.path_scope.as_deref().is_none_or(str::is_empty)
        && ((options.sort_field == "path" && options.sort_direction != "descending")
            || first_n_logical_order.is_some())
        && options.include_path
        && file_query.is_some()
        && content_query.is_some()
        && matches!(generation_reader, Some(GenerationCandidateReader::VNext(_))))
    .then_some(limit);

    let (docs, mut ids) =
        if options.regex && (file_regex_prefilter.is_some() || content_regex_prefilter.is_some()) {
            search_candidates(CandidateSearchRequest {
                index_dir,
                backend: &options.backend,
                file_query: file_regex_prefilter.as_deref(),
                content_query: content_regex_prefilter.as_deref(),
                first_n: None,
                first_n_conjunctive: None,
                first_n_logical_order: None,
                first_n_rank_by_row: None,
                logical_ids,
                logical_to_row,
                catalog_generation,
                generation_reader,
            })?
        } else if options.regex {
            (
                paths.len(),
                (0..paths.len()).map(|value| value as u32).collect(),
            )
        } else {
            search_candidates(CandidateSearchRequest {
                index_dir,
                backend: &options.backend,
                file_query,
                content_query,
                first_n: fast_first_n,
                first_n_conjunctive: fast_first_n_conjunctive,
                first_n_logical_order,
                first_n_rank_by_row,
                logical_ids,
                logical_to_row,
                catalog_generation,
                generation_reader,
            })?
        };
    if docs != paths.len() {
        return Err("index and GUI catalog document counts differ; rebuild index".to_owned());
    }
    if is_cancelled() {
        return Err("cancelled".to_owned());
    }

    let extension_filter = options
        .extensions
        .iter()
        .map(|value| value.trim_start_matches('.').to_ascii_lowercase())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    let has_catalog_metadata =
        catalog_sizes.len() == paths.len() && catalog_modified_ns.len() == paths.len();
    let fast_result_ordered = fast_first_n.is_some() || fast_first_n_conjunctive.is_some();
    let path_ordered = options.sort_field == "path";
    if path_ordered && !fast_result_ordered {
        ids.sort_unstable();
        if options.sort_direction == "descending" {
            ids.reverse();
        }
    }

    let mut hits = Vec::with_capacity(ids.len().min(limit));
    for doc_id in ids {
        if is_cancelled() {
            return Err("cancelled".to_owned());
        }
        let Some(relative) = paths.get(doc_id as usize) else {
            continue;
        };
        if !scope_matches(root, relative, options.path_scope.as_deref()) {
            continue;
        }
        let full = root.join(relative);
        let extension = full
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .to_owned();
        if !extension_filter.is_empty()
            && !extension_filter
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(&extension))
        {
            continue;
        }
        if let Some(query) = file_query {
            let target = if options.include_path {
                relative.as_str()
            } else {
                Path::new(relative)
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or(relative)
            };
            if !plain_or_regex_match(
                target,
                query,
                options.match_case,
                options.whole_words,
                file_regex.as_ref(),
            ) {
                continue;
            }
        }
        if let Some(query) = content_query {
            if options.regex || options.match_case || options.whole_words {
                let Some(text) = read_content(&full) else {
                    continue;
                };
                if !plain_or_regex_match(
                    &text,
                    query,
                    options.match_case,
                    options.whole_words,
                    content_regex.as_ref(),
                ) {
                    continue;
                }
            }
        }
        let metadata = (!has_catalog_metadata)
            .then(|| fs::metadata(&full).ok())
            .flatten();
        let name = full
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(relative)
            .to_owned();
        hits.push(SearchResult {
            file_id: doc_id,
            path: full.to_string_lossy().into_owned(),
            name,
            extension,
            size_bytes: if has_catalog_metadata {
                catalog_sizes[doc_id as usize]
            } else {
                metadata.as_ref().map_or(0, fs::Metadata::len)
            },
            modified_ns: if has_catalog_metadata {
                catalog_modified_ns[doc_id as usize]
            } else {
                metadata.as_ref().map_or(0, modified_ns)
            },
            content_state: if has_catalog_metadata || metadata.is_some() {
                "indexed".to_owned()
            } else {
                "missing".to_owned()
            },
        });
        if (path_ordered || fast_result_ordered) && hits.len() >= limit {
            break;
        }
    }

    if !path_ordered && !fast_result_ordered {
        let compare = |left: &SearchResult, right: &SearchResult| {
            let primary = match options.sort_field.as_str() {
                "name" => left.name.cmp(&right.name),
                "size" => left.size_bytes.cmp(&right.size_bytes),
                "modified" => left.modified_ns.cmp(&right.modified_ns),
                "extension" => left.extension.cmp(&right.extension),
                _ => left.path.cmp(&right.path),
            };
            let primary = if options.sort_direction == "descending" {
                primary.reverse()
            } else {
                primary
            };
            // Stable-sort semantics previously preserved path/document order for equal primary
            // keys. Use path as an explicit ascending tie-breaker so partial Top-K selection is
            // deterministic and byte-for-byte UI ordering remains stable across runs.
            primary.then_with(|| left.path.cmp(&right.path))
        };
        if hits.len() > limit.saturating_mul(2) && limit < hits.len() {
            let (top, _, _) = hits.select_nth_unstable_by(limit, compare);
            top.sort_unstable_by(compare);
            hits.truncate(limit);
        } else {
            hits.sort_unstable_by(compare);
            hits.truncate(limit);
        }
    }
    Ok(hits)
}

/// Execute GUI search controls while reusing scan-time metadata aligned with `paths`.
pub fn search_catalog_with_metadata<F>(
    index_dir: &Path,
    root: &Path,
    paths: &[String],
    catalog_sizes: &[u64],
    catalog_modified_ns: &[u64],
    options: &SearchOptions,
    is_cancelled: F,
) -> Result<Vec<SearchResult>, String>
where
    F: Fn() -> bool,
{
    search_catalog_impl(
        index_dir,
        GenerationSearchCatalog {
            root,
            paths,
            size_bytes: catalog_sizes,
            modified_ns: catalog_modified_ns,
            logical_ids: &[],
            logical_to_row: &[],
            generation: 0,
            first_n_logical_order: None,
            first_n_rank_by_row: None,
        },
        options,
        None,
        read_text,
        is_cancelled,
    )
}

/// Generation-aware search with application-owned structured-content verification.
pub fn search_catalog_with_generation_metadata_reader<F, R>(
    index_dir: &Path,
    catalog: GenerationSearchCatalog<'_>,
    options: &SearchOptions,
    read_content: R,
    is_cancelled: F,
) -> Result<Vec<SearchResult>, String>
where
    F: Fn() -> bool,
    R: Fn(&Path) -> Option<String>,
{
    search_catalog_impl(
        index_dir,
        catalog,
        options,
        None,
        read_content,
        is_cancelled,
    )
}

pub(crate) fn search_catalog_with_generation_session_reader<F, R>(
    index_dir: &Path,
    catalog: GenerationSearchCatalog<'_>,
    options: &SearchOptions,
    merged_session: &MergedSearchSession,
    read_content: R,
    is_cancelled: F,
) -> Result<Vec<SearchResult>, String>
where
    F: Fn() -> bool,
    R: Fn(&Path) -> Option<String>,
{
    search_catalog_impl(
        index_dir,
        catalog,
        options,
        Some(GenerationCandidateReader::Perf12(merged_session)),
        read_content,
        is_cancelled,
    )
}

pub(crate) fn search_catalog_with_vnext_generation_reader<F, R>(
    index_dir: &Path,
    catalog: GenerationSearchCatalog<'_>,
    options: &SearchOptions,
    index: &VNextGenerationIndex,
    read_content: R,
    is_cancelled: F,
) -> Result<Vec<SearchResult>, String>
where
    F: Fn() -> bool,
    R: Fn(&Path) -> Option<String>,
{
    search_catalog_impl(
        index_dir,
        catalog,
        options,
        Some(GenerationCandidateReader::VNext(index)),
        read_content,
        is_cancelled,
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnippetResult {
    pub line_number: usize,
    pub before: Vec<String>,
    pub hit_line: String,
    pub after: Vec<String>,
}

pub fn snippets(
    path: &Path,
    query: &str,
    context: usize,
    max_hits: usize,
    match_case: bool,
    whole_words: bool,
    regex_mode: bool,
) -> Result<Vec<SnippetResult>, String> {
    if query.is_empty() || max_hits == 0 {
        return Ok(Vec::new());
    }
    let text = read_text(path).ok_or_else(|| format!("failed to read {}", path.display()))?;
    snippets_from_text(
        &text,
        query,
        context,
        max_hits,
        match_case,
        whole_words,
        regex_mode,
    )
}

pub fn snippets_from_text(
    text: &str,
    query: &str,
    context: usize,
    max_hits: usize,
    match_case: bool,
    whole_words: bool,
    regex_mode: bool,
) -> Result<Vec<SnippetResult>, String> {
    if query.is_empty() || max_hits == 0 {
        return Ok(Vec::new());
    }
    let matcher = regex_mode
        .then(|| regex_for(query, match_case, whole_words))
        .transpose()?;
    let lines = text.lines().map(str::to_owned).collect::<Vec<_>>();
    let mut output = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if !plain_or_regex_match(line, query, match_case, whole_words, matcher.as_ref()) {
            continue;
        }
        let start = index.saturating_sub(context);
        let end = (index + context + 1).min(lines.len());
        output.push(SnippetResult {
            line_number: index + 1,
            before: lines[start..index].to_vec(),
            hit_line: line.clone(),
            after: lines[index + 1..end].to_vec(),
        });
        if output.len() >= max_hits {
            break;
        }
    }
    Ok(output)
}

#[cfg(test)]
mod fastpath_tests {
    use super::required_regex_literal_prefix;

    #[test]
    fn regex_prefilter_only_returns_provably_required_prefixes() {
        assert_eq!(
            required_regex_literal_prefix("error.*timeout").as_deref(),
            Some("error")
        );
        assert_eq!(
            required_regex_literal_prefix("^Report_[0-9]+\\.txt$").as_deref(),
            Some("Report_")
        );
        assert_eq!(required_regex_literal_prefix("foo|bar"), None);
        assert_eq!(required_regex_literal_prefix(".*timeout"), None);
        assert_eq!(required_regex_literal_prefix("\\d+timeout"), None);
        assert_eq!(required_regex_literal_prefix("ab?cd").as_deref(), None);
        assert_eq!(
            required_regex_literal_prefix("foobar?baz").as_deref(),
            Some("fooba")
        );
    }
}
