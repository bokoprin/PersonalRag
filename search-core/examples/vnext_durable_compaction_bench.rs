use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use personalrag_portable_search::{
    DocumentInput, PlannedUpsert, UpdatePlan, VNextDocumentInput, compact_vnext_generation_store,
    fold_ascii, initialize_vnext_generation_store, open_vnext_published_generation,
    publish_vnext_incremental_generation,
};

fn base_doc(id: u64) -> VNextDocumentInput {
    let path = format!("corpus/group_{:03}/module_{id:05}.txt", id % 127);
    let content = format!(
        "personalrag compact base document {id:05} timeout common payload unique_marker_{id:05} 日本語検索 \
         repeated text repeated text repeated text repeated text"
    );
    VNextDocumentInput::new(id, path, fold_ascii(content.as_bytes()))
}

fn updated_doc(id: u64, generation: u64) -> DocumentInput {
    let key = format!("key-{id:05}");
    let path = format!("corpus/updated_g{generation}/module_{id:05}.txt");
    let content = format!(
        "personalrag compact updated generation_{generation} document {id:05} \
         compact_generation_{generation}_marker timeout 日本語検索"
    );
    DocumentInput::new(
        key,
        &path,
        fold_ascii(path.as_bytes()),
        fold_ascii(content.as_bytes()),
    )
}

fn dir_bytes(path: &Path) -> Result<u64, Box<dyn Error>> {
    if !path.exists() {
        return Ok(0);
    }
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() {
            total = total
                .checked_add(dir_bytes(&entry.path())?)
                .ok_or("byte overflow")?;
        } else {
            total = total.checked_add(metadata.len()).ok_or("byte overflow")?;
        }
    }
    Ok(total)
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let docs = args
        .first()
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(20_000);
    let root = args
        .get(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("personalrag-vnext-compaction-bench"));
    if docs < 10_000 {
        return Err("vNext compaction benchmark needs at least 10000 documents".into());
    }
    let _ = fs::remove_dir_all(&root);

    let base = (1..=docs as u64).map(base_doc).collect::<Vec<_>>();
    let started = Instant::now();
    let init = initialize_vnext_generation_store(&root, &base, 5_000)?;
    println!(
        "VNEXT_COMPACT_INIT docs={} segments={} elapsed_ms={:.3}",
        init.live_docs,
        init.segment_count,
        started.elapsed().as_secs_f64() * 1000.0
    );

    let change_counts = [1usize, 10, 100, 1_000];
    for (index, &changes) in change_counts.iter().enumerate() {
        let generation = index as u64 + 1;
        let start_id = 1 + index * 2_000;
        let mut upserts = Vec::with_capacity(changes);
        let mut tombstones = Vec::with_capacity(changes);
        for offset in 0..changes {
            let id = ((start_id - 1 + offset) % docs + 1) as u64;
            upserts.push(PlannedUpsert {
                logical_id: id,
                is_insert: false,
                document: updated_doc(id, generation),
            });
            tombstones.push(id);
        }
        let plan = UpdatePlan {
            base_generation: generation - 1,
            next_generation: generation,
            upserts,
            tombstones,
            live_docs_after: docs,
            compaction_recommended: generation == 4,
        };
        publish_vnext_incremental_generation(&root, &plan, 5_000)?;
    }

    let before_started = Instant::now();
    let before = open_vnext_published_generation(&root)?;
    let before_open_ms = before_started.elapsed().as_secs_f64() * 1000.0;
    if before.layer_count() != 5 || before.live_docs() != docs {
        return Err("unexpected pre-compaction generation shape".into());
    }
    let before_common = before.search_content(b"timeout")?;
    let before_latest = before.search_content(b"compact_generation_4_marker")?;
    let before_rare = before.search_content(b"unique_marker_15000")?;
    let before_path = before.search_path(b"updated_g4")?;

    let mut source_bytes = dir_bytes(&root.join("components/base-g0000000000000000"))?;
    for generation in 1..=4u64 {
        source_bytes = source_bytes
            .checked_add(dir_bytes(
                &root.join(format!("components/delta-g{generation:016}")),
            )?)
            .ok_or("source bytes overflow")?;
    }

    let compact_started = Instant::now();
    let report = compact_vnext_generation_store(&root, 5_000)?;
    let compact_ms = compact_started.elapsed().as_secs_f64() * 1000.0;

    let after_started = Instant::now();
    let after = open_vnext_published_generation(&root)?;
    let after_open_ms = after_started.elapsed().as_secs_f64() * 1000.0;
    let after_common = after.search_content(b"timeout")?;
    let after_latest = after.search_content(b"compact_generation_4_marker")?;
    let after_rare = after.search_content(b"unique_marker_15000")?;
    let after_path = after.search_path(b"updated_g4")?;
    if before_common != after_common
        || before_latest != after_latest
        || before_rare != after_rare
        || before_path != after_path
    {
        return Err("pre/post compaction query mismatch".into());
    }

    let compacted_bytes = dir_bytes(&root.join(format!(
        "components/base-g{:016}",
        report.compacted_generation
    )))?;
    println!(
        "VNEXT_COMPACTION source_generation={} compacted_generation={} live_docs={} source_layers={} source_segments={} source_tombstones={} compacted_segments={} compact_ms={compact_ms:.3} before_open_ms={before_open_ms:.3} after_open_ms={after_open_ms:.3} source_referenced_bytes={} compacted_referenced_bytes={} common_hits={} latest_hits={} rare_hits={} path_hits={}",
        report.source_generation,
        report.compacted_generation,
        report.live_docs,
        report.source_layer_count,
        report.source_segment_count,
        report.source_tombstone_events,
        report.compacted_segment_count,
        source_bytes,
        compacted_bytes,
        after_common.len(),
        after_latest.len(),
        after_rare.len(),
        after_path.len()
    );
    Ok(())
}
