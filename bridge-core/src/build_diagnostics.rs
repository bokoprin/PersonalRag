use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

pub const INDEX_BUILD_DIAGNOSTIC_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IndexBuildStageTiming {
    pub name: String,
    pub elapsed_ms: f64,
}

impl IndexBuildStageTiming {
    #[must_use]
    pub fn new(name: impl Into<String>, elapsed_ms: f64) -> Self {
        Self {
            name: name.into(),
            elapsed_ms,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IndexBuildDiagnosticLog {
    pub schema_version: u32,
    pub job_id: String,
    pub status: String,
    pub mode: String,
    pub root: String,
    pub force_full: bool,
    pub scanner_mode: String,
    pub search_core_backend: String,
    pub max_file_bytes: u64,
    pub started_at_unix_ms: u64,
    pub finished_at_unix_ms: Option<u64>,
    pub total_ms: f64,
    pub discovered_entries: usize,
    pub discovered_file_entries: usize,
    pub discovered_directory_entries: usize,
    pub discovered_other_entries: usize,
    pub unselected_file_entries: usize,
    pub source_files: usize,
    pub processed_files: usize,
    pub indexed_files: usize,
    pub skipped_files: usize,
    pub pruned_files: usize,
    pub error_files: usize,
    pub bytes_read: u64,
    pub stages: Vec<IndexBuildStageTiming>,
    pub error: Option<String>,
}

impl IndexBuildDiagnosticLog {
    #[must_use]
    pub fn new(
        job_id: impl Into<String>,
        root: &Path,
        force_full: bool,
        scanner_mode: impl Into<String>,
        search_core_backend: impl Into<String>,
        max_file_bytes: u64,
        started_at_unix_ms: u64,
    ) -> Self {
        Self {
            schema_version: INDEX_BUILD_DIAGNOSTIC_SCHEMA_VERSION,
            job_id: job_id.into(),
            status: "running".to_owned(),
            mode: "undetermined".to_owned(),
            root: root.to_string_lossy().into_owned(),
            force_full,
            scanner_mode: scanner_mode.into(),
            search_core_backend: search_core_backend.into(),
            max_file_bytes,
            started_at_unix_ms,
            finished_at_unix_ms: None,
            total_ms: 0.0,
            discovered_entries: 0,
            discovered_file_entries: 0,
            discovered_directory_entries: 0,
            discovered_other_entries: 0,
            unselected_file_entries: 0,
            source_files: 0,
            processed_files: 0,
            indexed_files: 0,
            skipped_files: 0,
            pruned_files: 0,
            error_files: 0,
            bytes_read: 0,
            stages: Vec::new(),
            error: None,
        }
    }

    pub fn record_stage(&mut self, name: impl Into<String>, elapsed_ms: f64) {
        self.stages
            .push(IndexBuildStageTiming::new(name, elapsed_ms));
    }

    pub fn extend_stages(&mut self, stages: impl IntoIterator<Item = IndexBuildStageTiming>) {
        self.stages.extend(stages);
    }

    pub fn finish(
        &mut self,
        status: impl Into<String>,
        finished_at_unix_ms: u64,
        total_ms: f64,
        error: Option<String>,
    ) {
        self.status = status.into();
        self.finished_at_unix_ms = Some(finished_at_unix_ms);
        self.total_ms = total_ms;
        self.error = error;
    }

    /// Persist this run as an immutable history entry and refresh `index-build-latest.json`.
    ///
    /// The diagnostic directory is deliberately separate from the index generation so publishing
    /// or rolling back an index never deletes prior performance evidence.
    pub fn write_json(&self, diagnostics_dir: &Path) -> Result<PathBuf, String> {
        fs::create_dir_all(diagnostics_dir).map_err(|error| error.to_string())?;
        let bytes = serde_json::to_vec_pretty(self).map_err(|error| error.to_string())?;
        let history_path = diagnostics_dir.join(format!("index-build-{}.json", self.job_id));
        let latest_path = diagnostics_dir.join("index-build-latest.json");
        fs::write(&history_path, &bytes).map_err(|error| error.to_string())?;
        fs::write(latest_path, bytes).map_err(|error| error.to_string())?;
        Ok(history_path)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    #[test]
    fn diagnostic_log_persists_history_and_latest_json() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("personalrag-build-diagnostic-{unique}"));
        let diagnostics = root.join("diagnostics");
        let mut log = IndexBuildDiagnosticLog::new(
            "portable-123",
            Path::new("C:/sample"),
            true,
            "auto",
            "perf12",
            32 * 1024 * 1024,
            100,
        );
        log.mode = "full_rebuild".to_owned();
        log.discovered_entries = 14;
        log.discovered_file_entries = 10;
        log.discovered_directory_entries = 3;
        log.discovered_other_entries = 1;
        log.unselected_file_entries = 0;
        log.source_files = 10;
        log.indexed_files = 9;
        log.error_files = 1;
        log.record_stage("scan", 12.5);
        log.record_stage("build.base_index", 30.25);
        log.finish("completed", 200, 100.0, None);

        let history = log.write_json(&diagnostics).unwrap();
        assert_eq!(history, diagnostics.join("index-build-portable-123.json"));
        assert!(diagnostics.join("index-build-latest.json").is_file());

        let persisted: IndexBuildDiagnosticLog =
            serde_json::from_slice(&fs::read(&history).unwrap()).unwrap();
        assert_eq!(persisted, log);
        let json: serde_json::Value = serde_json::from_slice(&fs::read(&history).unwrap()).unwrap();
        assert_eq!(json["schemaVersion"], 2);
        assert_eq!(json["discoveredEntries"], 14);
        assert_eq!(json["discoveredFileEntries"], 10);
        assert_eq!(json["discoveredDirectoryEntries"], 3);
        assert_eq!(json["discoveredOtherEntries"], 1);
        assert_eq!(json["unselectedFileEntries"], 0);
        assert!(json.get("discoveredFiles").is_none());
        let latest: IndexBuildDiagnosticLog =
            serde_json::from_slice(&fs::read(diagnostics.join("index-build-latest.json")).unwrap())
                .unwrap();
        assert_eq!(latest, log);
        fs::remove_dir_all(root).unwrap();
    }
}
