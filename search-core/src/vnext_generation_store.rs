use std::collections::HashSet;
#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::format::{SearchError, fnv1a};
use crate::integration::UpdatePlan;
use crate::types::{Generation, LogicalDocId};
use crate::vnext_generation::{
    VNextGenerationIndex, VNextGenerationLayerKind, VNextGenerationLayerSpec,
};
use crate::vnext_segment::{
    VNextDocumentInput, VNextSegmentReader, write_vnext_segment_with_cpu_budget,
};

const MANIFEST_MAGIC: &str = "PRVGM001";
const CURRENT_MAGIC: &str = "PRVCU001";
const TOMBSTONE_MAGIC: &[u8; 8] = b"PRVTMB01";
const TOMBSTONE_FOOTER_BYTES: usize = 8;
const PUBLISH_BLOCK_SIZE: usize = 8 * 1024;
const MAX_LOCAL_ITEMS: usize = u16::MAX as usize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VNextDurableGenerationReport {
    pub generation: Generation,
    pub live_docs: usize,
    pub layer_count: usize,
    pub delta_count: usize,
    pub segment_count: usize,
    pub tombstone_events: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VNextDurableCompactionReport {
    pub source_generation: Generation,
    pub compacted_generation: Generation,
    pub live_docs: usize,
    pub source_layer_count: usize,
    pub source_segment_count: usize,
    pub source_tombstone_events: usize,
    pub compacted_segment_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VNextDurableGcReport {
    pub current_generation: Generation,
    pub reachable_component_dirs: usize,
    pub removed_component_dirs: usize,
    pub removed_manifest_files: usize,
    pub reclaimed_bytes: u64,
    pub deferred_by_grace: usize,
    pub deferred_in_use: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StoredLayerKind {
    Base,
    Delta,
}

impl StoredLayerKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Delta => "delta",
        }
    }

    fn parse(value: &str) -> Result<Self, SearchError> {
        match value {
            "base" => Ok(Self::Base),
            "delta" => Ok(Self::Delta),
            _ => Err(SearchError::Format(format!(
                "bad vNext durable layer kind {value}"
            ))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StoredLayer {
    kind: StoredLayerKind,
    generation: Generation,
    segment_files: Vec<String>,
    tombstone_file: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StoredManifest {
    generation: Generation,
    layers: Vec<StoredLayer>,
}

pub fn initialize_vnext_generation_store(
    root: impl AsRef<Path>,
    documents: &[VNextDocumentInput],
    segment_docs: usize,
) -> Result<VNextDurableGenerationReport, SearchError> {
    let root = root.as_ref();
    validate_segment_docs(segment_docs)?;
    if root.join("CURRENT").exists() {
        return Err(SearchError::InvalidArgument(
            "vNext durable generation store is already initialized".into(),
        ));
    }
    validate_document_inputs(documents)?;

    fs::create_dir_all(root)?;
    fs::create_dir_all(root.join("components"))?;
    fs::create_dir_all(root.join("generations"))?;

    let component_relative = component_relative(StoredLayerKind::Base, 0);
    let segment_files = publish_component(
        root,
        StoredLayerKind::Base,
        &component_relative,
        documents,
        &[],
        segment_docs,
    )?;
    finalize_initial_generation(root, segment_files, documents.len())
}

/// Initialize a durable vNext base generation from a deterministic stream of already-normalized
/// documents. Only a bounded number of documents are retained at once and completed segment
/// writes are parallelized, making this suitable for teeing the Perf12 hydration stream without
/// rereading or rematerializing the finished Perf12 index.
pub fn initialize_vnext_generation_store_streaming<I>(
    root: impl AsRef<Path>,
    documents: I,
    segment_docs: usize,
    workers: usize,
) -> Result<VNextDurableGenerationReport, SearchError>
where
    I: IntoIterator<Item = VNextDocumentInput>,
{
    let root = root.as_ref();
    validate_segment_docs(segment_docs)?;
    if root.join("CURRENT").exists() {
        return Err(SearchError::InvalidArgument(
            "vNext durable generation store is already initialized".into(),
        ));
    }
    fs::create_dir_all(root)?;
    fs::create_dir_all(root.join("components"))?;
    fs::create_dir_all(root.join("generations"))?;

    let component_relative = component_relative(StoredLayerKind::Base, 0);
    let (segment_files, live_docs) = publish_component_streaming(
        root,
        &component_relative,
        documents.into_iter(),
        segment_docs,
        workers,
    )?;
    finalize_initial_generation(root, segment_files, live_docs)
}

fn finalize_initial_generation(
    root: &Path,
    segment_files: Vec<String>,
    expected_live_docs: usize,
) -> Result<VNextDurableGenerationReport, SearchError> {
    let manifest = StoredManifest {
        generation: 0,
        layers: vec![StoredLayer {
            kind: StoredLayerKind::Base,
            generation: 0,
            segment_files,
            tombstone_file: None,
        }],
    };
    validate_stored_manifest(&manifest)?;

    let manifest_relative = manifest_relative(0, "base");
    publish_manifest(root, &manifest_relative, &manifest)?;
    let persisted = read_manifest(&safe_join(root, &manifest_relative)?)?;
    let index = open_manifest_index(root, &persisted)?;
    if index.live_docs() != expected_live_docs {
        return Err(SearchError::Format(
            "vNext initial durable live-doc count mismatch".into(),
        ));
    }

    publish_current(root, 0, &manifest_relative, index.live_docs())?;
    verify_current_pointer(root, 0, &manifest_relative)?;
    Ok(report_from_index(&index))
}

pub fn publish_vnext_incremental_generation(
    root: impl AsRef<Path>,
    plan: &UpdatePlan,
    segment_docs: usize,
) -> Result<VNextDurableGenerationReport, SearchError> {
    let root = root.as_ref();
    validate_segment_docs(segment_docs)?;
    if plan.upserts.is_empty() && plan.tombstones.is_empty() {
        return Err(SearchError::InvalidArgument(
            "vNext incremental generation must contain an upsert or tombstone".into(),
        ));
    }

    let current = read_current_record(root)?;
    let current_generation = current.generation;
    let current_manifest_relative = current.manifest_relative.clone();
    if plan.base_generation != current_generation {
        return Err(SearchError::InvalidArgument(
            "vNext incremental plan base generation does not match CURRENT".into(),
        ));
    }
    let expected_next = current_generation
        .checked_add(1)
        .ok_or_else(|| SearchError::InvalidArgument("vNext generation overflow".into()))?;
    if plan.next_generation != expected_next {
        return Err(SearchError::InvalidArgument(
            "vNext incremental plan next generation is invalid".into(),
        ));
    }
    validate_update_plan_payload(plan)?;

    let current_manifest = read_manifest(&safe_join(root, &current_manifest_relative)?)?;
    if current_manifest.generation != current_generation {
        return Err(SearchError::Format(
            "vNext CURRENT/manifest generation mismatch".into(),
        ));
    }
    validate_stored_manifest(&current_manifest)?;

    // New-format CURRENT stores the live count so a small delta does not need to reopen every
    // immutable base segment merely to validate UpdatePlan accounting. Older stores remain
    // readable; they pay the legacy full-open cost once until the next publish upgrades CURRENT.
    let current_live_docs = match current.live_docs {
        Some(value) => value,
        None => open_manifest_index(root, &current_manifest)?.live_docs(),
    };

    let documents = plan
        .upserts
        .iter()
        .map(|upsert| {
            VNextDocumentInput::new(
                upsert.logical_id,
                upsert.document.display_path.clone(),
                upsert.document.normalized_content.clone(),
            )
        })
        .collect::<Vec<_>>();

    let component_relative = component_relative(StoredLayerKind::Delta, plan.next_generation);
    let segment_files = publish_component(
        root,
        StoredLayerKind::Delta,
        &component_relative,
        &documents,
        &plan.tombstones,
        segment_docs,
    )?;
    let tombstone_file = format!("{component_relative}/tombstones.bin");

    let old_layers = current_manifest.layers.clone();
    let mut next_manifest = current_manifest;
    next_manifest.generation = plan.next_generation;
    next_manifest.layers.push(StoredLayer {
        kind: StoredLayerKind::Delta,
        generation: plan.next_generation,
        segment_files,
        tombstone_file: Some(tombstone_file),
    });
    validate_stored_manifest(&next_manifest)?;

    let manifest_relative = manifest_relative(plan.next_generation, "delta");
    publish_manifest(root, &manifest_relative, &next_manifest)?;

    // Localized pre-CURRENT validation: existing manifest layers are immutable and already
    // published, so validate that the persisted manifest carries them byte-for-byte, then fully
    // open only the newly written delta segments/tombstone. This keeps 1-document publish latency
    // independent of the size of the base generation while preserving fail-closed validation of
    // every newly visible byte.
    let persisted = read_manifest(&safe_join(root, &manifest_relative)?)?;
    if persisted.layers.len() != old_layers.len() + 1
        || persisted.layers[..old_layers.len()] != old_layers[..]
    {
        return Err(SearchError::Format(
            "vNext incremental manifest changed immutable prior layers".into(),
        ));
    }
    let new_layer = persisted
        .layers
        .last()
        .ok_or_else(|| SearchError::Format("vNext incremental manifest missing delta".into()))?;
    validate_new_delta_layer(root, new_layer, plan)?;

    let expected_live_docs = expected_live_docs_after(current_live_docs, plan)?;
    if expected_live_docs != plan.live_docs_after {
        return Err(SearchError::Format(
            "vNext persisted generation live-doc count mismatch".into(),
        ));
    }

    // A concurrent publisher must not advance CURRENT while this delta is being validated.
    verify_current_pointer(root, current_generation, &current_manifest_relative)?;
    publish_current(
        root,
        plan.next_generation,
        &manifest_relative,
        plan.live_docs_after,
    )?;
    verify_current_pointer(root, plan.next_generation, &manifest_relative)?;
    report_from_manifest_metadata(root, &persisted, plan.live_docs_after)
}

fn validate_new_delta_layer(
    root: &Path,
    layer: &StoredLayer,
    plan: &UpdatePlan,
) -> Result<(), SearchError> {
    if layer.kind != StoredLayerKind::Delta || layer.generation != plan.next_generation {
        return Err(SearchError::Format(
            "vNext persisted incremental layer metadata mismatch".into(),
        ));
    }
    let tombstone_relative = layer.tombstone_file.as_ref().ok_or_else(|| {
        SearchError::Format("vNext persisted delta missing tombstone file".into())
    })?;
    let persisted_tombstones = read_tombstones(&safe_join(root, tombstone_relative)?)?;
    if persisted_tombstones != plan.tombstones {
        return Err(SearchError::Format(
            "vNext persisted delta tombstones do not match update plan".into(),
        ));
    }

    let mut cursor = 0usize;
    for relative in &layer.segment_files {
        let reader = VNextSegmentReader::open(&safe_join(root, relative)?)?;
        for physical in 0..reader.doc_count() as usize {
            let expected = plan.upserts.get(cursor).ok_or_else(|| {
                SearchError::Format("vNext persisted delta has extra documents".into())
            })?;
            let doc_id = u16::try_from(physical)
                .map_err(|_| SearchError::Format("vNext delta local ID overflow".into()))?;
            if reader.logical_id(doc_id)? != expected.logical_id
                || reader.display_path(doc_id)? != expected.document.display_path
                || reader.normalized_content(doc_id)? != expected.document.normalized_content
            {
                return Err(SearchError::Format(
                    "vNext persisted delta document does not match update plan".into(),
                ));
            }
            cursor += 1;
        }
    }
    if cursor != plan.upserts.len() {
        return Err(SearchError::Format(
            "vNext persisted delta is missing update-plan documents".into(),
        ));
    }
    Ok(())
}

fn expected_live_docs_after(
    current_live_docs: usize,
    plan: &UpdatePlan,
) -> Result<usize, SearchError> {
    let upsert_ids = plan
        .upserts
        .iter()
        .map(|upsert| upsert.logical_id)
        .collect::<HashSet<_>>();
    let inserts = plan
        .upserts
        .iter()
        .filter(|upsert| upsert.is_insert)
        .count();
    let pure_deletes = plan
        .tombstones
        .iter()
        .filter(|logical_id| !upsert_ids.contains(logical_id))
        .count();
    current_live_docs
        .checked_add(inserts)
        .and_then(|value| value.checked_sub(pure_deletes))
        .ok_or_else(|| SearchError::Format("vNext incremental live-doc accounting overflow".into()))
}

fn report_from_manifest_metadata(
    root: &Path,
    manifest: &StoredManifest,
    live_docs: usize,
) -> Result<VNextDurableGenerationReport, SearchError> {
    let segment_count = manifest
        .layers
        .iter()
        .try_fold(0usize, |total, layer| {
            total.checked_add(layer.segment_files.len())
        })
        .ok_or_else(|| SearchError::Format("vNext segment count overflow".into()))?;
    let mut tombstone_events = 0usize;
    for layer in &manifest.layers {
        if let Some(relative) = &layer.tombstone_file {
            tombstone_events = tombstone_events
                .checked_add(read_tombstones(&safe_join(root, relative)?)?.len())
                .ok_or_else(|| SearchError::Format("vNext tombstone count overflow".into()))?;
        }
    }
    Ok(VNextDurableGenerationReport {
        generation: manifest.generation,
        live_docs,
        layer_count: manifest.layers.len(),
        delta_count: manifest.layers.len().saturating_sub(1),
        segment_count,
        tombstone_events,
    })
}

pub fn compact_vnext_generation_store(
    root: impl AsRef<Path>,
    segment_docs: usize,
) -> Result<VNextDurableCompactionReport, SearchError> {
    let root = root.as_ref();
    validate_segment_docs(segment_docs)?;

    let (source_generation, source_manifest_relative) = read_current(root)?;
    let source_manifest = read_manifest(&safe_join(root, &source_manifest_relative)?)?;
    if source_manifest.generation != source_generation {
        return Err(SearchError::Format(
            "vNext CURRENT/manifest generation mismatch".into(),
        ));
    }
    validate_stored_manifest(&source_manifest)?;
    if source_manifest.layers.len() <= 1 {
        return Err(SearchError::InvalidArgument(
            "vNext durable compaction requires at least one delta layer".into(),
        ));
    }

    let source_index = open_manifest_index(root, &source_manifest)?;
    let source_layer_count = source_index.layer_count();
    let source_segment_count = source_index.segment_count();
    let source_tombstone_events = source_index.tombstone_events();
    let live_documents = source_index.materialize_live_documents()?;

    let compacted_generation = source_generation
        .checked_add(1)
        .ok_or_else(|| SearchError::InvalidArgument("vNext generation overflow".into()))?;
    let component_relative = component_relative(StoredLayerKind::Base, compacted_generation);
    let segment_files = publish_component(
        root,
        StoredLayerKind::Base,
        &component_relative,
        &live_documents,
        &[],
        segment_docs,
    )?;

    let compacted_manifest = StoredManifest {
        generation: compacted_generation,
        layers: vec![StoredLayer {
            kind: StoredLayerKind::Base,
            generation: compacted_generation,
            segment_files,
            tombstone_file: None,
        }],
    };
    validate_stored_manifest(&compacted_manifest)?;

    let manifest_relative = manifest_relative(compacted_generation, "compact");
    publish_manifest(root, &manifest_relative, &compacted_manifest)?;

    // Validate the exact durable bytes while the old CURRENT is still authoritative. Compaction
    // is a background path, so compare every live path/content payload as well as the logical IDs.
    let persisted = read_manifest(&safe_join(root, &manifest_relative)?)?;
    let compacted_index = open_manifest_index(root, &persisted)?;
    if compacted_index.materialize_live_documents()? != live_documents {
        return Err(SearchError::Format(
            "vNext compacted durable snapshot differs from source live snapshot".into(),
        ));
    }

    // A compaction can take much longer than a small delta build. Re-check the source pointer just
    // before the visibility switch so a stale background compaction does not intentionally publish
    // over a newer generation in the supported single-writer workflow.
    verify_current_pointer(root, source_generation, &source_manifest_relative)?;
    publish_current(
        root,
        compacted_generation,
        &manifest_relative,
        compacted_index.live_docs(),
    )?;
    verify_current_pointer(root, compacted_generation, &manifest_relative)?;

    Ok(VNextDurableCompactionReport {
        source_generation,
        compacted_generation,
        live_docs: compacted_index.live_docs(),
        source_layer_count,
        source_segment_count,
        source_tombstone_events,
        compacted_segment_count: compacted_index.segment_count(),
    })
}

pub fn gc_vnext_generation_store(
    root: impl AsRef<Path>,
    grace_period: Duration,
) -> Result<VNextDurableGcReport, SearchError> {
    let root = root.as_ref();
    let (current_generation, current_manifest_relative) = read_current(root)?;
    let current_manifest = read_manifest(&safe_join(root, &current_manifest_relative)?)?;
    if current_manifest.generation != current_generation {
        return Err(SearchError::Format(
            "vNext CURRENT/manifest generation mismatch".into(),
        ));
    }
    validate_stored_manifest(&current_manifest)?;

    let mut reachable_components = HashSet::<String>::new();
    for layer in &current_manifest.layers {
        for segment in &layer.segment_files {
            let component = component_dir_from_file(segment)?;
            reachable_components.insert(component.to_owned());
        }
        if let Some(tombstone) = &layer.tombstone_file {
            let component = component_dir_from_file(tombstone)?;
            reachable_components.insert(component.to_owned());
        }
    }

    let now = SystemTime::now();
    let mut component_candidates = Vec::<(PathBuf, u64)>::new();
    let mut manifest_candidates = Vec::<(PathBuf, u64)>::new();
    let mut deferred_by_grace = 0usize;

    let components_dir = root.join("components");
    if components_dir.exists() {
        for entry in fs::read_dir(&components_dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if !file_type.is_dir() || file_type.is_symlink() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(generation) = parse_component_dir_generation(&name) else {
                continue;
            };
            if generation >= current_generation {
                continue;
            }
            let relative = format!("components/{name}");
            if reachable_components.contains(&relative) {
                continue;
            }
            let metadata = entry.metadata()?;
            if !is_older_than(&metadata, now, grace_period)? {
                deferred_by_grace += 1;
                continue;
            }
            let bytes = directory_size_no_follow(&entry.path())?;
            component_candidates.push((entry.path(), bytes));
        }
    }

    let generations_dir = root.join("generations");
    if generations_dir.exists() {
        for entry in fs::read_dir(&generations_dir)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if !file_type.is_file() || file_type.is_symlink() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(generation) = parse_manifest_generation(&name) else {
                continue;
            };
            if generation >= current_generation {
                continue;
            }
            let relative = format!("generations/{name}");
            if relative == current_manifest_relative {
                continue;
            }
            let metadata = entry.metadata()?;
            if !is_older_than(&metadata, now, grace_period)? {
                deferred_by_grace += 1;
                continue;
            }
            manifest_candidates.push((entry.path(), metadata.len()));
        }
    }

    // GC is intentionally conservative around concurrent publishers. Only generations strictly
    // older than the observed CURRENT are candidates, and CURRENT must still be unchanged after
    // the potentially slow directory scan before the first unlink is attempted. A future delta
    // always retains all paths reachable from its base manifest, while a future compaction no
    // longer needs the obsolete paths.
    verify_current_pointer(root, current_generation, &current_manifest_relative)?;

    component_candidates.sort_by(|left, right| left.0.cmp(&right.0));
    manifest_candidates.sort_by(|left, right| left.0.cmp(&right.0));

    let mut removed_component_dirs = 0usize;
    let mut removed_manifest_files = 0usize;
    let mut reclaimed_bytes = 0u64;
    let mut deferred_in_use = 0usize;

    for (path, bytes) in component_candidates {
        match fs::remove_dir_all(&path) {
            Ok(()) => {
                removed_component_dirs += 1;
                reclaimed_bytes = reclaimed_bytes.saturating_add(bytes);
            }
            Err(error) if gc_delete_may_be_in_use(&error) => {
                deferred_in_use += 1;
            }
            Err(error) => return Err(error.into()),
        }
    }
    if components_dir.exists() {
        sync_directory(&components_dir)?;
    }

    for (path, bytes) in manifest_candidates {
        match fs::remove_file(&path) {
            Ok(()) => {
                removed_manifest_files += 1;
                reclaimed_bytes = reclaimed_bytes.saturating_add(bytes);
            }
            Err(error) if gc_delete_may_be_in_use(&error) => {
                deferred_in_use += 1;
            }
            Err(error) => return Err(error.into()),
        }
    }
    if generations_dir.exists() {
        sync_directory(&generations_dir)?;
    }

    // Deletion never edits CURRENT or the current manifest. Re-open the published snapshot after
    // GC so an implementation bug cannot silently remove something still required for restart.
    verify_current_pointer(root, current_generation, &current_manifest_relative)?;
    let reopened = open_vnext_published_generation(root)?;
    if reopened.generation() != current_generation {
        return Err(SearchError::Format(
            "vNext GC changed the published generation".into(),
        ));
    }

    Ok(VNextDurableGcReport {
        current_generation,
        reachable_component_dirs: reachable_components.len(),
        removed_component_dirs,
        removed_manifest_files,
        reclaimed_bytes,
        deferred_by_grace,
        deferred_in_use,
    })
}

pub fn open_vnext_published_generation(
    root: impl AsRef<Path>,
) -> Result<VNextGenerationIndex, SearchError> {
    let root = root.as_ref();
    let (generation, manifest_relative) = read_current(root)?;
    let manifest = read_manifest(&safe_join(root, &manifest_relative)?)?;
    if manifest.generation != generation {
        return Err(SearchError::Format(
            "vNext CURRENT/manifest generation mismatch".into(),
        ));
    }
    open_manifest_index_published(root, &manifest)
}

pub fn verify_vnext_generation_store(
    root: impl AsRef<Path>,
) -> Result<VNextDurableGenerationReport, SearchError> {
    let index = open_vnext_published_generation(root)?;
    Ok(report_from_index(&index))
}

fn report_from_index(index: &VNextGenerationIndex) -> VNextDurableGenerationReport {
    VNextDurableGenerationReport {
        generation: index.generation(),
        live_docs: index.live_docs(),
        layer_count: index.layer_count(),
        delta_count: index.layer_count().saturating_sub(1),
        segment_count: index.segment_count(),
        tombstone_events: index.tombstone_events(),
    }
}

fn verify_current_pointer(
    root: &Path,
    generation: Generation,
    manifest_relative: &str,
) -> Result<(), SearchError> {
    let (persisted_generation, persisted_manifest) = read_current(root)?;
    if persisted_generation != generation || persisted_manifest != manifest_relative {
        return Err(SearchError::Format(
            "vNext CURRENT publish verification mismatch".into(),
        ));
    }
    Ok(())
}

fn publish_component(
    root: &Path,
    kind: StoredLayerKind,
    component_relative: &str,
    documents: &[VNextDocumentInput],
    tombstones: &[LogicalDocId],
    segment_docs: usize,
) -> Result<Vec<String>, SearchError> {
    validate_relative_path(component_relative)?;
    let final_path = safe_join(root, component_relative)?;
    if final_path.exists() {
        return Err(SearchError::InvalidArgument(format!(
            "vNext generation component already exists: {}",
            final_path.display()
        )));
    }
    let components = root.join("components");
    fs::create_dir_all(&components)?;
    let temp_path = components.join(format!(
        ".publish-{}-{}-{}.tmp",
        std::process::id(),
        final_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("generation"),
        unique_nonce()?
    ));
    fs::create_dir(&temp_path)?;

    let publish_result = (|| -> Result<Vec<String>, SearchError> {
        let batches = document_batches(documents, segment_docs)?;
        let segment_files = write_document_ranges_parallel(
            &temp_path,
            component_relative,
            documents,
            &batches,
            default_publish_workers(documents, batches.len()),
        )?;
        if kind == StoredLayerKind::Delta {
            write_tombstones(&temp_path.join("tombstones.bin"), tombstones)?;
        }
        sync_directory(&temp_path)?;
        fs::rename(&temp_path, &final_path)?;
        sync_directory(&components)?;
        Ok(segment_files)
    })();
    if publish_result.is_err() {
        let _ = fs::remove_dir_all(&temp_path);
    }
    publish_result
}

fn default_publish_workers(documents: &[VNextDocumentInput], segment_count: usize) -> usize {
    let cpus = std::thread::available_parallelism().map_or(1, usize::from);
    let average_content = if documents.is_empty() {
        0
    } else {
        documents
            .iter()
            .map(|document| document.normalized_content.len() as u64)
            .sum::<u64>()
            / documents.len() as u64
    };
    // Each segment writer already parallelizes q1/q2/q3/path indexes internally. Very small
    // documents therefore saturate cores with only two concurrent segments, while medium/large
    // documents benefit from four independent segment writers.
    let cap = if average_content <= 256 { 2 } else { 4 };
    cpus.clamp(1, cap).min(segment_count.max(1))
}

fn segment_writer_cpu_budget_for(cpus: usize, concurrent_segment_workers: usize) -> usize {
    cpus.max(1)
        .checked_div(concurrent_segment_workers.max(1))
        .unwrap_or(1)
        .max(1)
}

fn segment_writer_cpu_budget(concurrent_segment_workers: usize) -> usize {
    segment_writer_cpu_budget_for(
        thread::available_parallelism().map_or(1, usize::from),
        concurrent_segment_workers,
    )
}

fn write_document_ranges_parallel(
    temp_path: &Path,
    component_relative: &str,
    documents: &[VNextDocumentInput],
    batches: &[std::ops::Range<usize>],
    workers: usize,
) -> Result<Vec<String>, SearchError> {
    if batches.is_empty() {
        return Ok(Vec::new());
    }
    let workers = workers.clamp(1, batches.len());
    let segment_cpu_budget = segment_writer_cpu_budget(workers);
    let next = AtomicUsize::new(0);
    let (result_tx, result_rx) = mpsc::channel::<(usize, Result<(), SearchError>)>();
    thread::scope(|scope| {
        for _ in 0..workers {
            let result_tx = result_tx.clone();
            let next = &next;
            scope.spawn(move || {
                loop {
                    let segment_index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(range) = batches.get(segment_index) else {
                        break;
                    };
                    let filename = format!("segment-{segment_index:05}.prseg2");
                    let result = write_vnext_segment_with_cpu_budget(
                        &temp_path.join(&filename),
                        &documents[range.clone()],
                        segment_cpu_budget,
                    );
                    if result_tx.send((segment_index, result.map(|_| ()))).is_err() {
                        break;
                    }
                }
            });
        }
    });
    drop(result_tx);

    let mut errors = Vec::new();
    for _ in 0..batches.len() {
        let (segment_index, result) = result_rx.recv().map_err(|_| {
            SearchError::Format("vNext parallel segment writer result channel closed".into())
        })?;
        if let Err(error) = result {
            errors.push((segment_index, error));
        }
    }
    if let Some((_, error)) = errors.into_iter().min_by_key(|(index, _)| *index) {
        return Err(error);
    }
    Ok((0..batches.len())
        .map(|segment_index| format!("{component_relative}/segment-{segment_index:05}.prseg2"))
        .collect())
}

fn publish_component_streaming<I>(
    root: &Path,
    component_relative: &str,
    documents: I,
    segment_docs: usize,
    workers: usize,
) -> Result<(Vec<String>, usize), SearchError>
where
    I: Iterator<Item = VNextDocumentInput>,
{
    validate_relative_path(component_relative)?;
    let final_path = safe_join(root, component_relative)?;
    if final_path.exists() {
        return Err(SearchError::InvalidArgument(format!(
            "vNext generation component already exists: {}",
            final_path.display()
        )));
    }
    let components = root.join("components");
    fs::create_dir_all(&components)?;
    let temp_path = components.join(format!(
        ".publish-{}-{}-{}.tmp",
        std::process::id(),
        final_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("generation"),
        unique_nonce()?
    ));
    fs::create_dir(&temp_path)?;

    let publish_result = (|| -> Result<(Vec<String>, usize), SearchError> {
        let workers = workers.max(1).clamp(1, 4);
        let segment_cpu_budget = segment_writer_cpu_budget(workers);
        let (ready_tx, ready_rx) = mpsc::channel::<usize>();
        let (result_tx, result_rx) = mpsc::channel::<(usize, Result<(), SearchError>)>();
        let mut seen_ids = HashSet::new();
        let mut live_docs = 0usize;
        let mut segment_count = 0usize;

        thread::scope(|scope| -> Result<(), SearchError> {
            let mut task_senders = Vec::with_capacity(workers);
            for worker_id in 0..workers {
                let (task_tx, task_rx) = mpsc::sync_channel::<(usize, Vec<VNextDocumentInput>)>(1);
                task_senders.push(task_tx);
                let ready_tx = ready_tx.clone();
                let result_tx = result_tx.clone();
                let temp_path = &temp_path;
                scope.spawn(move || {
                    loop {
                        if ready_tx.send(worker_id).is_err() {
                            break;
                        }
                        let Ok((segment_index, batch)) = task_rx.recv() else {
                            break;
                        };
                        let filename = format!("segment-{segment_index:05}.prseg2");
                        let result = write_vnext_segment_with_cpu_budget(
                            &temp_path.join(&filename),
                            &batch,
                            segment_cpu_budget,
                        )
                        .map(|_| ());
                        if result_tx.send((segment_index, result)).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(ready_tx);
            drop(result_tx);

            let mut pending = Vec::with_capacity(segment_docs);
            let mut pending_blocks = 0usize;
            let dispatch = |batch: Vec<VNextDocumentInput>,
                            segment_index: usize,
                            task_senders: &Vec<
                mpsc::SyncSender<(usize, Vec<VNextDocumentInput>)>,
            >|
             -> Result<(), SearchError> {
                let worker = ready_rx.recv().map_err(|_| {
                    SearchError::Format("vNext streaming writer readiness channel closed".into())
                })?;
                task_senders[worker]
                    .send((segment_index, batch))
                    .map_err(|_| {
                        SearchError::Format("vNext streaming writer task channel closed".into())
                    })
            };

            for document in documents {
                if document.logical_id == 0 || !seen_ids.insert(document.logical_id) {
                    return Err(SearchError::InvalidArgument(
                        "vNext durable documents require unique non-zero logical IDs".into(),
                    ));
                }
                let blocks = document
                    .normalized_content
                    .len()
                    .div_ceil(PUBLISH_BLOCK_SIZE);
                if blocks > MAX_LOCAL_ITEMS {
                    return Err(SearchError::InvalidArgument(format!(
                        "vNext document {} requires too many local blocks: {blocks}",
                        document.logical_id
                    )));
                }
                let exceeds_docs = pending.len() == segment_docs;
                let exceeds_blocks = pending_blocks
                    .checked_add(blocks)
                    .is_none_or(|value| value > MAX_LOCAL_ITEMS);
                if !pending.is_empty() && (exceeds_docs || exceeds_blocks) {
                    let batch = core::mem::replace(&mut pending, Vec::with_capacity(segment_docs));
                    dispatch(batch, segment_count, &task_senders)?;
                    segment_count += 1;
                    pending_blocks = 0;
                }
                pending_blocks = pending_blocks.saturating_add(blocks);
                pending.push(document);
                live_docs += 1;
            }
            if !pending.is_empty() {
                dispatch(pending, segment_count, &task_senders)?;
                segment_count += 1;
            }
            drop(task_senders);

            let mut first_error = None::<(usize, SearchError)>;
            for _ in 0..segment_count {
                let (segment_index, result) = result_rx.recv().map_err(|_| {
                    SearchError::Format("vNext streaming writer result channel closed".into())
                })?;
                if let Err(error) = result
                    && first_error
                        .as_ref()
                        .is_none_or(|(current, _)| segment_index < *current)
                {
                    first_error = Some((segment_index, error));
                }
            }
            if let Some((_, error)) = first_error {
                return Err(error);
            }
            Ok(())
        })?;

        sync_directory(&temp_path)?;
        fs::rename(&temp_path, &final_path)?;
        sync_directory(&components)?;
        let segment_files = (0..segment_count)
            .map(|segment_index| format!("{component_relative}/segment-{segment_index:05}.prseg2"))
            .collect();
        Ok((segment_files, live_docs))
    })();
    if publish_result.is_err() {
        let _ = fs::remove_dir_all(&temp_path);
    }
    publish_result
}

fn document_batches(
    documents: &[VNextDocumentInput],
    segment_docs: usize,
) -> Result<Vec<std::ops::Range<usize>>, SearchError> {
    if documents.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut docs_in_batch = 0usize;
    let mut blocks_in_batch = 0usize;

    for (index, document) in documents.iter().enumerate() {
        let blocks = document
            .normalized_content
            .len()
            .div_ceil(PUBLISH_BLOCK_SIZE);
        if blocks > MAX_LOCAL_ITEMS {
            return Err(SearchError::InvalidArgument(format!(
                "vNext document {} requires too many local blocks: {blocks}",
                document.logical_id
            )));
        }
        let would_exceed_docs = docs_in_batch == segment_docs;
        let would_exceed_blocks = blocks_in_batch
            .checked_add(blocks)
            .is_none_or(|value| value > MAX_LOCAL_ITEMS);
        if docs_in_batch > 0 && (would_exceed_docs || would_exceed_blocks) {
            out.push(start..index);
            start = index;
            docs_in_batch = 0;
            blocks_in_batch = 0;
        }
        docs_in_batch += 1;
        blocks_in_batch = blocks_in_batch
            .checked_add(blocks)
            .ok_or_else(|| SearchError::Format("vNext block count overflow".into()))?;
    }
    if start < documents.len() {
        out.push(start..documents.len());
    }
    Ok(out)
}

fn open_manifest_index(
    root: &Path,
    manifest: &StoredManifest,
) -> Result<VNextGenerationIndex, SearchError> {
    validate_stored_manifest(manifest)?;
    let mut specs = Vec::with_capacity(manifest.layers.len());
    for layer in &manifest.layers {
        let segment_paths = layer
            .segment_files
            .iter()
            .map(|relative| safe_join(root, relative))
            .collect::<Result<Vec<_>, _>>()?;
        let tombstones = match &layer.tombstone_file {
            Some(relative) => read_tombstones(&safe_join(root, relative)?)?,
            None => Vec::new(),
        };
        specs.push(VNextGenerationLayerSpec {
            kind: match layer.kind {
                StoredLayerKind::Base => VNextGenerationLayerKind::Base,
                StoredLayerKind::Delta => VNextGenerationLayerKind::Delta,
            },
            generation: layer.generation,
            segment_paths,
            tombstones,
        });
    }
    VNextGenerationIndex::open(manifest.generation, &specs)
}

fn open_manifest_index_published(
    root: &Path,
    manifest: &StoredManifest,
) -> Result<VNextGenerationIndex, SearchError> {
    validate_stored_manifest(manifest)?;
    let mut specs = Vec::with_capacity(manifest.layers.len());
    for layer in &manifest.layers {
        let segment_paths = layer
            .segment_files
            .iter()
            .map(|relative| safe_join(root, relative))
            .collect::<Result<Vec<_>, _>>()?;
        let tombstones = match &layer.tombstone_file {
            Some(relative) => read_tombstones(&safe_join(root, relative)?)?,
            None => Vec::new(),
        };
        specs.push(VNextGenerationLayerSpec {
            kind: match layer.kind {
                StoredLayerKind::Base => VNextGenerationLayerKind::Base,
                StoredLayerKind::Delta => VNextGenerationLayerKind::Delta,
            },
            generation: layer.generation,
            segment_paths,
            tombstones,
        });
    }
    VNextGenerationIndex::open_published(manifest.generation, &specs)
}

fn write_tombstones(path: &Path, tombstones: &[LogicalDocId]) -> Result<(), SearchError> {
    validate_tombstones(tombstones)?;
    let mut bytes = Vec::with_capacity(16 + tombstones.len() * 8 + TOMBSTONE_FOOTER_BYTES);
    bytes.extend_from_slice(TOMBSTONE_MAGIC);
    bytes.extend_from_slice(&(tombstones.len() as u64).to_le_bytes());
    for &logical_id in tombstones {
        bytes.extend_from_slice(&logical_id.to_le_bytes());
    }
    let checksum = fnv1a(&bytes);
    bytes.extend_from_slice(&checksum.to_le_bytes());
    write_durable_file(path, &bytes)
}

fn read_tombstones(path: &Path) -> Result<Vec<LogicalDocId>, SearchError> {
    let bytes = fs::read(path)?;
    if bytes.len() < 16 + TOMBSTONE_FOOTER_BYTES || bytes.get(..8) != Some(TOMBSTONE_MAGIC) {
        return Err(SearchError::Format("bad vNext tombstone file".into()));
    }
    let payload_end = bytes.len() - TOMBSTONE_FOOTER_BYTES;
    let expected = u64::from_le_bytes(
        bytes[payload_end..]
            .try_into()
            .expect("fixed vNext tombstone checksum slice"),
    );
    if fnv1a(&bytes[..payload_end]) != expected {
        return Err(SearchError::Format(
            "vNext tombstone checksum mismatch".into(),
        ));
    }
    let count = u64::from_le_bytes(bytes[8..16].try_into().expect("fixed count slice"));
    let count = usize::try_from(count)
        .map_err(|_| SearchError::Format("vNext tombstone count too large".into()))?;
    let expected_len = 16usize
        .checked_add(
            count
                .checked_mul(8)
                .ok_or_else(|| SearchError::Format("vNext tombstone byte size overflow".into()))?,
        )
        .and_then(|value| value.checked_add(TOMBSTONE_FOOTER_BYTES))
        .ok_or_else(|| SearchError::Format("vNext tombstone file size overflow".into()))?;
    if bytes.len() != expected_len {
        return Err(SearchError::Format(
            "vNext tombstone file length mismatch".into(),
        ));
    }
    let mut out = Vec::with_capacity(count);
    for chunk in bytes[16..payload_end].chunks_exact(8) {
        out.push(u64::from_le_bytes(
            chunk.try_into().expect("fixed tombstone slice"),
        ));
    }
    validate_tombstones(&out)?;
    Ok(out)
}

fn write_manifest(path: &Path, manifest: &StoredManifest) -> Result<(), SearchError> {
    validate_stored_manifest(manifest)?;
    let mut body = format!(
        "{MANIFEST_MAGIC}\ngeneration {}\nlayers {}\n",
        manifest.generation,
        manifest.layers.len()
    );
    for layer in &manifest.layers {
        let tombstone = layer.tombstone_file.as_deref().unwrap_or("-");
        body.push_str(&format!(
            "layer {} {} {} {}\n",
            layer.kind.as_str(),
            layer.generation,
            tombstone,
            layer.segment_files.len()
        ));
        for segment in &layer.segment_files {
            body.push_str(&format!("segment {segment}\n"));
        }
    }
    let checksum = fnv1a(body.as_bytes());
    let text = format!("{body}checksum {checksum:016x}\n");
    write_durable_file(path, text.as_bytes())
}

fn read_manifest(path: &Path) -> Result<StoredManifest, SearchError> {
    let text = fs::read_to_string(path)?;
    let (body, checksum) = split_checked_text(&text, "vNext generation manifest")?;
    let mut lines = body.lines();
    if lines.next() != Some(MANIFEST_MAGIC) {
        return Err(SearchError::Format(
            "bad vNext generation manifest magic".into(),
        ));
    }
    let generation = parse_named_u64(lines.next(), "generation")?;
    let layer_count = parse_named_usize(lines.next(), "layers")?;
    let mut layers = Vec::with_capacity(layer_count);
    for _ in 0..layer_count {
        let line = lines
            .next()
            .ok_or_else(|| SearchError::Format("missing vNext generation layer".into()))?;
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 5 || fields[0] != "layer" {
            return Err(SearchError::Format(
                "bad vNext generation layer line".into(),
            ));
        }
        let kind = StoredLayerKind::parse(fields[1])?;
        let layer_generation = fields[2]
            .parse::<Generation>()
            .map_err(|_| SearchError::Format("bad vNext layer generation".into()))?;
        let tombstone_file = if fields[3] == "-" {
            None
        } else {
            validate_relative_path(fields[3])?;
            Some(fields[3].to_owned())
        };
        let segment_count = fields[4]
            .parse::<usize>()
            .map_err(|_| SearchError::Format("bad vNext layer segment count".into()))?;
        let mut segment_files = Vec::with_capacity(segment_count);
        for _ in 0..segment_count {
            let segment_line = lines
                .next()
                .ok_or_else(|| SearchError::Format("missing vNext segment line".into()))?;
            let mut segment_fields = segment_line.split_whitespace();
            if segment_fields.next() != Some("segment") {
                return Err(SearchError::Format("bad vNext segment line".into()));
            }
            let relative = segment_fields
                .next()
                .ok_or_else(|| SearchError::Format("vNext segment path missing".into()))?;
            if segment_fields.next().is_some() {
                return Err(SearchError::Format("bad vNext segment line".into()));
            }
            validate_relative_path(relative)?;
            segment_files.push(relative.to_owned());
        }
        layers.push(StoredLayer {
            kind,
            generation: layer_generation,
            segment_files,
            tombstone_file,
        });
    }
    if lines.next().is_some() {
        return Err(SearchError::Format(
            "vNext generation manifest has trailing lines".into(),
        ));
    }
    let manifest = StoredManifest { generation, layers };
    validate_stored_manifest(&manifest)?;
    // Keep checksum in scope until structure validation succeeds, making it explicit that both
    // transport integrity and semantic integrity are required.
    let _ = checksum;
    Ok(manifest)
}

fn publish_manifest(
    root: &Path,
    relative: &str,
    manifest: &StoredManifest,
) -> Result<(), SearchError> {
    validate_relative_path(relative)?;
    let final_path = safe_join(root, relative)?;
    if final_path.exists() {
        return Err(SearchError::InvalidArgument(format!(
            "vNext generation manifest already exists: {}",
            final_path.display()
        )));
    }
    let parent = final_path
        .parent()
        .ok_or_else(|| SearchError::Format("vNext manifest parent missing".into()))?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".{}.{}-{}.tmp",
        final_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("generation"),
        std::process::id(),
        unique_nonce()?
    ));
    let result = (|| -> Result<(), SearchError> {
        write_manifest(&temp, manifest)?;
        fs::rename(&temp, &final_path)?;
        sync_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn publish_current(
    root: &Path,
    generation: Generation,
    manifest_relative: &str,
    live_docs: usize,
) -> Result<(), SearchError> {
    validate_relative_path(manifest_relative)?;
    let body = format!(
        "{CURRENT_MAGIC}\ngeneration {generation}\nmanifest {manifest_relative}\nlive_docs {live_docs}\n"
    );
    let checksum = fnv1a(body.as_bytes());
    let text = format!("{body}checksum {checksum:016x}\n");
    let temp = root.join(format!(
        ".CURRENT.{}-{}.tmp",
        std::process::id(),
        unique_nonce()?
    ));
    let result = (|| -> Result<(), SearchError> {
        write_durable_file(&temp, text.as_bytes())?;
        fs::rename(&temp, root.join("CURRENT"))?;
        sync_directory(root)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CurrentRecord {
    generation: Generation,
    manifest_relative: String,
    live_docs: Option<usize>,
}

fn read_current_record(root: &Path) -> Result<CurrentRecord, SearchError> {
    let text = fs::read_to_string(root.join("CURRENT"))?;
    let (body, _checksum) = split_checked_text(&text, "vNext CURRENT")?;
    let mut lines = body.lines();
    if lines.next() != Some(CURRENT_MAGIC) {
        return Err(SearchError::Format("bad vNext CURRENT magic".into()));
    }
    let generation = parse_named_u64(lines.next(), "generation")?;
    let manifest_line = lines
        .next()
        .ok_or_else(|| SearchError::Format("vNext CURRENT manifest missing".into()))?;
    let mut fields = manifest_line.split_whitespace();
    if fields.next() != Some("manifest") {
        return Err(SearchError::Format(
            "bad vNext CURRENT manifest line".into(),
        ));
    }
    let manifest_relative = fields
        .next()
        .ok_or_else(|| SearchError::Format("vNext CURRENT manifest path missing".into()))?;
    if fields.next().is_some() {
        return Err(SearchError::Format(
            "bad vNext CURRENT manifest line".into(),
        ));
    }
    validate_relative_path(manifest_relative)?;
    if !manifest_relative.starts_with("generations/") {
        return Err(SearchError::Format(
            "vNext CURRENT manifest must be under generations".into(),
        ));
    }
    let live_docs = match lines.next() {
        Some(line) if line.starts_with("live_docs ") => {
            Some(parse_named_usize(Some(line), "live_docs")?)
        }
        Some(_) => {
            return Err(SearchError::Format(
                "bad vNext CURRENT live_docs line".into(),
            ));
        }
        None => None,
    };
    if lines.next().is_some() {
        return Err(SearchError::Format(
            "vNext CURRENT has trailing lines".into(),
        ));
    }
    Ok(CurrentRecord {
        generation,
        manifest_relative: manifest_relative.to_owned(),
        live_docs,
    })
}

fn read_current(root: &Path) -> Result<(Generation, String), SearchError> {
    let current = read_current_record(root)?;
    Ok((current.generation, current.manifest_relative))
}

fn split_checked_text<'a>(text: &'a str, label: &str) -> Result<(&'a str, u64), SearchError> {
    let stripped = text
        .strip_suffix('\n')
        .ok_or_else(|| SearchError::Format(format!("{label} missing final newline")))?;
    let split = stripped
        .rfind("\nchecksum ")
        .ok_or_else(|| SearchError::Format(format!("{label} checksum missing")))?;
    let body_without_newline = &stripped[..split];
    let checksum_text = &stripped[split + "\nchecksum ".len()..];
    if checksum_text.len() != 16 || !checksum_text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(SearchError::Format(format!("bad {label} checksum line")));
    }
    let expected = u64::from_str_radix(checksum_text, 16)
        .map_err(|_| SearchError::Format(format!("bad {label} checksum")))?;
    let body_len = body_without_newline
        .len()
        .checked_add(1)
        .ok_or_else(|| SearchError::Format(format!("{label} length overflow")))?;
    let body = &text[..body_len];
    if fnv1a(body.as_bytes()) != expected {
        return Err(SearchError::Format(format!("{label} checksum mismatch")));
    }
    Ok((body, expected))
}

fn validate_stored_manifest(manifest: &StoredManifest) -> Result<(), SearchError> {
    let first = manifest
        .layers
        .first()
        .ok_or_else(|| SearchError::Format("vNext durable manifest has no base layer".into()))?;
    if first.kind != StoredLayerKind::Base || first.tombstone_file.is_some() {
        return Err(SearchError::Format(
            "vNext durable manifest must start with a base layer without tombstones".into(),
        ));
    }

    let mut previous = None;
    let mut seen_paths = HashSet::<&str>::new();
    for (index, layer) in manifest.layers.iter().enumerate() {
        if index > 0 && layer.kind != StoredLayerKind::Delta {
            return Err(SearchError::Format(
                "vNext durable manifest may contain only one base layer".into(),
            ));
        }
        if previous.is_some_and(|value| layer.generation <= value) {
            return Err(SearchError::Format(
                "vNext durable layer generations must be strictly increasing".into(),
            ));
        }
        if layer.generation > manifest.generation {
            return Err(SearchError::Format(
                "vNext durable layer generation exceeds published generation".into(),
            ));
        }
        if layer.kind == StoredLayerKind::Delta && layer.tombstone_file.is_none() {
            return Err(SearchError::Format(
                "vNext durable delta layer must reference tombstones".into(),
            ));
        }
        for segment in &layer.segment_files {
            validate_component_path(segment, ".prseg2")?;
            if !seen_paths.insert(segment) {
                return Err(SearchError::Format(
                    "duplicate vNext durable segment path".into(),
                ));
            }
        }
        if let Some(tombstone) = &layer.tombstone_file {
            validate_component_path(tombstone, "tombstones.bin")?;
            if !seen_paths.insert(tombstone) {
                return Err(SearchError::Format(
                    "duplicate vNext durable tombstone path".into(),
                ));
            }
        }
        previous = Some(layer.generation);
    }
    if previous != Some(manifest.generation) {
        return Err(SearchError::Format(
            "vNext durable manifest generation must equal newest layer".into(),
        ));
    }
    Ok(())
}

fn validate_component_path(value: &str, suffix: &str) -> Result<(), SearchError> {
    validate_relative_path(value)?;
    if !value.starts_with("components/") || !value.ends_with(suffix) {
        return Err(SearchError::Format(
            "vNext durable component path has unexpected location or suffix".into(),
        ));
    }
    Ok(())
}

fn component_dir_from_file(relative: &str) -> Result<&str, SearchError> {
    validate_relative_path(relative)?;
    if !relative.starts_with("components/") {
        return Err(SearchError::Format("bad vNext component path".into()));
    }
    relative
        .rsplit_once('/')
        .map(|(parent, _)| parent)
        .filter(|parent| parent.starts_with("components/"))
        .ok_or_else(|| SearchError::Format("bad vNext component parent".into()))
}

fn parse_component_dir_generation(name: &str) -> Option<Generation> {
    let digits = name
        .strip_prefix("base-g")
        .or_else(|| name.strip_prefix("delta-g"))?;
    if digits.len() != 16 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn parse_manifest_generation(name: &str) -> Option<Generation> {
    let rest = name.strip_prefix('g')?;
    if rest.len() < 17 {
        return None;
    }
    let (digits, suffix) = rest.split_at(16);
    if !digits.bytes().all(|byte| byte.is_ascii_digit())
        || !matches!(
            suffix,
            "-base.manifest" | "-delta.manifest" | "-compact.manifest"
        )
    {
        return None;
    }
    digits.parse().ok()
}

fn is_older_than(
    metadata: &fs::Metadata,
    now: SystemTime,
    grace_period: Duration,
) -> Result<bool, SearchError> {
    if grace_period.is_zero() {
        return Ok(true);
    }
    let modified = metadata.modified()?;
    Ok(now
        .duration_since(modified)
        .is_ok_and(|elapsed| elapsed >= grace_period))
}

fn directory_size_no_follow(path: &Path) -> Result<u64, SearchError> {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() {
                total = total.saturating_add(metadata.len());
            } else if metadata.is_dir() {
                stack.push(entry.path());
            } else {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    Ok(total)
}

fn gc_delete_may_be_in_use(error: &std::io::Error) -> bool {
    #[cfg(windows)]
    {
        matches!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock
        )
    }
    #[cfg(not(windows))]
    {
        let _ = error;
        false
    }
}

fn validate_segment_docs(segment_docs: usize) -> Result<(), SearchError> {
    if segment_docs == 0 || segment_docs > MAX_LOCAL_ITEMS {
        return Err(SearchError::InvalidArgument(format!(
            "vNext durable segment_docs must be in 1..={MAX_LOCAL_ITEMS}"
        )));
    }
    Ok(())
}

fn validate_document_inputs(documents: &[VNextDocumentInput]) -> Result<(), SearchError> {
    let mut seen = HashSet::with_capacity(documents.len());
    for document in documents {
        if document.logical_id == 0 || !seen.insert(document.logical_id) {
            return Err(SearchError::InvalidArgument(
                "vNext durable documents require unique non-zero logical IDs".into(),
            ));
        }
    }
    Ok(())
}

fn validate_update_plan_payload(plan: &UpdatePlan) -> Result<(), SearchError> {
    let mut seen = HashSet::with_capacity(plan.upserts.len());
    for upsert in &plan.upserts {
        if upsert.logical_id == 0 || !seen.insert(upsert.logical_id) {
            return Err(SearchError::InvalidArgument(
                "vNext durable upserts require unique non-zero logical IDs".into(),
            ));
        }
    }
    validate_tombstones(&plan.tombstones)
}

fn validate_tombstones(tombstones: &[LogicalDocId]) -> Result<(), SearchError> {
    let mut previous = None;
    for &logical_id in tombstones {
        if logical_id == 0 || previous.is_some_and(|value| logical_id <= value) {
            return Err(SearchError::InvalidArgument(
                "vNext durable tombstones must be sorted unique non-zero logical IDs".into(),
            ));
        }
        previous = Some(logical_id);
    }
    Ok(())
}

fn component_relative(kind: StoredLayerKind, generation: Generation) -> String {
    format!(
        "components/{}-g{generation:016}",
        match kind {
            StoredLayerKind::Base => "base",
            StoredLayerKind::Delta => "delta",
        }
    )
}

fn manifest_relative(generation: Generation, suffix: &str) -> String {
    format!("generations/g{generation:016}-{suffix}.manifest")
}

fn parse_named_u64(line: Option<&str>, name: &str) -> Result<u64, SearchError> {
    let line = line.ok_or_else(|| SearchError::Format(format!("missing vNext {name} line")))?;
    let mut fields = line.split_whitespace();
    if fields.next() != Some(name) {
        return Err(SearchError::Format(format!("bad vNext {name} line")));
    }
    let value = fields
        .next()
        .ok_or_else(|| SearchError::Format(format!("missing vNext {name} value")))?
        .parse::<u64>()
        .map_err(|_| SearchError::Format(format!("invalid vNext {name}")))?;
    if fields.next().is_some() {
        return Err(SearchError::Format(format!("bad vNext {name} line")));
    }
    Ok(value)
}

fn parse_named_usize(line: Option<&str>, name: &str) -> Result<usize, SearchError> {
    let value = parse_named_u64(line, name)?;
    usize::try_from(value).map_err(|_| SearchError::Format(format!("vNext {name} too large")))
}

fn validate_relative_path(value: &str) -> Result<(), SearchError> {
    let path = Path::new(value);
    if value.is_empty() || path.is_absolute() {
        return Err(SearchError::Format(
            "unsafe vNext durable generation path".into(),
        ));
    }
    for component in path.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(SearchError::Format(
                "unsafe vNext durable generation path".into(),
            ));
        }
    }
    Ok(())
}

fn safe_join(root: &Path, relative: &str) -> Result<PathBuf, SearchError> {
    validate_relative_path(relative)?;
    Ok(root.join(relative))
}

fn write_durable_file(path: &Path, bytes: &[u8]) -> Result<(), SearchError> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), SearchError> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn unique_nonce() -> Result<u128, SearchError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .map_err(|_| SearchError::Format("system clock before UNIX epoch".into()))
}

#[cfg(test)]
mod publish_budget_tests {
    use super::segment_writer_cpu_budget_for;

    #[test]
    fn segment_writer_budget_shares_machine_cpus_without_oversubscription() {
        assert_eq!(segment_writer_cpu_budget_for(1, 4), 1);
        assert_eq!(segment_writer_cpu_budget_for(5, 4), 1);
        assert_eq!(segment_writer_cpu_budget_for(5, 2), 2);
        assert_eq!(segment_writer_cpu_budget_for(8, 4), 2);
        assert_eq!(segment_writer_cpu_budget_for(16, 1), 16);
        for cpus in 1usize..=64 {
            for workers in 1usize..=8 {
                let budget = segment_writer_cpu_budget_for(cpus, workers);
                assert!(budget >= 1);
                if workers <= cpus {
                    assert!(budget * workers <= cpus);
                }
            }
        }
    }
}
