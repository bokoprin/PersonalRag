from pathlib import Path


def replace_once(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, got {count}")
    p.write_text(text.replace(old, new, 1), encoding="utf-8")


builder = "search-core/src/builder.rs"
bridge = "bridge-core/src/engine.rs"
example = "bridge-core/examples/index_build_profile.rs"

replace_once(
    builder,
    """    pub name_post_work: Duration,\n    pub segment_write_work: Duration,\n    pub acceleration_work: Duration,\n""",
    """    pub name_post_work: Duration,\n    pub segment_write_work: Duration,\n    pub segment_write_prepare_work: Duration,\n    pub segment_write_open_work: Duration,\n    pub segment_write_body_work: Duration,\n    pub segment_write_metadata_work: Duration,\n    pub segment_write_sync_work: Duration,\n    pub segment_write_finalize_work: Duration,\n    pub acceleration_work: Duration,\n""",
)

replace_once(
    builder,
    """    name_post_ns: AtomicU64,\n    segment_write_ns: AtomicU64,\n    acceleration_ns: AtomicU64,\n""",
    """    name_post_ns: AtomicU64,\n    segment_write_ns: AtomicU64,\n    segment_write_prepare_ns: AtomicU64,\n    segment_write_open_ns: AtomicU64,\n    segment_write_body_ns: AtomicU64,\n    segment_write_metadata_ns: AtomicU64,\n    segment_write_sync_ns: AtomicU64,\n    segment_write_finalize_ns: AtomicU64,\n    acceleration_ns: AtomicU64,\n""",
)

replace_once(
    builder,
    """            name_post_work: accumulated_duration(&self.name_post_ns),\n            segment_write_work: accumulated_duration(&self.segment_write_ns),\n            acceleration_work: accumulated_duration(&self.acceleration_ns),\n""",
    """            name_post_work: accumulated_duration(&self.name_post_ns),\n            segment_write_work: accumulated_duration(&self.segment_write_ns),\n            segment_write_prepare_work: accumulated_duration(&self.segment_write_prepare_ns),\n            segment_write_open_work: accumulated_duration(&self.segment_write_open_ns),\n            segment_write_body_work: accumulated_duration(&self.segment_write_body_ns),\n            segment_write_metadata_work: accumulated_duration(&self.segment_write_metadata_ns),\n            segment_write_sync_work: accumulated_duration(&self.segment_write_sync_ns),\n            segment_write_finalize_work: accumulated_duration(&self.segment_write_finalize_ns),\n            acceleration_work: accumulated_duration(&self.acceleration_ns),\n""",
)

replace_once(
    builder,
    """    if let Some(timings) = timings {\n        add_duration(&timings.segment_write_ns, base_write_elapsed);\n    }\n    let accel_started = Instant::now();\n""",
    """    if let Some(timings) = timings {\n        add_duration(&timings.segment_write_ns, base_write_elapsed);\n        add_duration(&timings.segment_write_prepare_ns, written.write_breakdown.prepare);\n        add_duration(&timings.segment_write_open_ns, written.write_breakdown.open);\n        add_duration(&timings.segment_write_body_ns, written.write_breakdown.body);\n        add_duration(&timings.segment_write_metadata_ns, written.write_breakdown.metadata);\n        add_duration(&timings.segment_write_sync_ns, written.write_breakdown.sync);\n        add_duration(&timings.segment_write_finalize_ns, written.write_breakdown.finalize);\n    }\n    let accel_started = Instant::now();\n""",
)

replace_once(
    builder,
    """    if profile_build {\n        eprintln!(\n            \"BUILD_SEGMENT_WALL segment={} docs={} sample_ms={:.3} base_ms={:.3} base_write_ms={:.3} accel_ms={:.3} total_ms={:.3}\",\n""",
    """    if profile_build {\n        eprintln!(\n            \"BUILD_SEGMENT_WRITE segment={} prepare_ms={:.3} open_ms={:.3} body_ms={:.3} metadata_ms={:.3} sync_ms={:.3} finalize_ms={:.3} total_ms={:.3}\",\n            task.segment_index,\n            written.write_breakdown.prepare.as_secs_f64() * 1000.0,\n            written.write_breakdown.open.as_secs_f64() * 1000.0,\n            written.write_breakdown.body.as_secs_f64() * 1000.0,\n            written.write_breakdown.metadata.as_secs_f64() * 1000.0,\n            written.write_breakdown.sync.as_secs_f64() * 1000.0,\n            written.write_breakdown.finalize.as_secs_f64() * 1000.0,\n            base_write_ms,\n        );\n        eprintln!(\n            \"BUILD_SEGMENT_WALL segment={} docs={} sample_ms={:.3} base_ms={:.3} base_write_ms={:.3} accel_ms={:.3} total_ms={:.3}\",\n""",
)

replace_once(
    builder,
    """#[derive(Clone, Copy, Debug)]\nstruct WrittenSegmentMeta {\n    checksum: u64,\n    bytes: u64,\n}\n\nfn write_segment(path: &Path, data: &SegmentData, durable: bool) -> Result<WrittenSegmentMeta> {\n    let sizes = section_sizes(data);\n""",
    """#[derive(Clone, Copy, Debug, Default)]\nstruct SegmentWriteBreakdown {\n    prepare: Duration,\n    open: Duration,\n    body: Duration,\n    metadata: Duration,\n    sync: Duration,\n    finalize: Duration,\n}\n\n#[derive(Clone, Copy, Debug)]\nstruct WrittenSegmentMeta {\n    checksum: u64,\n    bytes: u64,\n    write_breakdown: SegmentWriteBreakdown,\n}\n\nfn write_segment(path: &Path, data: &SegmentData, durable: bool) -> Result<WrittenSegmentMeta> {\n    let prepare_started = Instant::now();\n    let sizes = section_sizes(data);\n""",
)

replace_once(
    builder,
    """    write_u32(&mut header, 480, Q3DirKind::Prefix10 as u32);\n\n    let mut file = OpenOptions::new()\n        .create(true)\n        .truncate(true)\n        .write(true)\n        .open(path)?;\n    let mut hash = FNV_OFFSET;\n""",
    """    write_u32(&mut header, 480, Q3DirKind::Prefix10 as u32);\n    let prepare = prepare_started.elapsed();\n\n    let open_started = Instant::now();\n    let mut file = OpenOptions::new()\n        .create(true)\n        .truncate(true)\n        .write(true)\n        .open(path)?;\n    let open = open_started.elapsed();\n    let body_started = Instant::now();\n    let mut hash = FNV_OFFSET;\n""",
)

replace_once(
    builder,
    """    file.write_all(FOOTER_MAGIC)?;\n    file.write_all(&hash.to_le_bytes())?;\n    if file.metadata()?.len() != final_size {\n        return Err(SearchError::Format(\"segment final size mismatch\".into()));\n    }\n    if durable {\n        file.sync_all()?;\n    }\n    set_read_only(path)?;\n    Ok(WrittenSegmentMeta {\n        checksum: hash,\n        bytes: final_size,\n    })\n""",
    """    file.write_all(FOOTER_MAGIC)?;\n    file.write_all(&hash.to_le_bytes())?;\n    let body = body_started.elapsed();\n\n    let metadata_started = Instant::now();\n    if file.metadata()?.len() != final_size {\n        return Err(SearchError::Format(\"segment final size mismatch\".into()));\n    }\n    let metadata = metadata_started.elapsed();\n\n    let sync = if durable {\n        let sync_started = Instant::now();\n        file.sync_all()?;\n        sync_started.elapsed()\n    } else {\n        Duration::ZERO\n    };\n\n    let finalize_started = Instant::now();\n    set_read_only(path)?;\n    let finalize = finalize_started.elapsed();\n    Ok(WrittenSegmentMeta {\n        checksum: hash,\n        bytes: final_size,\n        write_breakdown: SegmentWriteBreakdown {\n            prepare,\n            open,\n            body,\n            metadata,\n            sync,\n            finalize,\n        },\n    })\n""",
)

replace_once(
    example,
    """            \"namePostWorkMs\": duration_ms(timings.name_post_work),\n            \"segmentWriteWorkMs\": duration_ms(timings.segment_write_work),\n            \"accelerationWorkMs\": duration_ms(timings.acceleration_work),\n""",
    """            \"namePostWorkMs\": duration_ms(timings.name_post_work),\n            \"segmentWriteWorkMs\": duration_ms(timings.segment_write_work),\n            \"segmentWritePrepareWorkMs\": duration_ms(timings.segment_write_prepare_work),\n            \"segmentWriteOpenWorkMs\": duration_ms(timings.segment_write_open_work),\n            \"segmentWriteBodyWorkMs\": duration_ms(timings.segment_write_body_work),\n            \"segmentWriteMetadataWorkMs\": duration_ms(timings.segment_write_metadata_work),\n            \"segmentWriteSyncWorkMs\": duration_ms(timings.segment_write_sync_work),\n            \"segmentWriteFinalizeWorkMs\": duration_ms(timings.segment_write_finalize_work),\n            \"accelerationWorkMs\": duration_ms(timings.acceleration_work),\n""",
)

replace_once(
    bridge,
    """            (\n                \"build.base_index.segment_write_work\",\n                base_detail.segment_write_work,\n            ),\n            (\n                \"build.base_index.acceleration_work\",\n""",
    """            (\n                \"build.base_index.segment_write_work\",\n                base_detail.segment_write_work,\n            ),\n            (\n                \"build.base_index.segment_write_prepare_work\",\n                base_detail.segment_write_prepare_work,\n            ),\n            (\n                \"build.base_index.segment_write_open_work\",\n                base_detail.segment_write_open_work,\n            ),\n            (\n                \"build.base_index.segment_write_body_work\",\n                base_detail.segment_write_body_work,\n            ),\n            (\n                \"build.base_index.segment_write_metadata_work\",\n                base_detail.segment_write_metadata_work,\n            ),\n            (\n                \"build.base_index.segment_write_sync_work\",\n                base_detail.segment_write_sync_work,\n            ),\n            (\n                \"build.base_index.segment_write_finalize_work\",\n                base_detail.segment_write_finalize_work,\n            ),\n            (\n                \"build.base_index.acceleration_work\",\n""",
)

replace_once(
    bridge,
    """            \"build.base_index.content_grams_work\",\n            \"build.base_index.segment_write_work\",\n            \"build.base_index.acceleration_work\",\n""",
    """            \"build.base_index.content_grams_work\",\n            \"build.base_index.segment_write_work\",\n            \"build.base_index.segment_write_prepare_work\",\n            \"build.base_index.segment_write_open_work\",\n            \"build.base_index.segment_write_body_work\",\n            \"build.base_index.segment_write_metadata_work\",\n            \"build.base_index.segment_write_sync_work\",\n            \"build.base_index.segment_write_finalize_work\",\n            \"build.base_index.acceleration_work\",\n""",
)

print("SEGMENT_WRITE_BREAKDOWN_PATCH_APPLIED")
