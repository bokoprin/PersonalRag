use crate::format::{SearchError, fnv1a};
use crate::mapped_file::MappedFile;
use crate::vnext_fixed::{
    FixedPosting, build_content_fixed_index, build_content_q1_q2_from_q3_emission,
    build_content_q1_q2_from_q3_projection, build_content_q1_q2_fused_if_flat,
    build_path_fixed_index, lookup_fixed_index, validate_fixed_index,
};
use crate::vnext_q3::{
    VNextQ3Posting, build_path_q3_index, build_q3_index_with_q2_projection,
    build_q3_index_with_workers, collect_q3_cardinalities, lookup_q3, validate_q3_sections,
};
use std::fs::{self, File};
#[cfg(unix)]
use std::io::IoSlice;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::OnceLock;
use std::thread;
use std::time::Instant;

const MAGIC_V4: &[u8; 8] = b"PRSEG2A4";
const FOOTER_MAGIC_V4: &[u8; 8] = b"PR2FTR04";
const VERSION_V4: u32 = 4;
const MAGIC_V5: &[u8; 8] = b"PRSEG2A5";
const FOOTER_MAGIC_V5: &[u8; 8] = b"PR2FTR05";
const VERSION_V5: u32 = 5;
const MAGIC: &[u8; 8] = b"PRSEG2A6";
const FOOTER_MAGIC: &[u8; 8] = b"PR2FTR06";
const VERSION: u32 = 6;
const ENDIAN_MARKER: u32 = 0x0102_0304;
const HEADER_SIZE: usize = 128;
const SECTION_ENTRY_SIZE: usize = 32;
const SECTION_COUNT: usize = 14;
const FOOTER_SIZE: usize = 32;
const MAX_LOCAL_ITEMS: usize = u16::MAX as usize;
const DEFAULT_BLOCK_SIZE: u32 = 8 * 1024;
#[cfg(unix)]
const CONTENT_WRITEV_BATCH_DOCS: usize = 64;

const SECTION_DOC_SOA: u32 = 1;
const SECTION_PATH_BLOB: u32 = 2;
const SECTION_BLOCK_TABLE: u32 = 3;
const SECTION_CONTENT_BLOB: u32 = 4;
const SECTION_Q3_SHARD_DIR: u32 = 5;
const SECTION_Q3_DICTIONARY: u32 = 6;
const SECTION_Q3_POSTINGS: u32 = 7;
const SECTION_CONTENT_Q1: u32 = 8;
const SECTION_CONTENT_Q2: u32 = 9;
const SECTION_PATH_Q1: u32 = 10;
const SECTION_PATH_Q2: u32 = 11;
const SECTION_PATH_Q3_SHARD_DIR: u32 = 12;
const SECTION_PATH_Q3_DICTIONARY: u32 = 13;
const SECTION_PATH_Q3_POSTINGS: u32 = 14;

const HDR_VERSION: usize = 8;
const HDR_ENDIAN: usize = 12;
const HDR_HEADER_SIZE: usize = 16;
const HDR_SECTION_COUNT: usize = 20;
const HDR_DOC_COUNT: usize = 24;
const HDR_BLOCK_COUNT: usize = 28;
const HDR_BLOCK_SIZE: usize = 32;
const HDR_SECTION_DIR_OFF: usize = 40;
const HDR_FOOTER_OFF: usize = 48;
const HDR_FILE_SIZE: usize = 56;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VNextDocumentInput {
    pub logical_id: u64,
    pub display_path: String,
    pub normalized_content: Vec<u8>,
}

