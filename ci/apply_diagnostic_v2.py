from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    file = Path(path)
    text = file.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, got {count}: {old[:80]!r}")
    file.write_text(text.replace(old, new, 1), encoding="utf-8")


# search-core: bundle segment build options so the permanent timing hook does not violate clippy's
# too_many_arguments gate.
replace_once(
    "search-core/src/builder.rs",
    """fn build_owned_segment_profile(\n    task: SegmentBuildTask,\n    output_dir: &Path,\n    mode: BuildMode,\n    acceleration: AccelerationProfile,\n    build_workers: usize,\n    durable: bool,\n    retain_documents: bool,\n    timings: Option<&BuildTimingAccumulator>,\n) -> Result<(ManifestEntry, Option<Vec<DocumentInput>>)> {\n    let profile_build = profile_build_enabled();\n""",
    """struct OwnedSegmentBuildConfig<'a> {\n    output_dir: &'a Path,\n    mode: BuildMode,\n    acceleration: AccelerationProfile,\n    build_workers: usize,\n    durable: bool,\n    retain_documents: bool,\n    timings: Option<&'a BuildTimingAccumulator>,\n}\n\nfn build_owned_segment_profile(\n    task: SegmentBuildTask,\n    config: OwnedSegmentBuildConfig<'_>,\n) -> Result<(ManifestEntry, Option<Vec<DocumentInput>>)> {\n    let OwnedSegmentBuildConfig {\n        output_dir,\n        mode,\n        acceleration,\n        build_workers,\n        durable,\n        retain_documents,\n        timings,\n    } = config;\n    let profile_build = profile_build_enabled();\n""",
)
replace_once(
    "search-core/src/builder.rs",
    """                        let result = build_owned_segment_profile(\n                            task,\n                            output_dir,\n                            options.mode,\n                            acceleration,\n                            build_workers,\n                            durable,\n                            retain_documents,\n                            Some(timings.as_ref()),\n                        );\n""",
    """                        let result = build_owned_segment_profile(\n                            task,\n                            OwnedSegmentBuildConfig {\n                                output_dir,\n                                mode: options.mode,\n                                acceleration,\n                                build_workers,\n                                durable,\n                                retain_documents,\n                                timings: Some(timings.as_ref()),\n                            },\n                        );\n""",
)
replace_once(
    "search-core/src/builder.rs",
    """        let (entry, _) = build_owned_segment_profile(\n            task,\n            &self.output_dir,\n            self.options.mode,\n            self.acceleration,\n            1,\n            self.durable,\n            false,\n            None,\n        )?;\n""",
    """        let (entry, _) = build_owned_segment_profile(\n            task,\n            OwnedSegmentBuildConfig {\n                output_dir: &self.output_dir,\n                mode: self.options.mode,\n                acceleration: self.acceleration,\n                build_workers: 1,\n                durable: self.durable,\n                retain_documents: false,\n                timings: None,\n            },\n        )?;\n""",
)

