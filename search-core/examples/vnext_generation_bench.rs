use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use personalrag_portable_search::{
    VNextDocumentInput, VNextGenerationIndex, VNextGenerationLayerSpec, VNextSegmentReader,
    fold_ascii, write_vnext_segment,
};

fn doc(id: u64, path: String, content: String) -> VNextDocumentInput {
    VNextDocumentInput::new(id, path, fold_ascii(content.as_bytes()))
}

fn write(path: &Path, docs: &[VNextDocumentInput]) {
    write_vnext_segment(path, docs).unwrap();
}

fn logical_hits(reader: &VNextSegmentReader, local: Vec<u16>) -> Vec<u64> {
    let mut out = local
        .into_iter()
        .map(|doc_id| reader.logical_id(doc_id).unwrap())
        .collect::<Vec<_>>();
    out.sort_unstable();
    out.dedup();
    out
}

fn main() {
    let args = env::args().skip(1).collect::<Vec<_>>();
    let docs = args
        .first()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(20_000);
    let changed = args
        .get(1)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(100)
        .min(docs / 4);
    let root = args.get(2).map_or_else(
        || {
            env::temp_dir().join(format!(
                "personalrag-vnext-generation-bench-{}",
                std::process::id()
            ))
        },
        PathBuf::from,
    );
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();

    let base_docs = (1..=docs as u64)
        .map(|id| {
            doc(
                id,
                format!("base/group_{:03}/module_{id:06}.txt", id % 97),
                format!("timeout common payload old_version_id_{id:06}_only base-marker-{id:06}"),
            )
        })
        .collect::<Vec<_>>();
    let split = base_docs.len() / 2;
    let base_a = root.join("base-a.prseg2");
    let base_b = root.join("base-b.prseg2");
    write(&base_a, &base_docs[..split]);
    write(&base_b, &base_docs[split..]);

    let delete_count = changed / 2;
    let mut tombstones = (1..=changed as u64).collect::<Vec<_>>();
    tombstones.extend((changed as u64 + 1)..=(changed + delete_count) as u64);
    tombstones.sort_unstable();
    tombstones.dedup();
    let mut delta_docs = (1..=changed as u64)
        .map(|id| {
            doc(
                id,
                format!("updated/group_{:03}/module_{id:06}.txt", id % 97),
                format!("timeout common payload updated_generation_id_{id:06}_only"),
            )
        })
        .collect::<Vec<_>>();
    delta_docs.extend((1..=delete_count as u64).map(|offset| {
        let id = docs as u64 + offset;
        doc(
            id,
            format!("new/module_{id:06}.txt"),
            format!("timeout common payload inserted_generation_id_{id:06}_only"),
        )
    }));
    let delta = root.join("delta.prseg2");
    write(&delta, &delta_docs);

    let open_started = Instant::now();
    let generation = VNextGenerationIndex::open(
        1,
        &[
            VNextGenerationLayerSpec::base(0, [&base_a, &base_b]),
            VNextGenerationLayerSpec::delta(1, [&delta], tombstones),
        ],
    )
    .unwrap();
    let open_ms = open_started.elapsed().as_secs_f64() * 1000.0;

    let materialized = generation.materialize_live_documents().unwrap();
    let full_path = root.join("full-rebuild.prseg2");
    write(&full_path, &materialized);
    let full = VNextSegmentReader::open(&full_path).unwrap();

    let updated_id = 1u64;
    let deleted_id = changed as u64 + 1;
    let untouched_id = docs as u64;
    let inserted_id = docs as u64 + 1;
    let content_queries = [
        "timeout common".to_owned(),
        format!("old_version_id_{updated_id:06}_only"),
        format!("updated_generation_id_{updated_id:06}_only"),
        format!("old_version_id_{deleted_id:06}_only"),
        format!("base-marker-{untouched_id:06}"),
        format!("inserted_generation_id_{inserted_id:06}_only"),
        "definitely-not-present-generation-query".to_owned(),
    ];
    for query in &content_queries {
        let started = Instant::now();
        let generation_hits = generation.search_content(query.as_bytes()).unwrap();
        let elapsed_us = started.elapsed().as_secs_f64() * 1_000_000.0;
        let full_hits = logical_hits(&full, full.search_content(query.as_bytes()).unwrap());
        assert_eq!(generation_hits, full_hits, "content query={query}");
        println!(
            "GEN_QUERY kind=content query={query:?} hits={} elapsed_us={elapsed_us:.3}",
            generation_hits.len()
        );
    }

    let path_queries = [
        format!(
            "updated/group_{:03}/module_{updated_id:06}",
            updated_id % 97
        ),
        format!("base/group_{:03}/module_{updated_id:06}", updated_id % 97),
        format!("base/group_{:03}/module_{deleted_id:06}", deleted_id % 97),
        format!("new/module_{inserted_id:06}"),
    ];
    for query in &path_queries {
        let generation_hits = generation.search_path(query.as_bytes()).unwrap();
        let full_hits = logical_hits(&full, full.search_path(query.as_bytes()).unwrap());
        assert_eq!(generation_hits, full_hits, "path query={query}");
    }

    println!(
        "VNEXT_GENERATION_BENCH_PASS docs={docs} changed={changed} layers={} segments={} live_docs={} tombstone_events={} open_ms={open_ms:.3}",
        generation.layer_count(),
        generation.segment_count(),
        generation.live_docs(),
        generation.tombstone_events(),
    );
}
