from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, got {count}: {old[:80]!r}")
    p.write_text(text.replace(old, new, 1), encoding="utf-8")


builder = "search-core/src/builder.rs"

replace_once(
    builder,
    """#[derive(Clone, Debug)]\npub struct DiskPathBuildReport {\n""",
    """#[derive(Clone, Copy, Debug, Default)]\npub struct DiskPathBuildTimings {\n    /// End-to-end hydration wall time. This can overlap segment building.\n    pub hydration_wall: Duration,\n    /// Summed worker time across all segments; may exceed wall time when workers overlap.\n    pub segment_sample_work: Duration,\n    pub segment_core_work: Duration,\n    pub name_grams_work: Duration,\n    pub dedup_work: Duration,\n    pub content_grams_work: Duration,\n    pub content_post_work: Duration,\n    pub name_post_work: Duration,\n    pub segment_write_work: Duration,\n    pub acceleration_work: Duration,\n    /// Manifest serialization/write wall time after segment workers complete.\n    pub manifest_write_wall: Duration,\n}\n\n#[derive(Clone, Debug)]\npub struct DiskPathBuildReport {\n""",
)
replace_once(
    builder,
    """    pub skipped_files: usize,\n    pub bytes_read: u64,\n}\n\n/// A file selected""",
    """    pub skipped_files: usize,\n    pub bytes_read: u64,\n    pub timings: DiskPathBuildTimings,\n}\n\n/// A file selected""",
)
replace_once(
    builder,
    """struct SegmentBuildTask {\n    documents: Vec<DocumentInput>,\n    doc_base: usize,\n    segment_index: usize,\n}\n\nfn should_collect_q2_seed""",
    """struct SegmentBuildTask {\n    documents: Vec<DocumentInput>,\n    doc_base: usize,\n    segment_index: usize,\n}\n\n#[derive(Default)]\nstruct BuildTimingAccumulator {\n    segment_sample_ns: AtomicU64,\n    segment_core_ns: AtomicU64,\n    name_grams_ns: AtomicU64,\n    dedup_ns: AtomicU64,\n    content_grams_ns: AtomicU64,\n    content_post_ns: AtomicU64,\n    name_post_ns: AtomicU64,\n    segment_write_ns: AtomicU64,\n    acceleration_ns: AtomicU64,\n}\n\nfn add_duration(counter: &AtomicU64, elapsed: Duration) {\n    counter.fetch_add(\n        elapsed.as_nanos().min(u128::from(u64::MAX)) as u64,\n        Ordering::Relaxed,\n    );\n}\n\nfn accumulated_duration(counter: &AtomicU64) -> Duration {\n    Duration::from_nanos(counter.load(Ordering::Relaxed))\n}\n\nimpl BuildTimingAccumulator {\n    fn snapshot(&self, hydration_wall_ns: u64, manifest_write_wall: Duration) -> DiskPathBuildTimings {\n        DiskPathBuildTimings {\n            hydration_wall: Duration::from_nanos(hydration_wall_ns),\n            segment_sample_work: accumulated_duration(&self.segment_sample_ns),\n            segment_core_work: accumulated_duration(&self.segment_core_ns),\n            name_grams_work: accumulated_duration(&self.name_grams_ns),\n            dedup_work: accumulated_duration(&self.dedup_ns),\n            content_grams_work: accumulated_duration(&self.content_grams_ns),\n            content_post_work: accumulated_duration(&self.content_post_ns),\n            name_post_work: accumulated_duration(&self.name_post_ns),\n            segment_write_work: accumulated_duration(&self.segment_write_ns),\n            acceleration_work: accumulated_duration(&self.acceleration_ns),\n            manifest_write_wall,\n        }\n    }\n}\n\nfn should_collect_q2_seed""",
)
replace_once(
    builder,
    """    durable: bool,\n    retain_documents: bool,\n) -> Result<(ManifestEntry, Option<Vec<DocumentInput>>)> {""",
    """    durable: bool,\n    retain_documents: bool,\n    timings: Option<&BuildTimingAccumulator>,\n) -> Result<(ManifestEntry, Option<Vec<DocumentInput>>)> {""",
)
replace_once(
    builder,
    """    let sample = sample_stats(&task.documents);\n    let sample_ms = sample_started.elapsed().as_secs_f64() * 1000.0;\n""",
    """    let sample = sample_stats(&task.documents);\n    let sample_elapsed = sample_started.elapsed();\n    let sample_ms = sample_elapsed.as_secs_f64() * 1000.0;\n    if let Some(timings) = timings {\n        add_duration(&timings.segment_sample_ns, sample_elapsed);\n    }\n""",
)
replace_once(
    builder,
    """    let mut data = build_segment_data_slice_impl(&task.documents, task.doc_base, kind, collect_q2)?;\n    let base_ms = base_started.elapsed().as_secs_f64() * 1000.0;\n""",
    """    let mut data = build_segment_data_slice_impl(\n        &task.documents,\n        task.doc_base,\n        kind,\n        collect_q2,\n        timings,\n    )?;\n    let base_elapsed = base_started.elapsed();\n    let base_ms = base_elapsed.as_secs_f64() * 1000.0;\n    if let Some(timings) = timings {\n        add_duration(&timings.segment_core_ns, base_elapsed);\n    }\n""",
)
replace_once(
    builder,
    """    let written = write_segment(&path, &data, durable)?;\n    let base_write_ms = base_write_started.elapsed().as_secs_f64() * 1000.0;\n""",
    """    let written = write_segment(&path, &data, durable)?;\n    let base_write_elapsed = base_write_started.elapsed();\n    let base_write_ms = base_write_elapsed.as_secs_f64() * 1000.0;\n    if let Some(timings) = timings {\n        add_duration(&timings.segment_write_ns, base_write_elapsed);\n    }\n""",
)
replace_once(
    builder,
    """    let accel_ms = if acceleration == AccelerationProfile::None {\n        0.0\n    } else {\n        accel_started.elapsed().as_secs_f64() * 1000.0\n    };\n""",
    """    let accel_elapsed = if acceleration == AccelerationProfile::None {\n        Duration::ZERO\n    } else {\n        accel_started.elapsed()\n    };\n    let accel_ms = accel_elapsed.as_secs_f64() * 1000.0;\n    if let Some(timings) = timings {\n        add_duration(&timings.acceleration_ns, accel_elapsed);\n    }\n""",
)
replace_once(
    builder,
    """    let started = Instant::now();\n    let profile_build = profile_build_enabled();\n    if profile_build {\n        PROFILE_HYDRATION_READ_NS.store(0, Ordering::Relaxed);\n        PROFILE_HYDRATION_NORMALIZE_NS.store(0, Ordering::Relaxed);\n    }\n    let mut hydration_wall_ns = 0u64;\n""",
    """    let started = Instant::now();\n    let profile_build = profile_build_enabled();\n    if profile_build {\n        PROFILE_HYDRATION_READ_NS.store(0, Ordering::Relaxed);\n        PROFILE_HYDRATION_NORMALIZE_NS.store(0, Ordering::Relaxed);\n    }\n    let timings = Arc::new(BuildTimingAccumulator::default());\n    let mut hydration_wall_ns = 0u64;\n""",
)
replace_once(
    builder,
    """                let ready_tx = ready_tx.clone();\n                let result_tx = result_tx.clone();\n                scope.spawn(move || {\n""",
    """                let ready_tx = ready_tx.clone();\n                let result_tx = result_tx.clone();\n                let timings = Arc::clone(&timings);\n                scope.spawn(move || {\n""",
)
replace_once(
    builder,
    """                            build_workers,\n                            durable,\n                            retain_documents,\n                        );\n""",
    """                            build_workers,\n                            durable,\n                            retain_documents,\n                            Some(timings.as_ref()),\n                        );\n""",
)
replace_once(
    builder,
    """    write_manifest(output_dir, options.mode, total_docs, &entries, durable)?;\n    let index_bytes = entries.iter().map(|entry| entry.bytes).sum::<u64>();\n""",
    """    let manifest_write_started = Instant::now();\n    write_manifest(output_dir, options.mode, total_docs, &entries, durable)?;\n    let manifest_write_wall = manifest_write_started.elapsed();\n    let index_bytes = entries.iter().map(|entry| entry.bytes).sum::<u64>();\n""",
)
replace_once(
    builder,
    """            skipped_files: progress.skipped_files,\n            bytes_read: progress.bytes_read,\n        },\n""",
    """            skipped_files: progress.skipped_files,\n            bytes_read: progress.bytes_read,\n            timings: timings.snapshot(hydration_wall_ns, manifest_write_wall),\n        },\n""",
)
replace_once(
    builder,
    """            1,\n            self.durable,\n            false,\n        )?;\n""",
    """            1,\n            self.durable,\n            false,\n            None,\n        )?;\n""",
)
replace_once(
    builder,
    """        should_collect_q2_seed(acceleration, segment_docs),\n    )?;\n""",
    """        should_collect_q2_seed(acceleration, segment_docs),\n        None,\n    )?;\n""",
)
replace_once(
    builder,
    """    kind: BuilderKind,\n    collect_q2: bool,\n) -> Result<SegmentData> {\n""",
    """    kind: BuilderKind,\n    collect_q2: bool,\n    timings: Option<&BuildTimingAccumulator>,\n) -> Result<SegmentData> {\n""",
)
for marker, field in [
    ("name_grams_ms", "name_grams_ns"),
    ("dedup_ms", "dedup_ns"),
    ("content_grams_ms", "content_grams_ns"),
    ("content_post_ms", "content_post_ns"),
    ("name_post_ms", "name_post_ns"),
]:
    old = f"    let {marker} = phase_started.elapsed().as_secs_f64() * 1000.0;\n"
    new = (
        f"    let {marker[:-3]}elapsed = phase_started.elapsed();\n"
        f"    let {marker} = {marker[:-3]}elapsed.as_secs_f64() * 1000.0;\n"
        f"    if let Some(timings) = timings {{\n"
        f"        add_duration(&timings.{field}, {marker[:-3]}elapsed);\n"
        f"    }}\n"
    )
    replace_once(builder, old, new)