# bridge fallback scanner: count the type of every discovered entry so discovered -> source can be
# explained without conflating directories with files.
replace_once(
    "bridge-core/src/lib.rs",
    """pub struct ScanProgress {\n    pub discovered_entries: usize,\n    pub selected_files: usize,\n    pub pruned_entries: usize,\n    pub error_entries: usize,\n    pub selected_bytes: u64,\n    pub current_path: Option<PathBuf>,\n}\n""",
    """pub struct ScanProgress {\n    pub discovered_entries: usize,\n    pub file_entries: usize,\n    pub directory_entries: usize,\n    pub other_entries: usize,\n    pub selected_files: usize,\n    pub pruned_entries: usize,\n    pub error_entries: usize,\n    pub selected_bytes: u64,\n    pub current_path: Option<PathBuf>,\n}\n\nimpl ScanProgress {\n    #[must_use]\n    pub fn unselected_file_entries(&self) -> usize {\n        self.file_entries.saturating_sub(self.selected_files)\n    }\n}\n""",
)
replace_once(
    "bridge-core/src/lib.rs",
    """    let discovered = Arc::new(AtomicUsize::new(0));\n    let selected = Arc::new(AtomicUsize::new(0));\n    let pruned = Arc::new(AtomicUsize::new(0));\n""",
    """    let discovered = Arc::new(AtomicUsize::new(0));\n    let file_entries = Arc::new(AtomicUsize::new(0));\n    let directory_entries = Arc::new(AtomicUsize::new(0));\n    let other_entries = Arc::new(AtomicUsize::new(0));\n    let selected = Arc::new(AtomicUsize::new(0));\n    let pruned = Arc::new(AtomicUsize::new(0));\n""",
)
replace_once(
    "bridge-core/src/lib.rs",
    """    let progress_snapshot = || ScanProgress {\n        discovered_entries: discovered.load(AtomicOrdering::Relaxed),\n        selected_files: selected.load(AtomicOrdering::Relaxed),\n""",
    """    let progress_snapshot = || ScanProgress {\n        discovered_entries: discovered.load(AtomicOrdering::Relaxed),\n        file_entries: file_entries.load(AtomicOrdering::Relaxed),\n        directory_entries: directory_entries.load(AtomicOrdering::Relaxed),\n        other_entries: other_entries.load(AtomicOrdering::Relaxed),\n        selected_files: selected.load(AtomicOrdering::Relaxed),\n""",
)
replace_once(
    "bridge-core/src/lib.rs",
    """            let count = discovered.fetch_add(1, AtomicOrdering::Relaxed) + 1;\n            if count.is_multiple_of(256) {\n""",
    """            let count = discovered.fetch_add(1, AtomicOrdering::Relaxed) + 1;\n            if entry.file_type().is_some_and(|kind| kind.is_file()) {\n                file_entries.fetch_add(1, AtomicOrdering::Relaxed);\n            } else if entry.file_type().is_some_and(|kind| kind.is_dir()) {\n                directory_entries.fetch_add(1, AtomicOrdering::Relaxed);\n            } else {\n                other_entries.fetch_add(1, AtomicOrdering::Relaxed);\n            }\n            if count.is_multiple_of(256) {\n""",
)
replace_once(
    "bridge-core/src/lib.rs",
    """        let files_for_walk = Arc::clone(&files);\n        let discovered_for_walk = Arc::clone(&discovered);\n        let selected_for_walk = Arc::clone(&selected);\n""",
    """        let files_for_walk = Arc::clone(&files);\n        let discovered_for_walk = Arc::clone(&discovered);\n        let file_entries_for_walk = Arc::clone(&file_entries);\n        let directory_entries_for_walk = Arc::clone(&directory_entries);\n        let other_entries_for_walk = Arc::clone(&other_entries);\n        let selected_for_walk = Arc::clone(&selected);\n""",
)
replace_once(
    "bridge-core/src/lib.rs",
    """            let files = Arc::clone(&files_for_walk);\n            let discovered = Arc::clone(&discovered_for_walk);\n            let selected = Arc::clone(&selected_for_walk);\n""",
    """            let files = Arc::clone(&files_for_walk);\n            let discovered = Arc::clone(&discovered_for_walk);\n            let file_entries = Arc::clone(&file_entries_for_walk);\n            let directory_entries = Arc::clone(&directory_entries_for_walk);\n            let other_entries = Arc::clone(&other_entries_for_walk);\n            let selected = Arc::clone(&selected_for_walk);\n""",
)
replace_once(
    "bridge-core/src/lib.rs",
    """                let count = discovered.fetch_add(1, AtomicOrdering::Relaxed) + 1;\n                let report_path = count\n""",
    """                let count = discovered.fetch_add(1, AtomicOrdering::Relaxed) + 1;\n                if entry.file_type().is_some_and(|kind| kind.is_file()) {\n                    file_entries.fetch_add(1, AtomicOrdering::Relaxed);\n                } else if entry.file_type().is_some_and(|kind| kind.is_dir()) {\n                    directory_entries.fetch_add(1, AtomicOrdering::Relaxed);\n                } else {\n                    other_entries.fetch_add(1, AtomicOrdering::Relaxed);\n                }\n                let report_path = count\n""",
)
replace_once(
    "bridge-core/src/lib.rs",
    """                    on_progress(ScanProgress {\n                        discovered_entries: count,\n                        selected_files: selected.load(AtomicOrdering::Relaxed),\n""",
    """                    on_progress(ScanProgress {\n                        discovered_entries: count,\n                        file_entries: file_entries.load(AtomicOrdering::Relaxed),\n                        directory_entries: directory_entries.load(AtomicOrdering::Relaxed),\n                        other_entries: other_entries.load(AtomicOrdering::Relaxed),\n                        selected_files: selected.load(AtomicOrdering::Relaxed),\n""",
)
replace_once(
    "bridge-core/src/lib.rs",
    """}\n\n#[derive(Debug, Clone)]\npub struct SearchOptions {\n""",
    """}\n\n#[cfg(test)]\nmod scan_breakdown_tests {\n    use std::{\n        fs,\n        sync::{atomic::AtomicBool, Arc},\n        time::{SystemTime, UNIX_EPOCH},\n    };\n\n    use super::*;\n\n    #[test]\n    fn walkdir_scan_reports_discovered_entry_types() {\n        let unique = SystemTime::now()\n            .duration_since(UNIX_EPOCH)\n            .unwrap()\n            .as_nanos();\n        let root = std::env::temp_dir().join(format!(\"personalrag-scan-breakdown-{unique}\"));\n        let child = root.join(\"child\");\n        fs::create_dir_all(&child).unwrap();\n        fs::write(root.join(\"a.txt\"), b\"a\").unwrap();\n        fs::write(child.join(\"b.txt\"), b\"b\").unwrap();\n\n        let report = scan_files(\n            &root,\n            0,\n            ScannerMode::WalkDir,\n            &ScanExclusions::default(),\n            Arc::new(AtomicBool::new(false)),\n            Arc::new(|_| {}),\n        )\n        .unwrap();\n\n        assert_eq!(report.progress.discovered_entries, 4);\n        assert_eq!(report.progress.file_entries, 2);\n        assert_eq!(report.progress.directory_entries, 2);\n        assert_eq!(report.progress.other_entries, 0);\n        assert_eq!(report.progress.selected_files, 2);\n        assert_eq!(report.progress.unselected_file_entries(), 0);\n        assert_eq!(report.files.len(), 2);\n        fs::remove_dir_all(root).unwrap();\n    }\n}\n\n#[derive(Debug, Clone)]\npub struct SearchOptions {\n""",
)