impl VNextDocumentInput {
    #[must_use]
    pub fn new(
        logical_id: u64,
        display_path: impl Into<String>,
        normalized_content: impl Into<Vec<u8>>,
    ) -> Self {
        Self {
            logical_id,
            display_path: display_path.into(),
            normalized_content: normalized_content.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VNextBlock {
    pub doc_id: u16,
    pub content_offset: u32,
    pub content_len: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VNextWriteReport {
    pub docs: u32,
    pub blocks: u32,
    pub block_size: u32,
    pub file_bytes: u64,
    pub q3_keys: u32,
    pub q3_posting_ids: u64,
    pub q3_active_shards: u16,
    pub q3_singleton_keys: u32,
    pub q3_raw_u16_keys: u32,
    pub q3_dense_bitmap_keys: u32,
    pub q3_posting_bytes: u64,
    pub content_q1_posting_bytes: u64,
    pub content_q2_posting_bytes: u64,
    pub path_q1_posting_bytes: u64,
    pub path_q2_posting_bytes: u64,
    pub path_q3_posting_bytes: u64,
}

#[derive(Clone, Copy, Debug)]
struct SectionDesc {
    kind: u32,
    off: u64,
    size: u64,
    checksum: u64,
}

pub struct VNextSegmentReader {
    mapped: MappedFile,
    doc_count: u32,
    block_count: u32,
    block_size: u32,
    sections: [SectionDesc; SECTION_COUNT],
    all_docs_single_block: bool,
    path_q3_cardinalities: OnceLock<Vec<(u32, u32)>>,
}

fn q3_worker_budget(available_cpus: usize, requested: Option<usize>) -> usize {
    let available_cpus = available_cpus.max(1);
    let safe_max = if available_cpus <= 2 {
        1
    } else {
        available_cpus.saturating_sub(2).clamp(1, 4)
    };
    requested.unwrap_or(safe_max).max(1).min(safe_max)
}

fn configured_q3_worker_budget(available_cpus: usize) -> usize {
    let requested = std::env::var("PR_Q3_WORKERS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok());
    q3_worker_budget(available_cpus, requested)
}

fn timed_index_build<T>(
    build: impl FnOnce() -> Result<T, SearchError>,
) -> (Result<T, SearchError>, f64) {
    let started = Instant::now();
    let value = build();
    (value, started.elapsed().as_secs_f64() * 1000.0)
}

pub fn write_vnext_segment(
    path: &Path,
    docs: &[VNextDocumentInput],
) -> Result<VNextWriteReport, SearchError> {
    write_vnext_segment_with_block_size(path, docs, DEFAULT_BLOCK_SIZE)
}

pub fn write_vnext_segment_with_block_size(
    path: &Path,
    docs: &[VNextDocumentInput],
    block_size: u32,
) -> Result<VNextWriteReport, SearchError> {
    let cpu_budget = thread::available_parallelism().map_or(1, usize::from);
    write_vnext_segment_with_block_size_and_cpu_budget(path, docs, block_size, cpu_budget)
}

pub(crate) fn write_vnext_segment_with_cpu_budget(
    path: &Path,
    docs: &[VNextDocumentInput],
    cpu_budget: usize,
) -> Result<VNextWriteReport, SearchError> {
    write_vnext_segment_with_block_size_and_cpu_budget(path, docs, DEFAULT_BLOCK_SIZE, cpu_budget)
}

fn write_vnext_segment_with_block_size_and_cpu_budget(
    path: &Path,
    docs: &[VNextDocumentInput],
    block_size: u32,
    cpu_budget: usize,
) -> Result<VNextWriteReport, SearchError> {
    if block_size == 0 {
        return Err(SearchError::InvalidArgument(
            "vNext block size must be greater than zero".into(),
        ));
    }
    if docs.len() > MAX_LOCAL_ITEMS {
        return Err(SearchError::InvalidArgument(format!(
            "vNext segment has too many documents: {} > {MAX_LOCAL_ITEMS}",
            docs.len()
        )));
    }

    let profile_build = std::env::var_os("PR_PROFILE_BUILD").is_some();
    let profile_q3 = profile_build && std::env::var_os("PR_PROFILE_Q3").is_some();
    let total_started = Instant::now();
    let layout_started = Instant::now();
    let path_bytes = docs.iter().try_fold(0usize, |total, doc| {
        total
            .checked_add(doc.display_path.len())
            .ok_or_else(|| SearchError::Format("vNext path blob size overflow".into()))
    })?;
    let content_bytes = docs.iter().try_fold(0usize, |total, doc| {
        total
            .checked_add(doc.normalized_content.len())
            .ok_or_else(|| SearchError::Format("vNext content blob size overflow".into()))
    })?;
    let block_capacity = docs.iter().try_fold(0usize, |total, doc| {
        total
            .checked_add(doc.normalized_content.len().div_ceil(block_size as usize))
            .ok_or_else(|| SearchError::Format("vNext block count overflow".into()))
    })?;
    let mut logical_ids = Vec::with_capacity(docs.len());
    let mut path_offsets = Vec::with_capacity(docs.len() + 1);
    let mut first_blocks = Vec::with_capacity(docs.len());
    let mut block_counts = Vec::with_capacity(docs.len());
    let mut path_blob = Vec::with_capacity(path_bytes);
    let mut content_written = 0usize;
    let mut blocks = Vec::with_capacity(block_capacity);

    path_offsets.push(0u32);
    for (doc_index, doc) in docs.iter().enumerate() {
        logical_ids.push(doc.logical_id);
        path_blob.extend_from_slice(doc.display_path.as_bytes());
        path_offsets.push(checked_u32(path_blob.len(), "path blob")?);

        if blocks.len() > MAX_LOCAL_ITEMS {
            return Err(SearchError::InvalidArgument(
                "vNext segment block count exceeded u16 local-ID bound".into(),
            ));
        }
        first_blocks.push(checked_u16(blocks.len(), "first block")?);

        let block_start = blocks.len();
        for chunk in doc.normalized_content.chunks(block_size as usize) {
            if blocks.len() >= MAX_LOCAL_ITEMS {
                return Err(SearchError::InvalidArgument(format!(
                    "vNext segment has too many blocks: more than {MAX_LOCAL_ITEMS}"
                )));
            }
            let content_offset = checked_u32(content_written, "content blob offset")?;
            let content_len = checked_u32(chunk.len(), "block length")?;
            content_written = content_written
                .checked_add(chunk.len())
                .ok_or_else(|| SearchError::Format("vNext content blob size overflow".into()))?;
            blocks.push(VNextBlock {
                doc_id: checked_u16(doc_index, "document local ID")?,
                content_offset,
                content_len,
            });
        }
        block_counts.push(checked_u16(
            blocks.len() - block_start,
            "document block count",
        )?);
    }
    let _ = checked_u32(content_written, "content blob")?;
    debug_assert_eq!(content_written, content_bytes);
    let layout_ms = layout_started.elapsed().as_secs_f64() * 1000.0;

    let index_group_started = Instant::now();
    let available_cpus = cpu_budget.max(1);
    let q3_workers = configured_q3_worker_budget(available_cpus);
    let (
        q3,
        q3_ms,
        content_q1,
        content_q1_ms,
        content_q2,
        content_q2_ms,
        content_q12_fused_ms,
        path_q1,
        path_q1_ms,
        path_q2,
        path_q2_ms,
        path_q3,
        path_q3_ms,
    ) = if available_cpus <= 1 {
        let (q3, q3_ms) = timed_index_build(|| {
            build_q3_index_with_q2_projection(
                docs,
                &first_blocks,
                block_size,
                blocks.len(),
                profile_q3,
            )
        });
        let mut q3 = q3?;
        let (content_q1, content_q1_ms, content_q2, content_q2_ms, content_q12_fused_ms) =
            if q3.emitted_q2_pairs.is_some() && q3.emitted_q1_lists.is_some() {
                let q2_pairs = q3.emitted_q2_pairs.take().expect("checked emitted q2");
                let q1_lists = q3.emitted_q1_lists.take().expect("checked emitted q1");
                let (projected, projected_ms) = timed_index_build(|| {
                    build_content_q1_q2_from_q3_emission(blocks.len(), q1_lists, q2_pairs)
                });
                let (content_q1, content_q2) = projected?;
                (content_q1, 0.0, content_q2, 0.0, projected_ms)
            } else if let Some(projected_q2) = q3.projected_q2.take() {
                let (projected, projected_ms) = timed_index_build(|| {
                    build_content_q1_q2_from_q3_projection(
                        docs,
                        &first_blocks,
                        block_size,
                        blocks.len(),
                        projected_q2,
                    )
                });
                let (content_q1, content_q2) = projected?;
                (content_q1, 0.0, content_q2, 0.0, projected_ms)
            } else {
                let (fused_fixed, fused_fixed_ms) = timed_index_build(|| {
                    build_content_q1_q2_fused_if_flat(
                        docs,
                        &first_blocks,
                        block_size,
                        blocks.len(),
                        Some(&q3.periodic_q2_skip_from),
                    )
                });
                let fused_fixed = fused_fixed?;
                if let Some((content_q1, content_q2)) = fused_fixed {
                    (content_q1, 0.0, content_q2, 0.0, fused_fixed_ms)
                } else {
                    let (content_q2, content_q2_ms) = timed_index_build(|| {
                        build_content_fixed_index(docs, &first_blocks, block_size, blocks.len(), 2)
                    });
                    let (content_q1, content_q1_ms) = timed_index_build(|| {
                        build_content_fixed_index(docs, &first_blocks, block_size, blocks.len(), 1)
                    });
                    (content_q1?, content_q1_ms, content_q2?, content_q2_ms, 0.0)
                }
            };
        let (path_q1, path_q1_ms) = timed_index_build(|| build_path_fixed_index(docs, 1));
        let (path_q2, path_q2_ms) = timed_index_build(|| build_path_fixed_index(docs, 2));
        let (path_q3, path_q3_ms) = timed_index_build(|| build_path_q3_index(docs, profile_q3));
        (
            q3,
            q3_ms,
            content_q1,
            content_q1_ms,
            content_q2,
            content_q2_ms,
            content_q12_fused_ms,
            path_q1?,
            path_q1_ms,
            path_q2?,
            path_q2_ms,
            path_q3?,
            path_q3_ms,
        )
    } else if available_cpus == 2 {
        thread::scope(|scope| {
            let q3_lane = scope.spawn(|| {
                let q3 = timed_index_build(|| {
                    build_q3_index_with_workers(
                        docs,
                        &first_blocks,
                        block_size,
                        blocks.len(),
                        profile_q3,
                        1,
                    )
                });
                let path_q3 = timed_index_build(|| build_path_q3_index(docs, profile_q3));
                (q3, path_q3)
            });
            let q2_q1_lane = scope.spawn(|| {
                let content_q2 = timed_index_build(|| {
                    build_content_fixed_index(docs, &first_blocks, block_size, blocks.len(), 2)
                });
                let content_q1 = timed_index_build(|| {
                    build_content_fixed_index(docs, &first_blocks, block_size, blocks.len(), 1)
                });
                let path_q2 = timed_index_build(|| build_path_fixed_index(docs, 2));
                let path_q1 = timed_index_build(|| build_path_fixed_index(docs, 1));
                (content_q2, content_q1, path_q2, path_q1)
            });
            let (q3, path_q3) = q3_lane
                .join()
                .map_err(|_| SearchError::Format("vNext q3 lane panicked".into()))?;
            let (content_q2, content_q1, path_q2, path_q1) = q2_q1_lane
                .join()
                .map_err(|_| SearchError::Format("vNext q2/q1 lane panicked".into()))?;
            Ok::<_, SearchError>((
                q3.0?,
                q3.1,
                content_q1.0?,
                content_q1.1,
                content_q2.0?,
                content_q2.1,
                0.0,
                path_q1.0?,
                path_q1.1,
                path_q2.0?,
                path_q2.1,
                path_q3.0?,
                path_q3.1,
            ))
        })?
    } else {
        // Three bounded lanes pair work by gram width. While content q3 uses internal workers,
        // q2 and q1 each occupy one lane, so q3_workers + 2 never exceeds the CPU budget.
        // When a content index finishes, that same lane immediately builds the matching path
        // index. This avoids a fixed auxiliary tail when repetitive content makes q3 very cheap.
        thread::scope(|scope| {
            let q3_lane = scope.spawn(|| {
                let q3 = timed_index_build(|| {
                    build_q3_index_with_workers(
                        docs,
                        &first_blocks,
                        block_size,
                        blocks.len(),
                        profile_q3,
                        q3_workers,
                    )
                });
                let path_q3 = timed_index_build(|| build_path_q3_index(docs, profile_q3));
                (q3, path_q3)
            });
            let q2_lane = scope.spawn(|| {
                let content_q2 = timed_index_build(|| {
                    build_content_fixed_index(docs, &first_blocks, block_size, blocks.len(), 2)
                });
                let path_q2 = timed_index_build(|| build_path_fixed_index(docs, 2));
                (content_q2, path_q2)
            });
            let q1_lane = scope.spawn(|| {
                let content_q1 = timed_index_build(|| {
                    build_content_fixed_index(docs, &first_blocks, block_size, blocks.len(), 1)
                });
                let path_q1 = timed_index_build(|| build_path_fixed_index(docs, 1));
                (content_q1, path_q1)
            });

            let (q3, path_q3) = q3_lane
                .join()
                .map_err(|_| SearchError::Format("vNext q3 lane panicked".into()))?;
            let (content_q2, path_q2) = q2_lane
                .join()
                .map_err(|_| SearchError::Format("vNext q2 lane panicked".into()))?;
            let (content_q1, path_q1) = q1_lane
                .join()
                .map_err(|_| SearchError::Format("vNext q1 lane panicked".into()))?;
            Ok::<_, SearchError>((
                q3.0?,
                q3.1,
                content_q1.0?,
                content_q1.1,
                content_q2.0?,
                content_q2.1,
                0.0,
                path_q1.0?,
                path_q1.1,
                path_q2.0?,
                path_q2.1,
                path_q3.0?,
                path_q3.1,
            ))
        })?
    };
    let index_group_ms = index_group_started.elapsed().as_secs_f64() * 1000.0;
    let encode_started = Instant::now();
    let q3_keys = q3.key_count;
    let q3_posting_ids = q3.posting_ids;
    let q3_active_shards = q3.active_shards;
    let q3_singleton_keys = q3.singleton_keys;
    let q3_raw_u16_keys = q3.raw_u16_keys;
    let q3_dense_bitmap_keys = q3.dense_bitmap_keys;
    let q3_posting_bytes = q3.postings.len() as u64;
    let q3_build_profile = q3.build_profile;
    let path_q3_build_profile = path_q3.build_profile;
    let content_q1_posting_bytes = content_q1.stats.posting_bytes;
    let content_q2_posting_bytes = content_q2.stats.posting_bytes;
    let path_q1_posting_bytes = path_q1.stats.posting_bytes;
    let path_q2_posting_bytes = path_q2.stats.posting_bytes;
    let path_q3_posting_bytes = path_q3.postings.len() as u64;
    let doc_soa = encode_doc_soa(&logical_ids, &path_offsets, &first_blocks, &block_counts);
    let block_table = encode_block_table(&blocks);
    let sections_data = [
        doc_soa,
        path_blob,
        block_table,
        Vec::new(),
        q3.shard_dir,
        q3.dictionary,
        q3.postings,
        content_q1.bytes,
        content_q2.bytes,
        path_q1.bytes,
        path_q2.bytes,
        path_q3.shard_dir,
        path_q3.dictionary,
        path_q3.postings,
    ];
    let section_kinds = [
        SECTION_DOC_SOA,
        SECTION_PATH_BLOB,
        SECTION_BLOCK_TABLE,
        SECTION_CONTENT_BLOB,
        SECTION_Q3_SHARD_DIR,
        SECTION_Q3_DICTIONARY,
        SECTION_Q3_POSTINGS,
        SECTION_CONTENT_Q1,
        SECTION_CONTENT_Q2,
        SECTION_PATH_Q1,
        SECTION_PATH_Q2,
        SECTION_PATH_Q3_SHARD_DIR,
        SECTION_PATH_Q3_DICTIONARY,
        SECTION_PATH_Q3_POSTINGS,
    ];

    let section_dir_off = HEADER_SIZE as u64;
    let mut cursor = align8((HEADER_SIZE + SECTION_ENTRY_SIZE * SECTION_COUNT) as u64);
    let mut sections = [SectionDesc {
        kind: 0,
        off: 0,
        size: 0,
        checksum: 0,
    }; SECTION_COUNT];
    let encode_ms = encode_started.elapsed().as_secs_f64() * 1000.0;
    let checksum_started = Instant::now();
    let section_checksums = compute_section_checksums(&sections_data, available_cpus);
    let checksum_ms = checksum_started.elapsed().as_secs_f64() * 1000.0;
    for index in 0..SECTION_COUNT {
        cursor = align8(cursor);
        sections[index] = SectionDesc {
            kind: section_kinds[index],
            off: cursor,
            size: if section_kinds[index] == SECTION_CONTENT_BLOB {
                content_bytes as u64
            } else {
                sections_data[index].len() as u64
            },
            checksum: section_checksums[index],
        };
        cursor = cursor
            .checked_add(sections[index].size)
            .ok_or_else(|| SearchError::Format("vNext file size overflow".into()))?;
    }
    let footer_off = align8(cursor);
    let file_size = footer_off
        .checked_add(FOOTER_SIZE as u64)
        .ok_or_else(|| SearchError::Format("vNext file size overflow".into()))?;

    let first_section_off = sections.first().map_or(footer_off, |section| section.off);
    let prefix_len = usize::try_from(first_section_off)
        .map_err(|_| SearchError::Format("vNext prefix is too large for this platform".into()))?;
    let mut prefix = vec![0u8; prefix_len];
    prefix[..8].copy_from_slice(MAGIC);
    put_u32_at(&mut prefix, HDR_VERSION, VERSION)?;
    put_u32_at(&mut prefix, HDR_ENDIAN, ENDIAN_MARKER)?;
    put_u32_at(&mut prefix, HDR_HEADER_SIZE, HEADER_SIZE as u32)?;
    put_u32_at(&mut prefix, HDR_SECTION_COUNT, SECTION_COUNT as u32)?;
    put_u32_at(&mut prefix, HDR_DOC_COUNT, docs.len() as u32)?;
    put_u32_at(&mut prefix, HDR_BLOCK_COUNT, blocks.len() as u32)?;
    put_u32_at(&mut prefix, HDR_BLOCK_SIZE, block_size)?;
    put_u64_at(&mut prefix, HDR_SECTION_DIR_OFF, section_dir_off)?;
    put_u64_at(&mut prefix, HDR_FOOTER_OFF, footer_off)?;
    put_u64_at(&mut prefix, HDR_FILE_SIZE, file_size)?;

    for (index, desc) in sections.iter().enumerate() {
        let entry = HEADER_SIZE + index * SECTION_ENTRY_SIZE;
        put_u32_at(&mut prefix, entry, desc.kind)?;
        put_u64_at(&mut prefix, entry + 8, desc.off)?;
        put_u64_at(&mut prefix, entry + 16, desc.size)?;
        put_u64_at(&mut prefix, entry + 24, desc.checksum)?;
    }

    let write_started = Instant::now();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("prseg2.tmp");
    let mut write_open_ms = 0.0f64;
    let mut write_stream_ms = 0.0f64;
    let mut write_sync_ms = 0.0f64;
    let write_result = (|| -> Result<(), SearchError> {
        let open_started = Instant::now();
        let file = File::create(&temp)?;
        if profile_build {
            write_open_ms = open_started.elapsed().as_secs_f64() * 1000.0;
        }
        let stream_started = Instant::now();
        let mut writer = BufWriter::with_capacity(1024 * 1024, file);
        let mut whole_checksum = Xxh64State::new();
        let mut written = 0u64;

        write_whole_hashed(&mut writer, &mut whole_checksum, &prefix)?;
        written = written
            .checked_add(prefix.len() as u64)
            .ok_or_else(|| SearchError::Format("vNext written byte count overflow".into()))?;

        for (desc, data) in sections.iter().zip(sections_data.iter()) {
            write_zero_padding_whole(&mut writer, &mut whole_checksum, &mut written, desc.off)?;
            if desc.kind == SECTION_CONTENT_BLOB {
                write_content_blob_hashed(&mut writer, &mut whole_checksum, docs)?;
                written = written.checked_add(content_bytes as u64).ok_or_else(|| {
                    SearchError::Format("vNext written byte count overflow".into())
                })?;
            } else {
                write_whole_hashed(&mut writer, &mut whole_checksum, data)?;
                written = written.checked_add(data.len() as u64).ok_or_else(|| {
                    SearchError::Format("vNext written byte count overflow".into())
                })?;
            }
        }
        write_zero_padding_whole(&mut writer, &mut whole_checksum, &mut written, footer_off)?;

        if written != footer_off {
            return Err(SearchError::Format(
                "vNext streaming writer footer offset mismatch".into(),
            ));
        }

        let mut footer = [0u8; FOOTER_SIZE];
        footer[..8].copy_from_slice(FOOTER_MAGIC);
        put_u32_at(&mut footer, 8, VERSION)?;
        put_u64_at(&mut footer, 16, file_size)?;
        put_u64_at(&mut footer, 24, whole_checksum.digest())?;
        writer.write_all(&footer)?;
        writer.flush()?;
        let file = writer
            .into_inner()
            .map_err(|error| SearchError::Io(error.into_error()))?;
        if profile_build {
            write_stream_ms = stream_started.elapsed().as_secs_f64() * 1000.0;
        }
        let sync_started = Instant::now();
        file.sync_all()?;
        if profile_build {
            write_sync_ms = sync_started.elapsed().as_secs_f64() * 1000.0;
        }
        Ok(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    let rename_started = Instant::now();
    fs::rename(&temp, path)?;
    let write_rename_ms = if profile_build {
        rename_started.elapsed().as_secs_f64() * 1000.0
    } else {
        0.0
    };

    if profile_q3 {
        eprintln!(
            "VNEXT_Q3_PHASE segment={} kind=content occurrences={} radix_occurrences={} local_saved={} local_blocks={} local_hash_blocks={} periodic_skip_occurrences={} periodic_skip_blocks={} direct_blocks={} sample_occurrences={} sample_duplicates={} unique_pairs={} emit_workers={} shard_workers={} emit_ms={:.3} shard_wall_ms={:.3} radix_prepare_cpu_ms={:.3} radix_count_cpu_ms={:.3} radix_prefix_cpu_ms={:.3} radix_scatter_cpu_ms={:.3} dedup_cpu_ms={:.3} encode_cpu_ms={:.3} q2_projection_cpu_ms={:.3} q2_projection_pairs={}",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("?"),
            q3_build_profile.occurrences,
            q3_build_profile.radix_occurrences,
            q3_build_profile.local_dedup_saved,
            q3_build_profile.local_dedup_blocks,
            q3_build_profile.local_hash_blocks,
            q3_build_profile.periodic_skip_occurrences,
            q3_build_profile.periodic_skip_blocks,
            q3_build_profile.direct_blocks,
            q3_build_profile.local_sample_occurrences,
            q3_build_profile.local_sample_duplicates,
            q3_build_profile.unique_pairs,
            q3_build_profile.emit_workers,
            q3_build_profile.shard_workers,
            q3_build_profile.emit_ms,
            q3_build_profile.shard_wall_ms,
            q3_build_profile.radix_prepare_ms,
            q3_build_profile.radix_count_ms,
            q3_build_profile.radix_prefix_ms,
            q3_build_profile.radix_scatter_ms,
            q3_build_profile.dedup_ms,
            q3_build_profile.encode_ms,
            q3_build_profile.q2_projection_ms,
            q3_build_profile.q2_projection_pairs,
        );
        eprintln!(
            "VNEXT_Q3_PHASE segment={} kind=path occurrences={} radix_occurrences={} local_saved={} local_blocks={} local_hash_blocks={} direct_blocks={} sample_occurrences={} sample_duplicates={} unique_pairs={} emit_workers={} shard_workers={} emit_ms={:.3} shard_wall_ms={:.3} radix_prepare_cpu_ms={:.3} radix_count_cpu_ms={:.3} radix_prefix_cpu_ms={:.3} radix_scatter_cpu_ms={:.3} dedup_cpu_ms={:.3} encode_cpu_ms={:.3}",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("?"),
            path_q3_build_profile.occurrences,
            path_q3_build_profile.radix_occurrences,
            path_q3_build_profile.local_dedup_saved,
            path_q3_build_profile.local_dedup_blocks,
            path_q3_build_profile.local_hash_blocks,
            path_q3_build_profile.direct_blocks,
            path_q3_build_profile.local_sample_occurrences,
            path_q3_build_profile.local_sample_duplicates,
            path_q3_build_profile.unique_pairs,
            path_q3_build_profile.emit_workers,
            path_q3_build_profile.shard_workers,
            path_q3_build_profile.emit_ms,
            path_q3_build_profile.shard_wall_ms,
            path_q3_build_profile.radix_prepare_ms,
            path_q3_build_profile.radix_count_ms,
            path_q3_build_profile.radix_prefix_ms,
            path_q3_build_profile.radix_scatter_ms,
            path_q3_build_profile.dedup_ms,
            path_q3_build_profile.encode_ms,
        );
    }
    if profile_build {
        eprintln!(
            "VNEXT_BUILD_PHASE segment={} docs={} cpu_budget={} layout_ms={:.3} index_group_ms={:.3} cq1_ms={:.3} cq2_ms={:.3} cq12_fused_ms={:.3} cq3_ms={:.3} pq1_ms={:.3} pq2_ms={:.3} pq3_ms={:.3} encode_ms={:.3} checksum_ms={:.3} write_open_ms={:.3} write_stream_ms={:.3} write_sync_ms={:.3} write_rename_ms={:.3} write_ms={:.3} total_ms={:.3}",
            path.file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("?"),
            docs.len(),
            available_cpus,
            layout_ms,
            index_group_ms,
            content_q1_ms,
            content_q2_ms,
            content_q12_fused_ms,
            q3_ms,
            path_q1_ms,
            path_q2_ms,
            path_q3_ms,
            encode_ms,
            checksum_ms,
            write_open_ms,
            write_stream_ms,
            write_sync_ms,
            write_rename_ms,
            write_started.elapsed().as_secs_f64() * 1000.0,
            total_started.elapsed().as_secs_f64() * 1000.0,
        );
    }

    Ok(VNextWriteReport {
        docs: docs.len() as u32,
        blocks: blocks.len() as u32,
        block_size,
        file_bytes: file_size,
        q3_keys,
        q3_posting_ids,
        q3_active_shards,
        q3_singleton_keys,
        q3_raw_u16_keys,
        q3_dense_bitmap_keys,
        q3_posting_bytes,
        content_q1_posting_bytes,
        content_q2_posting_bytes,
        path_q1_posting_bytes,
        path_q2_posting_bytes,
        path_q3_posting_bytes,
    })
}

impl VNextSegmentReader {
    pub fn open(path: &Path) -> Result<Self, SearchError> {
        Self::open_impl(path, true)
    }

    pub(crate) fn open_published(path: &Path) -> Result<Self, SearchError> {
        Self::open_impl(path, false)
    }

    fn open_impl(path: &Path, strict_structure: bool) -> Result<Self, SearchError> {
        let mapped = MappedFile::open(path)?;
        let bytes = mapped.as_slice();
        if bytes.len() < HEADER_SIZE + SECTION_ENTRY_SIZE * SECTION_COUNT + FOOTER_SIZE {
            return Err(SearchError::Format("vNext segment is truncated".into()));
        }
        let version = match bytes.get(..8) {
            Some(magic) if magic == MAGIC.as_slice() => VERSION,
            Some(magic) if magic == MAGIC_V5.as_slice() => VERSION_V5,
            Some(magic) if magic == MAGIC_V4.as_slice() => VERSION_V4,
            _ => return Err(SearchError::Format("bad vNext segment magic".into())),
        };
        if rd_u32(bytes, HDR_VERSION)? != version {
            return Err(SearchError::Format(
                "unsupported vNext segment version".into(),
            ));
        }
        if rd_u32(bytes, HDR_ENDIAN)? != ENDIAN_MARKER {
            return Err(SearchError::Format("bad vNext little-endian marker".into()));
        }
        if rd_u32(bytes, HDR_HEADER_SIZE)? != HEADER_SIZE as u32 {
            return Err(SearchError::Format("bad vNext header size".into()));
        }
        if rd_u32(bytes, HDR_SECTION_COUNT)? != SECTION_COUNT as u32 {
            return Err(SearchError::Format("bad vNext section count".into()));
        }
        let doc_count = rd_u32(bytes, HDR_DOC_COUNT)?;
        let block_count = rd_u32(bytes, HDR_BLOCK_COUNT)?;
        let block_size = rd_u32(bytes, HDR_BLOCK_SIZE)?;
        if block_size == 0 {
            return Err(SearchError::Format("vNext block size is zero".into()));
        }
        if doc_count as usize > MAX_LOCAL_ITEMS || block_count as usize > MAX_LOCAL_ITEMS {
            return Err(SearchError::Format("vNext local-ID bound exceeded".into()));
        }
        let section_dir_off = rd_u64(bytes, HDR_SECTION_DIR_OFF)?;
        let footer_off = rd_u64(bytes, HDR_FOOTER_OFF)?;
        let file_size = rd_u64(bytes, HDR_FILE_SIZE)?;
        if section_dir_off != HEADER_SIZE as u64 || file_size != bytes.len() as u64 {
            return Err(SearchError::Format(
                "vNext header offsets are inconsistent".into(),
            ));
        }
        let footer = usize::try_from(footer_off)
            .map_err(|_| SearchError::Format("vNext footer offset too large".into()))?;
        if footer.checked_add(FOOTER_SIZE) != Some(bytes.len()) {
            return Err(SearchError::Format(
                "vNext footer is not at end of file".into(),
            ));
        }
        let expected_footer_magic = match version {
            VERSION => FOOTER_MAGIC,
            VERSION_V5 => FOOTER_MAGIC_V5,
            VERSION_V4 => FOOTER_MAGIC_V4,
            _ => unreachable!("version is validated from magic"),
        };
        if bytes.get(footer..footer + 8) != Some(expected_footer_magic.as_slice()) {
            return Err(SearchError::Format("bad vNext footer magic".into()));
        }
        if rd_u32(bytes, footer + 8)? != version || rd_u64(bytes, footer + 16)? != file_size {
            return Err(SearchError::Format("bad vNext footer metadata".into()));
        }
        let expected_file_checksum = rd_u64(bytes, footer + 24)?;
        let actual_file_checksum = if version >= VERSION {
            xxh64(&bytes[..footer])
        } else {
            fnv1a(&bytes[..footer])
        };
        if expected_file_checksum != actual_file_checksum {
            return Err(SearchError::Format("vNext file checksum mismatch".into()));
        }

        let mut sections = [SectionDesc {
            kind: 0,
            off: 0,
            size: 0,
            checksum: 0,
        }; SECTION_COUNT];
        for (index, expected_kind) in [
            SECTION_DOC_SOA,
            SECTION_PATH_BLOB,
            SECTION_BLOCK_TABLE,
            SECTION_CONTENT_BLOB,
            SECTION_Q3_SHARD_DIR,
            SECTION_Q3_DICTIONARY,
            SECTION_Q3_POSTINGS,
            SECTION_CONTENT_Q1,
            SECTION_CONTENT_Q2,
            SECTION_PATH_Q1,
            SECTION_PATH_Q2,
            SECTION_PATH_Q3_SHARD_DIR,
            SECTION_PATH_Q3_DICTIONARY,
            SECTION_PATH_Q3_POSTINGS,
        ]
        .into_iter()
        .enumerate()
        {
            let entry = HEADER_SIZE + index * SECTION_ENTRY_SIZE;
            let desc = SectionDesc {
                kind: rd_u32(bytes, entry)?,
                off: rd_u64(bytes, entry + 8)?,
                size: rd_u64(bytes, entry + 16)?,
                checksum: rd_u64(bytes, entry + 24)?,
            };
            if desc.kind != expected_kind {
                return Err(SearchError::Format(format!(
                    "bad vNext section kind {} at slot {index}",
                    desc.kind
                )));
            }
            let section = checked_range(bytes, desc.off, desc.size, footer_off)?;
            // Published-fast open always verifies the version-specific whole-file checksum above. v5+
            // makes the large content section rely on that 64-bit checksum instead of hashing
            // the same content a second time during build. All other sections keep standalone
            // FNV checksums; v4 retains the original content checksum for backward compatibility.
            let content_without_section_checksum =
                version >= VERSION_V5 && desc.kind == SECTION_CONTENT_BLOB;
            if content_without_section_checksum && desc.checksum != 0 {
                return Err(SearchError::Format(
                    "vNext v5+ content checksum field must be zero".into(),
                ));
            }
            if strict_structure
                && !content_without_section_checksum
                && fnv1a(section) != desc.checksum
            {
                return Err(SearchError::Format(format!(
                    "vNext section checksum mismatch for kind {}",
                    desc.kind
                )));
            }
            sections[index] = desc;
        }
        validate_non_overlapping(&sections, footer_off)?;

        let mut reader = Self {
            mapped,
            doc_count,
            block_count,
            block_size,
            sections,
            all_docs_single_block: false,
            path_q3_cardinalities: OnceLock::new(),
        };
        if strict_structure {
            reader.validate_structure()?;
        }
        reader.all_docs_single_block = reader.compute_all_docs_single_block()?;
        // Path q3 cardinalities are a query-planning accelerator, not part of structural
        // validation. Building the full cache here made multi-segment generation open scale with
        // every path q3 key. Keep it lazy so content-only workloads and restart/open do not pay
        // that cost. The underlying q3 sections were fully validated above.
        Ok(reader)
    }

    #[must_use]
    pub const fn doc_count(&self) -> u32 {
        self.doc_count
    }

    #[must_use]
    pub const fn block_count(&self) -> u32 {
        self.block_count
    }

    #[must_use]
    pub const fn block_size(&self) -> u32 {
        self.block_size
    }

    pub fn q3_posting(&self, gram: [u8; 3]) -> Result<VNextQ3Posting<'_>, SearchError> {
        lookup_q3(
            self.section_bytes(SECTION_Q3_SHARD_DIR)?,
            self.section_bytes(SECTION_Q3_DICTIONARY)?,
            self.section_bytes(SECTION_Q3_POSTINGS)?,
            gram,
            self.block_count,
        )
    }

    pub fn q3_stats(&self) -> Result<(u32, u64, u16), SearchError> {
        validate_q3_sections(
            self.section_bytes(SECTION_Q3_SHARD_DIR)?,
            self.section_bytes(SECTION_Q3_DICTIONARY)?,
            self.section_bytes(SECTION_Q3_POSTINGS)?,
            self.block_count,
        )
    }

    pub(crate) fn content_short_posting(
        &self,
        gram: &[u8],
    ) -> Result<FixedPosting<'_>, SearchError> {
        let kind = match gram.len() {
            1 => SECTION_CONTENT_Q1,
            2 => SECTION_CONTENT_Q2,
            _ => {
                return Err(SearchError::InvalidArgument(
                    "content short gram must be q1/q2".into(),
                ));
            }
        };
        lookup_fixed_index(self.section_bytes(kind)?, gram, self.block_count as usize)
    }

    pub(crate) fn path_short_posting(&self, gram: &[u8]) -> Result<FixedPosting<'_>, SearchError> {
        let kind = match gram.len() {
            1 => SECTION_PATH_Q1,
            2 => SECTION_PATH_Q2,
            _ => {
                return Err(SearchError::InvalidArgument(
                    "path short gram must be q1/q2".into(),
                ));
            }
        };
        lookup_fixed_index(self.section_bytes(kind)?, gram, self.doc_count as usize)
    }

    pub(crate) fn path_q3_cardinality(&self, gram: [u8; 3]) -> Result<usize, SearchError> {
        let cardinalities = if let Some(cached) = self.path_q3_cardinalities.get() {
            cached
        } else {
            let built = collect_q3_cardinalities(
                self.section_bytes(SECTION_PATH_Q3_SHARD_DIR)?,
                self.section_bytes(SECTION_PATH_Q3_DICTIONARY)?,
            )?;
            // Concurrent first path queries may race to initialize. Both values are derived from
            // the same immutable mmap, so losing the set race is harmless.
            let _ = self.path_q3_cardinalities.set(built);
            self.path_q3_cardinalities.get().ok_or_else(|| {
                SearchError::Format("vNext path q3 cardinality cache initialization failed".into())
            })?
        };
        let key = (u32::from(gram[0]) << 16) | (u32::from(gram[1]) << 8) | u32::from(gram[2]);
        Ok(cardinalities
            .binary_search_by_key(&key, |(stored, _)| *stored)
            .ok()
            .map_or(0, |index| cardinalities[index].1 as usize))
    }

    pub(crate) fn path_q3_posting(&self, gram: [u8; 3]) -> Result<VNextQ3Posting<'_>, SearchError> {
        lookup_q3(
            self.section_bytes(SECTION_PATH_Q3_SHARD_DIR)?,
            self.section_bytes(SECTION_PATH_Q3_DICTIONARY)?,
            self.section_bytes(SECTION_PATH_Q3_POSTINGS)?,
            gram,
            self.doc_count,
        )
    }

    pub fn logical_id(&self, doc_id: u16) -> Result<u64, SearchError> {
        let index = self.check_doc_id(doc_id)?;
        rd_u64(self.doc_soa()?, index * 8)
    }

    pub fn display_path(&self, doc_id: u16) -> Result<&str, SearchError> {
        let index = self.check_doc_id(doc_id)?;
        let (path_offsets_off, _, _) = self.doc_soa_offsets()?;
        let soa = self.doc_soa()?;
        let start = rd_u32(soa, path_offsets_off + index * 4)? as usize;
        let end = rd_u32(soa, path_offsets_off + (index + 1) * 4)? as usize;
        let blob = self.section_bytes(SECTION_PATH_BLOB)?;
        let data = blob
            .get(start..end)
            .ok_or_else(|| SearchError::Format("vNext path range out of bounds".into()))?;
        std::str::from_utf8(data)
            .map_err(|_| SearchError::Format("vNext path is not valid UTF-8".into()))
    }

    pub fn first_block(&self, doc_id: u16) -> Result<u16, SearchError> {
        let index = self.check_doc_id(doc_id)?;
        let (_, first_blocks_off, _) = self.doc_soa_offsets()?;
        rd_u16(self.doc_soa()?, first_blocks_off + index * 2)
    }

    pub fn document_block_count(&self, doc_id: u16) -> Result<u16, SearchError> {
        let index = self.check_doc_id(doc_id)?;
        let (_, _, block_counts_off) = self.doc_soa_offsets()?;
        rd_u16(self.doc_soa()?, block_counts_off + index * 2)
    }

    pub fn block(&self, block_id: u16) -> Result<VNextBlock, SearchError> {
        let index = block_id as usize;
        if index >= self.block_count as usize {
            return Err(SearchError::InvalidArgument(format!(
                "vNext block ID {block_id} out of range"
            )));
        }
        let table = self.section_bytes(SECTION_BLOCK_TABLE)?;
        let off = index * 12;
        Ok(VNextBlock {
            doc_id: rd_u16(table, off)?,
            content_offset: rd_u32(table, off + 4)?,
            content_len: rd_u32(table, off + 8)?,
        })
    }

    pub fn block_content(&self, block_id: u16) -> Result<&[u8], SearchError> {
        let block = self.block(block_id)?;
        self.block_content_from_meta(block)
    }

    pub(crate) fn block_content_from_meta(&self, block: VNextBlock) -> Result<&[u8], SearchError> {
        let blob = self.section_bytes(SECTION_CONTENT_BLOB)?;
        let start = block.content_offset as usize;
        let end = start
            .checked_add(block.content_len as usize)
            .ok_or_else(|| SearchError::Format("vNext block range overflow".into()))?;
        blob.get(start..end)
            .ok_or_else(|| SearchError::Format("vNext block content out of bounds".into()))
    }

    pub(crate) fn content_blob_len(&self) -> Result<usize, SearchError> {
        Ok(self.section_bytes(SECTION_CONTENT_BLOB)?.len())
    }

    pub fn normalized_content(&self, doc_id: u16) -> Result<&[u8], SearchError> {
        let count = self.document_block_count(doc_id)? as usize;
        if count == 0 {
            return Ok(&[]);
        }
        let first = self.first_block(doc_id)? as usize;
        let first_block = self.block(first as u16)?;
        let last_index = first
            .checked_add(count - 1)
            .ok_or_else(|| SearchError::Format("vNext document block range overflow".into()))?;
        let last_block = self.block(checked_u16(last_index, "last block")?)?;
        let start = first_block.content_offset as usize;
        let end = (last_block.content_offset as usize)
            .checked_add(last_block.content_len as usize)
            .ok_or_else(|| SearchError::Format("vNext document content range overflow".into()))?;
        self.section_bytes(SECTION_CONTENT_BLOB)?
            .get(start..end)
            .ok_or_else(|| SearchError::Format("vNext document content out of bounds".into()))
    }

    fn validate_structure(&self) -> Result<(), SearchError> {
        let soa = self.doc_soa()?;
        let expected_soa = self.doc_count as usize * 16 + 4;
        if soa.len() != expected_soa {
            return Err(SearchError::Format(format!(
                "bad vNext document SoA size: {} != {expected_soa}",
                soa.len()
            )));
        }
        let (path_offsets_off, first_blocks_off, block_counts_off) = self.doc_soa_offsets()?;
        let path_blob = self.section_bytes(SECTION_PATH_BLOB)?;
        let mut previous = 0usize;
        for index in 0..=self.doc_count as usize {
            let value = rd_u32(soa, path_offsets_off + index * 4)? as usize;
            if value < previous || value > path_blob.len() {
                return Err(SearchError::Format("invalid vNext path offsets".into()));
            }
            previous = value;
        }
        if previous != path_blob.len() {
            return Err(SearchError::Format(
                "vNext path blob has trailing bytes".into(),
            ));
        }
        for doc_id in 0..self.doc_count as usize {
            let start = rd_u32(soa, path_offsets_off + doc_id * 4)? as usize;
            let end = rd_u32(soa, path_offsets_off + (doc_id + 1) * 4)? as usize;
            std::str::from_utf8(&path_blob[start..end])
                .map_err(|_| SearchError::Format("vNext path is not valid UTF-8".into()))?;
        }

        let table = self.section_bytes(SECTION_BLOCK_TABLE)?;
        if table.len() != self.block_count as usize * 12 {
            return Err(SearchError::Format("bad vNext block table size".into()));
        }
        let content = self.section_bytes(SECTION_CONTENT_BLOB)?;
        let mut expected_first = 0usize;
        let mut expected_content_off = 0usize;
        for doc_id in 0..self.doc_count as usize {
            let first = rd_u16(soa, first_blocks_off + doc_id * 2)? as usize;
            let count = rd_u16(soa, block_counts_off + doc_id * 2)? as usize;
            if first != expected_first {
                return Err(SearchError::Format(
                    "vNext document block ranges are not contiguous".into(),
                ));
            }
            let end = first
                .checked_add(count)
                .ok_or_else(|| SearchError::Format("vNext block range overflow".into()))?;
            if end > self.block_count as usize {
                return Err(SearchError::Format(
                    "vNext document block range out of bounds".into(),
                ));
            }
            for block_id in first..end {
                let block = self.block(checked_u16(block_id, "block ID")?)?;
                if block.doc_id as usize != doc_id {
                    return Err(SearchError::Format(
                        "vNext block points at wrong document".into(),
                    ));
                }
                if block.content_len == 0 || block.content_len > self.block_size {
                    return Err(SearchError::Format("invalid vNext block length".into()));
                }
                if block.content_offset as usize != expected_content_off {
                    return Err(SearchError::Format(
                        "vNext content blocks are not contiguous".into(),
                    ));
                }
                expected_content_off = expected_content_off
                    .checked_add(block.content_len as usize)
                    .ok_or_else(|| SearchError::Format("vNext content offset overflow".into()))?;
                if expected_content_off > content.len() {
                    return Err(SearchError::Format(
                        "vNext content block out of bounds".into(),
                    ));
                }
            }
            expected_first = end;
        }
        if expected_first != self.block_count as usize || expected_content_off != content.len() {
            return Err(SearchError::Format(
                "vNext block/content coverage mismatch".into(),
            ));
        }
        validate_q3_sections(
            self.section_bytes(SECTION_Q3_SHARD_DIR)?,
            self.section_bytes(SECTION_Q3_DICTIONARY)?,
            self.section_bytes(SECTION_Q3_POSTINGS)?,
            self.block_count,
        )?;
        validate_fixed_index(
            self.section_bytes(SECTION_CONTENT_Q1)?,
            1,
            self.block_count as usize,
        )?;
        validate_fixed_index(
            self.section_bytes(SECTION_CONTENT_Q2)?,
            2,
            self.block_count as usize,
        )?;
        validate_fixed_index(
            self.section_bytes(SECTION_PATH_Q1)?,
            1,
            self.doc_count as usize,
        )?;
        validate_fixed_index(
            self.section_bytes(SECTION_PATH_Q2)?,
            2,
            self.doc_count as usize,
        )?;
        validate_q3_sections(
            self.section_bytes(SECTION_PATH_Q3_SHARD_DIR)?,
            self.section_bytes(SECTION_PATH_Q3_DICTIONARY)?,
            self.section_bytes(SECTION_PATH_Q3_POSTINGS)?,
            self.doc_count,
        )?;
        Ok(())
    }

    pub(crate) const fn all_docs_single_block(&self) -> bool {
        self.all_docs_single_block
    }

    pub(crate) fn single_block_sections(&self) -> Result<(&[u8], &[u8]), SearchError> {
        if !self.all_docs_single_block {
            return Err(SearchError::InvalidArgument(
                "vNext segment is not one-block-per-document".into(),
            ));
        }
        Ok((
            self.section_bytes(SECTION_BLOCK_TABLE)?,
            self.section_bytes(SECTION_CONTENT_BLOB)?,
        ))
    }

    fn compute_all_docs_single_block(&self) -> Result<bool, SearchError> {
        if self.doc_count == 0 || self.doc_count != self.block_count {
            return Ok(false);
        }
        let soa = self.doc_soa()?;
        let (_, first_blocks_off, block_counts_off) = self.doc_soa_offsets()?;
        for doc_id in 0..self.doc_count as usize {
            if rd_u16(soa, block_counts_off + doc_id * 2)? != 1
                || usize::from(rd_u16(soa, first_blocks_off + doc_id * 2)?) != doc_id
            {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn check_doc_id(&self, doc_id: u16) -> Result<usize, SearchError> {
        let index = doc_id as usize;
        if index >= self.doc_count as usize {
            return Err(SearchError::InvalidArgument(format!(
                "vNext document ID {doc_id} out of range"
            )));
        }
        Ok(index)
    }

    fn doc_soa(&self) -> Result<&[u8], SearchError> {
        self.section_bytes(SECTION_DOC_SOA)
    }

    fn doc_soa_offsets(&self) -> Result<(usize, usize, usize), SearchError> {
        let docs = self.doc_count as usize;
        let path_offsets_off = docs
            .checked_mul(8)
            .ok_or_else(|| SearchError::Format("vNext SoA offset overflow".into()))?;
        let first_blocks_off = path_offsets_off
            .checked_add((docs + 1) * 4)
            .ok_or_else(|| SearchError::Format("vNext SoA offset overflow".into()))?;
        let block_counts_off = first_blocks_off
            .checked_add(docs * 2)
            .ok_or_else(|| SearchError::Format("vNext SoA offset overflow".into()))?;
        Ok((path_offsets_off, first_blocks_off, block_counts_off))
    }

    fn section_bytes(&self, kind: u32) -> Result<&[u8], SearchError> {
        let desc = self
            .sections
            .iter()
            .find(|desc| desc.kind == kind)
            .ok_or_else(|| SearchError::Format(format!("missing vNext section {kind}")))?;
        checked_range(
            self.mapped.as_slice(),
            desc.off,
            desc.size,
            self.mapped.as_slice().len() as u64,
        )
    }
}

fn encode_doc_soa(
    logical_ids: &[u64],
    path_offsets: &[u32],
    first_blocks: &[u16],
    block_counts: &[u16],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(logical_ids.len() * 16 + 4);
    for value in logical_ids {
        out.extend_from_slice(&value.to_le_bytes());
    }
    for value in path_offsets {
        out.extend_from_slice(&value.to_le_bytes());
    }
    for value in first_blocks {
        out.extend_from_slice(&value.to_le_bytes());
    }
    for value in block_counts {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

fn encode_block_table(blocks: &[VNextBlock]) -> Vec<u8> {
    let mut out = Vec::with_capacity(blocks.len() * 12);
    for block in blocks {
        out.extend_from_slice(&block.doc_id.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&block.content_offset.to_le_bytes());
        out.extend_from_slice(&block.content_len.to_le_bytes());
    }
    out
}

fn validate_non_overlapping(
    sections: &[SectionDesc; SECTION_COUNT],
    footer_off: u64,
) -> Result<(), SearchError> {
    let data_start = align8((HEADER_SIZE + SECTION_ENTRY_SIZE * SECTION_COUNT) as u64);
    let mut previous_end = data_start;
    for desc in sections {
        if desc.off < data_start || desc.off % 8 != 0 || desc.off < previous_end {
            return Err(SearchError::Format("invalid vNext section layout".into()));
        }
        previous_end = desc
            .off
            .checked_add(desc.size)
            .ok_or_else(|| SearchError::Format("vNext section range overflow".into()))?;
        if previous_end > footer_off {
            return Err(SearchError::Format("vNext section overlaps footer".into()));
        }
    }
    Ok(())
}

fn checked_range(bytes: &[u8], off: u64, size: u64, limit: u64) -> Result<&[u8], SearchError> {
    let end = off
        .checked_add(size)
        .ok_or_else(|| SearchError::Format("vNext section range overflow".into()))?;
    if end > limit || end > bytes.len() as u64 {
        return Err(SearchError::Format("vNext section out of bounds".into()));
    }
    let start = usize::try_from(off)
        .map_err(|_| SearchError::Format("vNext section offset too large".into()))?;
    let end = usize::try_from(end)
        .map_err(|_| SearchError::Format("vNext section end too large".into()))?;
    Ok(&bytes[start..end])
}

fn checked_u16(value: usize, label: &str) -> Result<u16, SearchError> {
    u16::try_from(value)
        .map_err(|_| SearchError::InvalidArgument(format!("vNext {label} exceeds u16")))
}

fn checked_u32(value: usize, label: &str) -> Result<u32, SearchError> {
    u32::try_from(value)
        .map_err(|_| SearchError::InvalidArgument(format!("vNext {label} exceeds u32")))
}

fn compute_section_checksums(
    sections: &[Vec<u8>; SECTION_COUNT],
    cpu_budget: usize,
) -> [u64; SECTION_COUNT] {
    let workers = cpu_budget.clamp(1, 4).min(SECTION_COUNT);
    if workers <= 1 {
        return std::array::from_fn(|index| {
            if index == 3 {
                0
            } else {
                fnv1a(&sections[index])
            }
        });
    }

    let partials = std::thread::scope(|scope| {
        let handles = (0..workers)
            .map(|worker| {
                scope.spawn(move || {
                    (worker..SECTION_COUNT)
                        .step_by(workers)
                        .map(|index| {
                            let checksum = if index == 3 {
                                0
                            } else {
                                fnv1a(&sections[index])
                            };
                            (index, checksum)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("vNext checksum worker panicked"))
            .collect::<Vec<_>>()
    });
    let mut checksums = [0u64; SECTION_COUNT];
    for partial in partials {
        for (index, checksum) in partial {
            checksums[index] = checksum;
        }
    }
    checksums
}

const XXH64_PRIME1: u64 = 11_400_714_785_074_694_791;
const XXH64_PRIME2: u64 = 14_029_467_366_897_019_727;
const XXH64_PRIME3: u64 = 1_609_587_929_392_839_161;
const XXH64_PRIME4: u64 = 9_650_029_242_287_828_579;
const XXH64_PRIME5: u64 = 2_870_177_450_012_600_261;

#[derive(Clone)]
struct Xxh64State {
    total_len: u64,
    v1: u64,
    v2: u64,
    v3: u64,
    v4: u64,
    mem: [u8; 32],
    mem_len: usize,
}

impl Xxh64State {
    fn new() -> Self {
        Self {
            total_len: 0,
            v1: XXH64_PRIME1.wrapping_add(XXH64_PRIME2),
            v2: XXH64_PRIME2,
            v3: 0,
            v4: 0u64.wrapping_sub(XXH64_PRIME1),
            mem: [0; 32],
            mem_len: 0,
        }
    }

    fn update(&mut self, mut bytes: &[u8]) {
        self.total_len = self.total_len.wrapping_add(bytes.len() as u64);

        if self.mem_len + bytes.len() < 32 {
            self.mem[self.mem_len..self.mem_len + bytes.len()].copy_from_slice(bytes);
            self.mem_len += bytes.len();
            return;
        }

        if self.mem_len != 0 {
            let fill = 32 - self.mem_len;
            self.mem[self.mem_len..32].copy_from_slice(&bytes[..fill]);
            let block = self.mem;
            self.consume_block(&block);
            self.mem_len = 0;
            bytes = &bytes[fill..];
        }

        while bytes.len() >= 32 {
            self.consume_block(&bytes[..32]);
            bytes = &bytes[32..];
        }

        if !bytes.is_empty() {
            self.mem[..bytes.len()].copy_from_slice(bytes);
            self.mem_len = bytes.len();
        }
    }

    fn consume_block(&mut self, block: &[u8]) {
        self.v1 = xxh64_round(self.v1, read_u64_le(block, 0));
        self.v2 = xxh64_round(self.v2, read_u64_le(block, 8));
        self.v3 = xxh64_round(self.v3, read_u64_le(block, 16));
        self.v4 = xxh64_round(self.v4, read_u64_le(block, 24));
    }

    fn digest(&self) -> u64 {
        let mut hash = if self.total_len >= 32 {
            let mut hash = self
                .v1
                .rotate_left(1)
                .wrapping_add(self.v2.rotate_left(7))
                .wrapping_add(self.v3.rotate_left(12))
                .wrapping_add(self.v4.rotate_left(18));
            hash = xxh64_merge_round(hash, self.v1);
            hash = xxh64_merge_round(hash, self.v2);
            hash = xxh64_merge_round(hash, self.v3);
            xxh64_merge_round(hash, self.v4)
        } else {
            XXH64_PRIME5
        };

        hash = hash.wrapping_add(self.total_len);
        let mut pos = 0usize;
        let tail = &self.mem[..self.mem_len];
        while pos + 8 <= tail.len() {
            let lane = xxh64_round(0, read_u64_le(tail, pos));
            hash ^= lane;
            hash = hash
                .rotate_left(27)
                .wrapping_mul(XXH64_PRIME1)
                .wrapping_add(XXH64_PRIME4);
            pos += 8;
        }
        if pos + 4 <= tail.len() {
            hash ^= u64::from(read_u32_le(tail, pos)).wrapping_mul(XXH64_PRIME1);
            hash = hash
                .rotate_left(23)
                .wrapping_mul(XXH64_PRIME2)
                .wrapping_add(XXH64_PRIME3);
            pos += 4;
        }
        while pos < tail.len() {
            hash ^= u64::from(tail[pos]).wrapping_mul(XXH64_PRIME5);
            hash = hash.rotate_left(11).wrapping_mul(XXH64_PRIME1);
            pos += 1;
        }
        hash ^= hash >> 33;
        hash = hash.wrapping_mul(XXH64_PRIME2);
        hash ^= hash >> 29;
        hash = hash.wrapping_mul(XXH64_PRIME3);
        hash ^ (hash >> 32)
    }
}

fn read_u64_le(bytes: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(bytes[off..off + 8].try_into().expect("xxh64 lane"))
}

fn read_u32_le(bytes: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(bytes[off..off + 4].try_into().expect("xxh64 lane"))
}

fn xxh64_round(acc: u64, lane: u64) -> u64 {
    acc.wrapping_add(lane.wrapping_mul(XXH64_PRIME2))
        .rotate_left(31)
        .wrapping_mul(XXH64_PRIME1)
}

fn xxh64_merge_round(acc: u64, lane: u64) -> u64 {
    (acc ^ xxh64_round(0, lane))
        .wrapping_mul(XXH64_PRIME1)
        .wrapping_add(XXH64_PRIME4)
}

fn xxh64(bytes: &[u8]) -> u64 {
    let mut state = Xxh64State::new();
    state.update(bytes);
    state.digest()
}

fn write_whole_hashed(
    writer: &mut impl Write,
    checksum: &mut Xxh64State,
    bytes: &[u8],
) -> Result<(), SearchError> {
    writer.write_all(bytes)?;
    checksum.update(bytes);
    Ok(())
}

#[cfg(unix)]
#[inline(never)]
fn write_all_vectored_exact(
    writer: &mut impl Write,
    buffers: &mut [IoSlice<'_>],
) -> std::io::Result<()> {
    let mut remaining = buffers;
    while !remaining.is_empty() {
        match writer.write_vectored(remaining) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "failed to write vNext vectored content",
                ));
            }
            Ok(written) => IoSlice::advance_slices(&mut remaining, written),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(unix)]
#[inline(never)]
fn write_content_blob_hashed(
    writer: &mut BufWriter<File>,
    checksum: &mut Xxh64State,
    docs: &[VNextDocumentInput],
) -> Result<(), SearchError> {
    writer.flush()?;
    let file = writer.get_mut();
    let mut buffers = Vec::with_capacity(CONTENT_WRITEV_BATCH_DOCS);
    for chunk in docs.chunks(CONTENT_WRITEV_BATCH_DOCS) {
        buffers.clear();
        for doc in chunk {
            checksum.update(&doc.normalized_content);
            if !doc.normalized_content.is_empty() {
                buffers.push(IoSlice::new(&doc.normalized_content));
            }
        }
        if !buffers.is_empty() {
            write_all_vectored_exact(file, &mut buffers)?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
#[inline(never)]
fn write_content_blob_hashed(
    writer: &mut BufWriter<File>,
    checksum: &mut Xxh64State,
    docs: &[VNextDocumentInput],
) -> Result<(), SearchError> {
    for doc in docs {
        write_whole_hashed(writer, checksum, &doc.normalized_content)?;
    }
    Ok(())
}

fn write_zero_padding_whole(
    writer: &mut impl Write,
    checksum: &mut Xxh64State,
    written: &mut u64,
    target: u64,
) -> Result<(), SearchError> {
    let padding = target.checked_sub(*written).ok_or_else(|| {
        SearchError::Format("vNext streaming writer offset moved backwards".into())
    })?;
    if padding > 7 {
        return Err(SearchError::Format(
            "vNext streaming writer encountered unexpected alignment gap".into(),
        ));
    }
    const ZEROES: [u8; 8] = [0; 8];
    let padding_len = usize::try_from(padding)
        .map_err(|_| SearchError::Format("vNext padding is too large".into()))?;
    write_whole_hashed(writer, checksum, &ZEROES[..padding_len])?;
    *written = target;
    Ok(())
}

const fn align8(value: u64) -> u64 {
    (value + 7) & !7
}

fn rd_u16(bytes: &[u8], off: usize) -> Result<u16, SearchError> {
    let data = bytes
        .get(off..off + 2)
        .ok_or_else(|| SearchError::Format("vNext u16 read out of bounds".into()))?;
    Ok(u16::from_le_bytes([data[0], data[1]]))
}

fn rd_u32(bytes: &[u8], off: usize) -> Result<u32, SearchError> {
    let data = bytes
        .get(off..off + 4)
        .ok_or_else(|| SearchError::Format("vNext u32 read out of bounds".into()))?;
    Ok(u32::from_le_bytes(
        data.try_into().expect("fixed u32 slice"),
    ))
}

fn rd_u64(bytes: &[u8], off: usize) -> Result<u64, SearchError> {
    let data = bytes
        .get(off..off + 8)
        .ok_or_else(|| SearchError::Format("vNext u64 read out of bounds".into()))?;
    Ok(u64::from_le_bytes(
        data.try_into().expect("fixed u64 slice"),
    ))
}

fn put_u32_at(bytes: &mut [u8], off: usize, value: u32) -> Result<(), SearchError> {
    let out = bytes
        .get_mut(off..off + 4)
        .ok_or_else(|| SearchError::Format("vNext u32 write out of bounds".into()))?;
    out.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn put_u64_at(bytes: &mut [u8], off: usize, value: u64) -> Result<(), SearchError> {
    let out = bytes
        .get_mut(off..off + 8)
        .ok_or_else(|| SearchError::Format("vNext u64 write out of bounds".into()))?;
    out.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

#[cfg(test)]
mod worker_budget_tests {
    use super::q3_worker_budget;

    #[test]
    fn q3_worker_budget_never_oversubscribes_segment_index_workers() {
        for cpus in 1usize..=64 {
            let q3 = q3_worker_budget(cpus, None);
            assert!((1..=4).contains(&q3));
            if cpus <= 2 {
                assert_eq!(q3, 1);
            } else {
                assert!(q3 + 2 <= cpus);
            }
        }
    }

    #[test]
    fn q3_worker_budget_clamps_manual_override_to_safe_limit() {
        assert_eq!(q3_worker_budget(8, Some(1)), 1);
        assert_eq!(q3_worker_budget(8, Some(99)), 4);
        assert_eq!(q3_worker_budget(4, Some(99)), 2);
        assert_eq!(q3_worker_budget(2, Some(99)), 1);
        assert_eq!(q3_worker_budget(8, Some(0)), 1);
    }
}

#[cfg(test)]
mod format_v6_tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn vectored_writer_handles_partial_writes_without_reordering_bytes() {
        #[derive(Default)]
        struct PartialWriter {
            bytes: Vec<u8>,
        }
        impl Write for PartialWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                let take = bytes.len().min(5);
                self.bytes.extend_from_slice(&bytes[..take]);
                Ok(take)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
            fn write_vectored(&mut self, buffers: &[IoSlice<'_>]) -> std::io::Result<usize> {
                let mut remaining = 7usize;
                let mut written = 0usize;
                for buffer in buffers {
                    if remaining == 0 {
                        break;
                    }
                    let take = buffer.len().min(remaining);
                    self.bytes.extend_from_slice(&buffer[..take]);
                    written += take;
                    remaining -= take;
                    if take < buffer.len() {
                        break;
                    }
                }
                Ok(written)
            }
        }
        let parts = [
            b"alpha".as_slice(),
            b"-beta-".as_slice(),
            b"gamma".as_slice(),
        ];
        let mut buffers = parts
            .iter()
            .map(|part| IoSlice::new(part))
            .collect::<Vec<_>>();
        let mut writer = PartialWriter::default();
        write_all_vectored_exact(&mut writer, &mut buffers).unwrap();
        assert_eq!(writer.bytes, b"alpha-beta-gamma");
    }
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_file(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "personalrag-vnext-{label}-{}-{nonce}.prseg2",
            std::process::id()
        ))
    }

    fn sample_docs() -> Vec<VNextDocumentInput> {
        vec![
            VNextDocumentInput::new(
                11,
                "alpha/a.txt",
                b"header alpha abcabcabcabc periodic tail".to_vec(),
            ),
            VNextDocumentInput::new(
                22,
                "beta/b.bin",
                (0u8..=127).cycle().take(2_000).collect::<Vec<_>>(),
            ),
        ]
    }

    #[test]
    fn v6_content_uses_fast_whole_file_checksum_without_standalone_content_hash() {
        let path = temp_file("v6-content-checksum");
        let corrupt = temp_file("v6-content-corrupt");
        let docs = sample_docs();
        write_vnext_segment(&path, &docs).unwrap();

        let mut bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..8], MAGIC);
        assert_eq!(rd_u32(&bytes, HDR_VERSION).unwrap(), VERSION);
        let content_entry = HEADER_SIZE + 3 * SECTION_ENTRY_SIZE;
        assert_eq!(rd_u32(&bytes, content_entry).unwrap(), SECTION_CONTENT_BLOB);
        assert_eq!(rd_u64(&bytes, content_entry + 24).unwrap(), 0);
        VNextSegmentReader::open(&path).unwrap();

        let content_off = rd_u64(&bytes, content_entry + 8).unwrap() as usize;
        bytes[content_off] ^= 0x5a;
        std::fs::write(&corrupt, &bytes).unwrap();
        let error = match VNextSegmentReader::open(&corrupt) {
            Ok(_) => panic!("corrupted v6 content unexpectedly opened"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("file checksum mismatch"));

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(corrupt);
    }

    #[test]
    fn xxh64_matches_reference_vectors_and_streaming_boundaries() {
        let vectors: Vec<(Vec<u8>, u64)> = vec![
            (Vec::new(), 0xef46_db37_51d8_e999),
            (b"a".to_vec(), 0xd24e_c4f1_a98c_6e5b),
            (b"abc".to_vec(), 0x44bc_2cf5_ad77_0999),
            ((0u8..=255).collect(), 0x1fac_be84_06cd_904b),
            (
                (0..10_000)
                    .map(|index| ((index * 37 + 11) & 0xff) as u8)
                    .collect(),
                0x5e9f_4f7f_2b4b_2cfc,
            ),
        ];
        for (bytes, expected) in vectors {
            assert_eq!(xxh64(&bytes), expected);
            for chunk_size in [1usize, 3, 7, 31, 32, 33, 127, 1024] {
                let mut state = Xxh64State::new();
                for chunk in bytes.chunks(chunk_size) {
                    state.update(chunk);
                }
                assert_eq!(state.digest(), expected, "chunk_size={chunk_size}");
            }
        }
    }

    #[test]
    fn reader_accepts_v5_segment_with_fnv_whole_file_checksum() {
        let v6_path = temp_file("v6-to-v5-source");
        let v5_path = temp_file("v5-compat");
        let docs = sample_docs();
        write_vnext_segment(&v6_path, &docs).unwrap();
        let mut bytes = std::fs::read(&v6_path).unwrap();

        bytes[..8].copy_from_slice(MAGIC_V5);
        put_u32_at(&mut bytes, HDR_VERSION, VERSION_V5).unwrap();
        let footer = rd_u64(&bytes, HDR_FOOTER_OFF).unwrap() as usize;
        bytes[footer..footer + 8].copy_from_slice(FOOTER_MAGIC_V5);
        put_u32_at(&mut bytes, footer + 8, VERSION_V5).unwrap();
        let whole_checksum = fnv1a(&bytes[..footer]);
        put_u64_at(&mut bytes, footer + 24, whole_checksum).unwrap();
        std::fs::write(&v5_path, &bytes).unwrap();

        let reader = VNextSegmentReader::open(&v5_path).unwrap();
        assert_eq!(reader.logical_id(0).unwrap(), 11);
        assert_eq!(reader.display_path(1).unwrap(), "beta/b.bin");
        assert_eq!(
            reader.normalized_content(0).unwrap(),
            docs[0].normalized_content
        );
        assert_eq!(
            reader.normalized_content(1).unwrap(),
            docs[1].normalized_content
        );

        let _ = std::fs::remove_file(v6_path);
        let _ = std::fs::remove_file(v5_path);
    }

    #[test]
    fn reader_accepts_v4_segment_with_legacy_content_checksum() {
        let v6_path = temp_file("v6-to-v4-source");
        let v4_path = temp_file("v4-compat");
        let docs = sample_docs();
        write_vnext_segment(&v6_path, &docs).unwrap();
        let mut bytes = std::fs::read(&v6_path).unwrap();

        bytes[..8].copy_from_slice(MAGIC_V4);
        put_u32_at(&mut bytes, HDR_VERSION, VERSION_V4).unwrap();
        let content_entry = HEADER_SIZE + 3 * SECTION_ENTRY_SIZE;
        let content_off = rd_u64(&bytes, content_entry + 8).unwrap() as usize;
        let content_size = rd_u64(&bytes, content_entry + 16).unwrap() as usize;
        let content_checksum = fnv1a(&bytes[content_off..content_off + content_size]);
        put_u64_at(&mut bytes, content_entry + 24, content_checksum).unwrap();

        let footer = rd_u64(&bytes, HDR_FOOTER_OFF).unwrap() as usize;
        bytes[footer..footer + 8].copy_from_slice(FOOTER_MAGIC_V4);
        put_u32_at(&mut bytes, footer + 8, VERSION_V4).unwrap();
        let whole_checksum = fnv1a(&bytes[..footer]);
        put_u64_at(&mut bytes, footer + 24, whole_checksum).unwrap();
        std::fs::write(&v4_path, &bytes).unwrap();

        let reader = VNextSegmentReader::open(&v4_path).unwrap();
        assert_eq!(reader.logical_id(0).unwrap(), 11);
        assert_eq!(reader.display_path(1).unwrap(), "beta/b.bin");
        assert_eq!(
            reader.normalized_content(0).unwrap(),
            docs[0].normalized_content
        );
        assert_eq!(
            reader.normalized_content(1).unwrap(),
            docs[1].normalized_content
        );

        let _ = std::fs::remove_file(v6_path);
        let _ = std::fs::remove_file(v4_path);
    }
}
