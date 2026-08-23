from pathlib import Path

path = Path("search-core/src/builder.rs")
text = path.read_text(encoding="utf-8")

replacements = [
    (
        "use std::sync::{Arc, Mutex, OnceLock, mpsc};",
        "use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};",
    ),
    (
        "const SEGMENT_WRITE_BUFFER_BYTES: usize = 1024 * 1024;",
        """const SEGMENT_WRITE_BUFFER_BYTES: usize = 1024 * 1024;\nconst DEFAULT_SEGMENT_SYNC_CONCURRENCY: usize = 2;\nconst PROFILE_SEGMENT_SYNC_CONCURRENCY_ENV: &str = \"PR_PROFILE_SEGMENT_SYNC_CONCURRENCY\";\n\nstruct SegmentSyncLimiter {\n    active: Mutex<usize>,\n    ready: Condvar,\n    limit: usize,\n}\n\nstruct SegmentSyncPermit<'a> {\n    limiter: &'a SegmentSyncLimiter,\n}\n\nimpl SegmentSyncLimiter {\n    fn new(limit: usize) -> Self {\n        Self {\n            active: Mutex::new(0),\n            ready: Condvar::new(),\n            limit: limit.max(1),\n        }\n    }\n\n    fn acquire(&self) -> SegmentSyncPermit<'_> {\n        let mut active = self.active.lock().unwrap_or_else(|poisoned| poisoned.into_inner());\n        while *active >= self.limit {\n            active = self\n                .ready\n                .wait(active)\n                .unwrap_or_else(|poisoned| poisoned.into_inner());\n        }\n        *active += 1;\n        SegmentSyncPermit { limiter: self }\n    }\n}\n\nimpl Drop for SegmentSyncPermit<'_> {\n    fn drop(&mut self) {\n        let mut active = self\n            .limiter\n            .active\n            .lock()\n            .unwrap_or_else(|poisoned| poisoned.into_inner());\n        *active = active.saturating_sub(1);\n        self.limiter.ready.notify_one();\n    }\n}\n\nfn segment_sync_concurrency_for(build_workers: usize, override_value: Option<usize>) -> usize {\n    let build_workers = build_workers.max(1);\n    override_value\n        .unwrap_or(DEFAULT_SEGMENT_SYNC_CONCURRENCY)\n        .clamp(1, build_workers)\n}\n\nfn segment_sync_concurrency(build_workers: usize) -> usize {\n    let override_value = profile_build_enabled()\n        .then(|| {\n            std::env::var(PROFILE_SEGMENT_SYNC_CONCURRENCY_ENV)\n                .ok()\n                .and_then(|value| value.parse::<usize>().ok())\n        })\n        .flatten();\n    segment_sync_concurrency_for(build_workers, override_value)\n}\n""",
    ),
    (
        """    retain_documents: bool,\n    timings: Option<&'a BuildTimingAccumulator>,\n}""",
        """    retain_documents: bool,\n    timings: Option<&'a BuildTimingAccumulator>,\n    sync_limiter: Option<&'a SegmentSyncLimiter>,\n}""",
    ),
    (
        """        retain_documents,\n        timings,\n    } = config;""",
        """        retain_documents,\n        timings,\n        sync_limiter,\n    } = config;""",
    ),
    (
        "let written = write_segment(&path, &data, durable)?;",
        "let written = write_segment(&path, &data, durable, sync_limiter)?;",
    ),
    (
        """    let build_workers = options.workers.max(1);\n    let batch_paths = options.segment_docs.saturating_mul(2).max(1_024);""",
        """    let build_workers = options.workers.max(1);\n    let segment_sync_limiter =\n        durable.then(|| SegmentSyncLimiter::new(segment_sync_concurrency(build_workers)));\n    let batch_paths = options.segment_docs.saturating_mul(2).max(1_024);""",
    ),
    (
        """                let timings = Arc::clone(&timings);\n                scope.spawn(move || {""",
        """                let timings = Arc::clone(&timings);\n                let sync_limiter = segment_sync_limiter.as_ref();\n                scope.spawn(move || {""",
    ),
    (
        """                                retain_documents,\n                                timings: Some(timings.as_ref()),\n                            },""",
        """                                retain_documents,\n                                timings: Some(timings.as_ref()),\n                                sync_limiter,\n                            },""",
    ),
    (
        """                retain_documents: false,\n                timings: None,\n            },""",
        """                retain_documents: false,\n                timings: None,\n                sync_limiter: None,\n            },""",
    ),
    (
        """    let worker_count = options.workers.max(1).min(segment_count.max(1));\n\n    std::thread::scope(|scope| {""",
        """    let worker_count = options.workers.max(1).min(segment_count.max(1));\n    let segment_sync_limiter =\n        durable.then(|| SegmentSyncLimiter::new(segment_sync_concurrency(worker_count)));\n\n    std::thread::scope(|scope| {""",
    ),
    (
        """            let output_dir = &output_dir;\n            let next = &next;\n            scope.spawn(move || {""",
        """            let output_dir = &output_dir;\n            let next = &next;\n            let sync_limiter = segment_sync_limiter.as_ref();\n            scope.spawn(move || {""",
    ),
    (
        """                        options.workers,\n                        durable,\n                    );""",
        """                        options.workers,\n                        durable,\n                        sync_limiter,\n                    );""",
    ),
    (
        """    build_workers: usize,\n    durable: bool,\n) -> Result<ManifestEntry> {""",
        """    build_workers: usize,\n    durable: bool,\n    sync_limiter: Option<&SegmentSyncLimiter>,\n) -> Result<ManifestEntry> {""",
    ),
    (
        "fn write_segment(path: &Path, data: &SegmentData, durable: bool) -> Result<WrittenSegmentMeta> {",
        """fn write_segment(\n    path: &Path,\n    data: &SegmentData,\n    durable: bool,\n    sync_limiter: Option<&SegmentSyncLimiter>,\n) -> Result<WrittenSegmentMeta> {""",
    ),
    (
        """    let sync = if durable {\n        let sync_started = Instant::now();\n        file.get_ref().sync_all()?;\n        sync_started.elapsed()\n    } else {""",
        """    let sync = if durable {\n        let sync_started = Instant::now();\n        let _sync_permit = sync_limiter.map(SegmentSyncLimiter::acquire);\n        file.get_ref().sync_all()?;\n        sync_started.elapsed()\n    } else {""",
    ),
]