# Windows native scanner: classify each discovered directory record. Root is a discovered
# directory so initialize both counters to one.
replace_once(
    "bridge-core/src/windows_native_scanner.rs",
    """    struct LocalProgressCounters {\n        discovered: usize,\n        selected: usize,\n        pruned: usize,\n        selected_bytes: u64,\n    }\n""",
    """    struct LocalProgressCounters {\n        discovered: usize,\n        file_entries: usize,\n        directory_entries: usize,\n        selected: usize,\n        pruned: usize,\n        selected_bytes: u64,\n    }\n""",
)
replace_once(
    "bridge-core/src/windows_native_scanner.rs",
    """        fn record_selected(&mut self, bytes: u64) {\n            self.selected += 1;\n            self.selected_bytes = self.selected_bytes.saturating_add(bytes);\n        }\n\n        fn record_pruned(&mut self) {\n""",
    """        fn record_file_entry(&mut self) {\n            self.file_entries += 1;\n        }\n\n        fn record_directory_entry(&mut self) {\n            self.directory_entries += 1;\n        }\n\n        fn record_selected(&mut self, bytes: u64) {\n            self.selected += 1;\n            self.selected_bytes = self.selected_bytes.saturating_add(bytes);\n        }\n\n        fn record_pruned(&mut self) {\n""",
)
replace_once(
    "bridge-core/src/windows_native_scanner.rs",
    """            if self.selected != 0 {\n                shared.selected.fetch_add(self.selected, Ordering::Relaxed);\n                self.selected = 0;\n            }\n""",
    """            if self.file_entries != 0 {\n                shared\n                    .file_entries\n                    .fetch_add(self.file_entries, Ordering::Relaxed);\n                self.file_entries = 0;\n            }\n            if self.directory_entries != 0 {\n                shared\n                    .directory_entries\n                    .fetch_add(self.directory_entries, Ordering::Relaxed);\n                self.directory_entries = 0;\n            }\n            if self.selected != 0 {\n                shared.selected.fetch_add(self.selected, Ordering::Relaxed);\n                self.selected = 0;\n            }\n""",
)
replace_once(
    "bridge-core/src/windows_native_scanner.rs",
    """        discovered: Arc<AtomicUsize>,\n        selected: Arc<AtomicUsize>,\n        pruned: Arc<AtomicUsize>,\n""",
    """        discovered: Arc<AtomicUsize>,\n        file_entries: Arc<AtomicUsize>,\n        directory_entries: Arc<AtomicUsize>,\n        other_entries: Arc<AtomicUsize>,\n        selected: Arc<AtomicUsize>,\n        pruned: Arc<AtomicUsize>,\n""",
)
replace_once(
    "bridge-core/src/windows_native_scanner.rs",
    """        ScanProgress {\n            discovered_entries: shared.discovered.load(Ordering::Relaxed),\n            selected_files: shared.selected.load(Ordering::Relaxed),\n""",
    """        ScanProgress {\n            discovered_entries: shared.discovered.load(Ordering::Relaxed),\n            file_entries: shared.file_entries.load(Ordering::Relaxed),\n            directory_entries: shared.directory_entries.load(Ordering::Relaxed),\n            other_entries: shared.other_entries.load(Ordering::Relaxed),\n            selected_files: shared.selected.load(Ordering::Relaxed),\n""",
)
replace_once(
    "bridge-core/src/windows_native_scanner.rs",
    """                if !is_directory && max_file_bytes != 0 && record.end_of_file > max_file_bytes {\n                    scratch.progress.record_discovered();\n                    scratch.progress.record_pruned();\n""",
    """                if !is_directory && max_file_bytes != 0 && record.end_of_file > max_file_bytes {\n                    scratch.progress.record_discovered();\n                    scratch.progress.record_file_entry();\n                    scratch.progress.record_pruned();\n""",
)
replace_once(
    "bridge-core/src/windows_native_scanner.rs",
    """                let display_name = name.to_string_lossy();\n                scratch.progress.record_discovered();\n\n                if is_directory {\n""",
    """                let display_name = name.to_string_lossy();\n                scratch.progress.record_discovered();\n                if is_directory {\n                    scratch.progress.record_directory_entry();\n                } else {\n                    scratch.progress.record_file_entry();\n                }\n\n                if is_directory {\n""",
)
replace_once(
    "bridge-core/src/windows_native_scanner.rs",
    """            discovered: Arc::new(AtomicUsize::new(1)), // Match WalkBuilder's root entry.\n            selected: Arc::new(AtomicUsize::new(0)),\n""",
    """            discovered: Arc::new(AtomicUsize::new(1)), // Match WalkBuilder's root entry.\n            file_entries: Arc::new(AtomicUsize::new(0)),\n            directory_entries: Arc::new(AtomicUsize::new(1)),\n            other_entries: Arc::new(AtomicUsize::new(0)),\n            selected: Arc::new(AtomicUsize::new(0)),\n""",
)