replace_once(
    "search-core/src/lib.rs",
    """    DiskPathBuildReport, DiskPathInput, build_disk_corpus, build_disk_corpus_parallel,\n""",
    """    DiskPathBuildReport, DiskPathBuildTimings, DiskPathInput, build_disk_corpus,\n    build_disk_corpus_parallel,\n""",
)

engine = "bridge-core/src/engine.rs"
replace_once(
    engine,
    """        stage_timings.push(IndexBuildStageTiming::new(\n            "build.base_index",\n            base_index_started.elapsed().as_secs_f64() * 1000.0,\n        ));\n""",
    """        stage_timings.push(IndexBuildStageTiming::new(\n            "build.base_index",\n            base_index_started.elapsed().as_secs_f64() * 1000.0,\n        ));\n        let base_detail = report.timings;\n        for (name, elapsed) in [\n            ("build.base_index.hydration_wall", base_detail.hydration_wall),\n            (\n                "build.base_index.segment_sample_work",\n                base_detail.segment_sample_work,\n            ),\n            (\n                "build.base_index.segment_core_work",\n                base_detail.segment_core_work,\n            ),\n            ("build.base_index.name_grams_work", base_detail.name_grams_work),\n            ("build.base_index.dedup_work", base_detail.dedup_work),\n            (\n                "build.base_index.content_grams_work",\n                base_detail.content_grams_work,\n            ),\n            (\n                "build.base_index.content_post_work",\n                base_detail.content_post_work,\n            ),\n            ("build.base_index.name_post_work", base_detail.name_post_work),\n            (\n                "build.base_index.segment_write_work",\n                base_detail.segment_write_work,\n            ),\n            (\n                "build.base_index.acceleration_work",\n                base_detail.acceleration_work,\n            ),\n            (\n                "build.base_index.manifest_write_wall",\n                base_detail.manifest_write_wall,\n            ),\n        ] {\n            stage_timings.push(IndexBuildStageTiming::new(\n                name,\n                elapsed.as_secs_f64() * 1000.0,\n            ));\n        }\n""",
)

# Extend the existing timing regression so the medium-grained stages are guaranteed without
# PR_PROFILE_BUILD.
replace_once(
    engine,
    """            "build.base_index",\n            "build.verify_base",\n""",
    """            "build.base_index",\n            "build.base_index.hydration_wall",\n            "build.base_index.segment_core_work",\n            "build.base_index.content_grams_work",\n            "build.base_index.segment_write_work",\n            "build.base_index.acceleration_work",\n            "build.base_index.manifest_write_wall",\n            "build.verify_base",\n""",
)

print("base-index detail source transformation complete")
