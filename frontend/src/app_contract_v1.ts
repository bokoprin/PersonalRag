export const APP_CONTRACT_VERSION = 1 as const;
export const APP_CONTRACT_NAME = "personalrag-app-contract" as const;

export type ContractInfo = {
  name: string;
  version: number;
  capabilities: string[];
};

export type SearchHit = {
  file_id: number;
  path: string;
  name: string;
  extension: string;
  size_bytes: number;
  modified_ns: number;
  content_state: string;
};

export type SnippetHit = {
  line_number: number;
  before: string[];
  hit_line: string;
  after: string[];
};

export type SnippetRequest = {
  path: string;
  query: string;
  context: number;
  maxHits: number;
  matchCase: boolean;
  wholeWords: boolean;
  regex: boolean;
};

export type SnippetBatchRequest = {
  items: SnippetRequest[];
};

export type SnippetBatchResult = {
  path: string;
  hits: SnippetHit[];
};

export type ScannerMode = "auto" | "walk_dir" | "windows_native" | "ntfs_mft_benchmark";

export type ExclusionConfig = {
  dev_caches: boolean;
  virtual_envs: boolean;
  node_modules: boolean;
  build_artifacts: boolean;
  vcs: boolean;
  use_gitignore: boolean;
  custom_directory_names: string[];
  custom_relative_paths: string[];
  custom_globs: string[];
};

export type Settings = {
  roots: string[];
  max_bytes: number;
  background_enabled?: boolean;
  search_backend?: "v1" | "v2";
  search_core_backend?: "perf12" | "shadow" | "vnext";
  scanner_mode?: ScannerMode;
  exclusions?: ExclusionConfig;
};


export type SearchCoreBackendStatus = {
  requested: "perf12" | "shadow" | "vnext";
  active: "perf12" | "vnext";
  vnext_ready: boolean;
  generation: number;
  searches: number;
  fallbacks: number;
  shadow_comparisons: number;
  shadow_mismatches: number;
  shadow_queued: number;
  shadow_coalesced: number;
  shadow_dropped: number;
  shadow_failures: number;
  common_result_searches: number;
  common_result_total_micros: number;
  common_result_max_micros: number;
  last_search_micros: number;
};

export type RebuildState =
  | "idle"
  | "starting"
  | "scanning"
  | "reconciling"
  | "catching_up"
  | "cancelling"
  | "completed"
  | "cancelled"
  | "failed";

export type RebuildProgress = {
  phase?: "scanning" | "preparing" | "building" | "q2" | "pos1" | "pos23" | "verifying" | "publishing" | "completed";
  total_files?: number;
  processed_files: number;
  indexed_files: number;
  unchanged_files: number;
  skipped_files: number;
  error_files: number;
  bytes_read: number;
  current_path?: string | null;
  elapsed_ms: number;
  discovered_files?: number;
  pruned_files?: number;
  files_per_second?: number;
  mib_per_second?: number;
  queue_files?: number;
  prepared_bytes?: number;
  remaining_files?: number | null;
  eta_ms?: number | null;
  estimated_completion_at_ms?: number | null;
};

export type RebuildStatus = {
  job_id: string;
  state: RebuildState;
  progress: RebuildProgress;
  started_at?: string | null;
  finished_at?: string | null;
  error?: string | null;
};

export type BackgroundSyncState =
  | "starting"
  | "catching_up"
  | "up_to_date"
  | "rebuilding"
  | "degraded"
  | "error"
  | "stopped";

export type BackgroundStatus = {
  running: boolean;
  mode: string;
  sync_state: BackgroundSyncState;
  pending_changes: number;
  last_sync_at?: string | null;
  last_error?: string | null;
  rebuild?: RebuildStatus | null;
};

export type IndexRequest = {
  roots: string[];
  maxBytes: number | null;
  scannerMode?: string | null;
  exclusions?: ExclusionConfig | null;
};

export type IndexResponse = {
  accepted: boolean;
  job_id?: string | null;
  state: RebuildState;
  message: string;
  status: BackgroundStatus;
  settings: Settings;
};

export type SearchRequest = {
  file_query: string | null;
  include_path: boolean;
  content_query: string | null;
  extensions: string[];
  path_scope: string | null;
  match_case: boolean;
  whole_words: boolean;
  regex: boolean;
  sort_field: string;
  sort_direction: string;
  limit: number;
};

export type BackendReadiness = {
  search_v2_ready: boolean;
  state: string;
};

export type SearchBackendStatus = {
  requested: string;
  active: string;
  readiness: BackendReadiness;
};

export function assertCompatibleContract(info: ContractInfo): void {
  if (info.name !== APP_CONTRACT_NAME || info.version !== APP_CONTRACT_VERSION) {
    throw new Error(
      `App Contract mismatch: expected ${APP_CONTRACT_NAME} v${APP_CONTRACT_VERSION}, got ${info.name} v${info.version}`,
    );
  }
}
