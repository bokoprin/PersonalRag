use std::error::Error;
use std::path::PathBuf;
use std::time::Instant;

use personalrag_portable_search::{
    DocumentInput, PlannedUpsert, UpdatePlan, VNextDocumentInput, fold_ascii,
    initialize_vnext_generation_store, open_vnext_published_generation,
    publish_vnext_incremental_generation,
};

fn base_doc(id: u64) -> VNextDocumentInput {
    let path = format!("corpus/group_{:03}/module_{id:05}.txt", id % 127);
    let content = format!(
        "personalrag durable base document {id:05} timeout common payload unique_marker_{id:05} 日本語検索 \
         repeated text repeated text repeated text repeated text"
    );
    VNextDocumentInput::new(id, path, fold_ascii(content.as_bytes()))
}

fn updated_doc(id: u64, generation: u64) -> DocumentInput {
    let key = format!("key-{id:05}");
    let path = format!("corpus/updated_g{generation}/module_{id:05}.txt");
    let content = format!(
        "personalrag durable updated generation_{generation} document {id:05} \
         durable_generation_{generation}_marker timeout 日本語検索"
    );
    DocumentInput::new(
        key,
        &path,
        fold_ascii(path.as_bytes()),
        fold_ascii(content.as_bytes()),
    )
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
        .unwrap_or_else(|| std::env::temp_dir().join("personalrag-vnext-durable-bench"));
    if docs < 2_000 {
        return Err("vNext durable benchmark needs at least 2000 documents".into());
    }
    let _ = std::fs::remove_dir_all(&root);

    let base = (1..=docs as u64).map(base_doc).collect::<Vec<_>>();
    let started = Instant::now();
    let init = initialize_vnext_generation_store(&root, &base, 5_000)?;
    println!(
        "VNEXT_DURABLE_INIT docs={} segments={} elapsed_ms={:.3}",
        init.live_docs,
        init.segment_count,
        started.elapsed().as_secs_f64() * 1000.0
    );

    let change_counts = [1usize, 10, 100, 1_000];
    let mut generation = 0u64;
    for &changes in &change_counts {
        generation += 1;
        let start_id = 1 + ((generation as usize - 1) * 2_000) % docs;
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
        tombstones.sort_unstable();
        tombstones.dedup();
        let plan = UpdatePlan {
            base_generation: generation - 1,
            next_generation: generation,
            upserts,
            tombstones,
            live_docs_after: docs,
            compaction_recommended: false,
        };

        let started = Instant::now();
        let report = publish_vnext_incremental_generation(&root, &plan, 5_000)?;
        let publish_ms = started.elapsed().as_secs_f64() * 1000.0;

        // Restart-style verification: construct a fresh index only from CURRENT and durable files.
        let reopen_started = Instant::now();
        let reopened = open_vnext_published_generation(&root)?;
        let reopen_ms = reopen_started.elapsed().as_secs_f64() * 1000.0;
        let marker = format!("durable_generation_{generation}_marker");
        let hits = reopened.search_content(marker.as_bytes())?;
        if hits.len() != changes {
            return Err(format!(
                "generation {generation} restart hit mismatch: expected {changes}, got {}",
                hits.len()
            )
            .into());
        }
        println!(
            "VNEXT_DURABLE_DELTA generation={} changes={} publish_ms={publish_ms:.3} reopen_ms={reopen_ms:.3} layers={} segments={} live_docs={} marker_hits={}",
            generation,
            changes,
            report.layer_count,
            report.segment_count,
            report.live_docs,
            hits.len()
        );
    }

    Ok(())
}