for old, new in replacements:
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"expected exactly one match, found {count}: {old[:120]!r}")
    text = text.replace(old, new, 1)

# The parallel slice builder has a second direct write_segment call; update only that remaining call.
old = "let written = write_segment(&path, &data, durable)?;"
count = text.count(old)
if count != 1:
    raise SystemExit(f"expected exactly one remaining write_segment call, found {count}")
text = text.replace(old, "let written = write_segment(&path, &data, durable, sync_limiter)?;", 1)

# Add policy tests near the existing raw posting tests without touching format/durable behavior.
marker = "#[cfg(test)]\nmod raw_posting_tests {"
insert = """#[cfg(test)]\nmod segment_sync_concurrency_tests {\n    use super::segment_sync_concurrency_for;\n\n    #[test]\n    fn segment_sync_concurrency_is_bounded_by_worker_count() {\n        assert_eq!(segment_sync_concurrency_for(0, Some(4)), 1);\n        assert_eq!(segment_sync_concurrency_for(4, Some(1)), 1);\n        assert_eq!(segment_sync_concurrency_for(4, Some(2)), 2);\n        assert_eq!(segment_sync_concurrency_for(4, Some(4)), 4);\n        assert_eq!(segment_sync_concurrency_for(4, Some(99)), 4);\n        assert!((1..=4).contains(&segment_sync_concurrency_for(4, None)));\n    }\n}\n\n"""
count = text.count(marker)
if count != 1:
    raise SystemExit(f"expected test marker once, found {count}")
text = text.replace(marker, insert + marker, 1)

path.write_text(text, encoding="utf-8", newline="\n")
print("SEGMENT_SYNC_CONCURRENCY_PATCH_APPLIED")
