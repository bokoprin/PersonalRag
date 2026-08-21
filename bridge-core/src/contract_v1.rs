use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const APP_CONTRACT_VERSION: u32 = 1;
pub const APP_CONTRACT_NAME: &str = "personalrag-app-contract";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractInfo {
    pub name: String,
    pub version: u32,
    pub capabilities: Vec<String>,
}

impl Default for ContractInfo {
    fn default() -> Self {
        Self {
            name: APP_CONTRACT_NAME.to_owned(),
            version: APP_CONTRACT_VERSION,
            capabilities: vec![
                "portable-search".to_owned(),
                "portable-index".to_owned(),
                "snippet-batch".to_owned(),
                "search-core-backend-switch".to_owned(),
                "vnext-shadow-compare".to_owned(),
            ],
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExclusionConfig {
    #[serde(default)]
    pub dev_caches: bool,
    #[serde(default)]
    pub virtual_envs: bool,
    #[serde(default)]
    pub node_modules: bool,
    #[serde(default)]
    pub build_artifacts: bool,
    #[serde(default)]
    pub vcs: bool,
    #[serde(default)]
    pub use_gitignore: bool,
    #[serde(default)]
    pub custom_directory_names: Vec<String>,
    #[serde(default)]
    pub custom_relative_paths: Vec<String>,
    #[serde(default)]
    pub custom_globs: Vec<String>,
}

fn default_search_backend() -> String {
    "v2".to_owned()
}

fn default_search_core_backend() -> String {
    "perf12".to_owned()
}

fn default_scanner_mode() -> String {
    "auto".to_owned()
}

fn default_max_bytes() -> u64 {
    32 * 1024 * 1024
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default)]
    pub roots: Vec<PathBuf>,
    #[serde(default = "default_max_bytes")]
    pub max_bytes: u64,
    #[serde(default)]
    pub background_enabled: bool,
    #[serde(default = "default_search_backend")]
    pub search_backend: String,
    #[serde(default = "default_search_core_backend")]
    pub search_core_backend: String,
    #[serde(default = "default_scanner_mode")]
    pub scanner_mode: String,
    #[serde(default)]
    pub exclusions: ExclusionConfig,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            max_bytes: default_max_bytes(),
            background_enabled: false,
            search_backend: default_search_backend(),
            search_core_backend: default_search_core_backend(),
            scanner_mode: default_scanner_mode(),
            exclusions: ExclusionConfig::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RebuildProgress {
    pub phase: String,
    pub total_files: usize,
    pub processed_files: usize,
    pub indexed_files: usize,
    pub unchanged_files: usize,
    pub skipped_files: usize,
    pub error_files: usize,
    pub bytes_read: u64,
    pub current_path: Option<String>,
    pub elapsed_ms: f64,
    pub discovered_files: usize,
    pub pruned_files: usize,
    pub files_per_second: f64,
    pub mib_per_second: f64,
    pub queue_files: usize,
    pub prepared_bytes: u64,
    pub remaining_files: Option<usize>,
    pub eta_ms: Option<f64>,
    pub estimated_completion_at_ms: Option<f64>,
}

impl Default for RebuildProgress {
    fn default() -> Self {
        Self {
            phase: "scanning".to_owned(),
            total_files: 0,
            processed_files: 0,
            indexed_files: 0,
            unchanged_files: 0,
            skipped_files: 0,
            error_files: 0,
            bytes_read: 0,
            current_path: None,
            elapsed_ms: 0.0,
            discovered_files: 0,
            pruned_files: 0,
            files_per_second: 0.0,
            mib_per_second: 0.0,
            queue_files: 0,
            prepared_bytes: 0,
            remaining_files: None,
            eta_ms: None,
            estimated_completion_at_ms: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RebuildStatus {
    pub job_id: String,
    pub state: String,
    pub progress: RebuildProgress,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackgroundStatus {
    pub running: bool,
    pub mode: String,
    pub sync_state: String,
    pub pending_changes: usize,
    pub last_sync_at: Option<String>,
    pub last_error: Option<String>,
    pub rebuild: Option<RebuildStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IndexRequest {
    pub roots: Vec<PathBuf>,
    pub max_bytes: Option<u64>,
    #[serde(default)]
    pub scanner_mode: Option<String>,
    #[serde(default)]
    pub exclusions: Option<ExclusionConfig>,
}

pub type BackgroundRequest = IndexRequest;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexResponse {
    pub accepted: bool,
    pub job_id: Option<String>,
    pub state: String,
    pub message: String,
    pub status: BackgroundStatus,
    pub settings: Settings,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchRequest {
    pub file_query: Option<String>,
    pub include_path: bool,
    pub content_query: Option<String>,
    #[serde(default)]
    pub extensions: Vec<String>,
    pub path_scope: Option<String>,
    pub match_case: bool,
    pub whole_words: bool,
    pub regex: bool,
    pub sort_field: String,
    pub sort_direction: String,
    pub limit: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchHit {
    pub file_id: u32,
    pub path: String,
    pub name: String,
    pub extension: String,
    pub size_bytes: u64,
    pub modified_ns: u64,
    pub content_state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SnippetRequest {
    pub path: PathBuf,
    pub query: String,
    pub context: usize,
    pub max_hits: usize,
    pub match_case: bool,
    pub whole_words: bool,
    pub regex: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnippetHit {
    pub line_number: usize,
    pub before: Vec<String>,
    pub hit_line: String,
    pub after: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SnippetBatchRequest {
    pub items: Vec<SnippetRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnippetBatchResult {
    pub path: PathBuf,
    pub hits: Vec<SnippetHit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackendReadiness {
    pub search_v2_ready: bool,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchBackendStatus {
    pub requested: String,
    pub active: String,
    pub readiness: BackendReadiness,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchCoreBackendStatus {
    pub requested: String,
    pub active: String,
    pub vnext_ready: bool,
    pub generation: u64,
    pub searches: u64,
    pub fallbacks: u64,
    pub shadow_comparisons: u64,
    pub shadow_mismatches: u64,
    pub shadow_queued: u64,
    pub shadow_coalesced: u64,
    pub shadow_dropped: u64,
    pub shadow_failures: u64,
    pub common_result_searches: u64,
    pub common_result_total_micros: u64,
    pub common_result_max_micros: u64,
    pub last_search_micros: u64,
}