# Diagnostic JSON v2: preserve sourceFiles as the selected/index-source count and make the scan
# entry taxonomy explicit.
replace_once(
    "bridge-core/src/build_diagnostics.rs",
    "pub const INDEX_BUILD_DIAGNOSTIC_SCHEMA_VERSION: u32 = 1;",
    "pub const INDEX_BUILD_DIAGNOSTIC_SCHEMA_VERSION: u32 = 2;",
)
replace_once(
    "bridge-core/src/build_diagnostics.rs",
    """    pub total_ms: f64,\n    pub discovered_files: usize,\n    pub source_files: usize,\n""",
    """    pub total_ms: f64,\n    pub discovered_entries: usize,\n    pub discovered_file_entries: usize,\n    pub discovered_directory_entries: usize,\n    pub discovered_other_entries: usize,\n    pub unselected_file_entries: usize,\n    pub source_files: usize,\n""",
)
replace_once(
    "bridge-core/src/build_diagnostics.rs",
    """            total_ms: 0.0,\n            discovered_files: 0,\n            source_files: 0,\n""",
    """            total_ms: 0.0,\n            discovered_entries: 0,\n            discovered_file_entries: 0,\n            discovered_directory_entries: 0,\n            discovered_other_entries: 0,\n            unselected_file_entries: 0,\n            source_files: 0,\n""",
)
replace_once(
    "bridge-core/src/build_diagnostics.rs",
    """        log.mode = \"full_rebuild\".to_owned();\n        log.source_files = 10;\n        log.indexed_files = 9;\n""",
    """        log.mode = \"full_rebuild\".to_owned();\n        log.discovered_entries = 14;\n        log.discovered_file_entries = 10;\n        log.discovered_directory_entries = 3;\n        log.discovered_other_entries = 1;\n        log.unselected_file_entries = 0;\n        log.source_files = 10;\n        log.indexed_files = 9;\n""",
)
replace_once(
    "bridge-core/src/build_diagnostics.rs",
    """        assert_eq!(persisted, log);\n        let latest: IndexBuildDiagnosticLog =\n""",
    """        assert_eq!(persisted, log);\n        let json: serde_json::Value =\n            serde_json::from_slice(&fs::read(&history).unwrap()).unwrap();\n        assert_eq!(json[\"schemaVersion\"], 2);\n        assert_eq!(json[\"discoveredEntries\"], 14);\n        assert_eq!(json[\"discoveredFileEntries\"], 10);\n        assert_eq!(json[\"discoveredDirectoryEntries\"], 3);\n        assert_eq!(json[\"discoveredOtherEntries\"], 1);\n        assert_eq!(json[\"unselectedFileEntries\"], 0);\n        assert!(json.get(\"discoveredFiles\").is_none());\n        let latest: IndexBuildDiagnosticLog =\n""",
)

replace_once(
    "src-tauri/src/main.rs",
    """            diagnostics.discovered_files = scan.progress.discovered_entries;\n            diagnostics.source_files = scan.files.len();\n            diagnostics.pruned_files = scan.progress.pruned_entries;\n""",
    """            diagnostics.discovered_entries = scan.progress.discovered_entries;\n            diagnostics.discovered_file_entries = scan.progress.file_entries;\n            diagnostics.discovered_directory_entries = scan.progress.directory_entries;\n            diagnostics.discovered_other_entries = scan.progress.other_entries;\n            diagnostics.unselected_file_entries = scan.progress.unselected_file_entries();\n            diagnostics.source_files = scan.files.len();\n            diagnostics.pruned_files = scan.progress.pruned_entries;\n""",
)

print("diagnostic v2 source transformation complete")
