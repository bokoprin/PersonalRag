use personalrag_v2::incremental::{DeltaOverlay, load_delta_generation, write_delta_generation};
use personalrag_v2::{MetadataFileKind, MetadataIndex, MetadataRecord, MetadataSearchRequest};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn record(id: u64, path: String) -> MetadataRecord {
    MetadataRecord {
        file_id: id,
        path: PathBuf::from(path),
        source_root: 0,
        size: 128,
        modified_ns: 1,
        kind: MetadataFileKind::File,
        content_searchable: false,
        extractable: false,
    }
}

fn percentile(values: &mut [Duration], pct: usize) -> Duration {
    values.sort_unstable();
    let index = ((values.len() - 1) * pct).div_ceil(100);
    values[index]
}

fn main() {
    let count = 1_000_000_u64;
    let storm = 10_000_u64;
    let repeats = 21_usize;
    let started = Instant::now();
    let records = (0..count)
        .map(|id| record(id, format!("root/dir_{:04}/file_{id:07}.txt", id % 4096)))
        .collect::<Vec<_>>();
    let base = MetadataIndex::build(records).expect("build base metadata");
    let build = started.elapsed();
    let mut delta = DeltaOverlay::new(&base, 2, 1);

    let create_start = Instant::now();
    for index in 0..storm {
        let id = count + index;
        delta.upsert(
            &base,
            record(id, format!("root/new/create_{index:05}.txt")),
            true,
        );
    }
    let create = create_start.elapsed();

    let rename_start = Instant::now();
    for id in 0..storm {
        delta
            .rename(
                &base,
                id,
                PathBuf::from(format!("root/renamed/renamed_{id:05}.txt")),
            )
            .expect("rename");
    }
    let rename = rename_start.elapsed();

    let delete_start = Instant::now();
    for id in storm..storm * 2 {
        delta.delete(&base, id);
    }
    let delete = delete_start.elapsed();

    let temp = std::env::temp_dir().join(format!(
        "personalrag-step4-storm-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&temp).unwrap();
    let publish_start = Instant::now();
    let delta_path = write_delta_generation(&temp, &delta.snapshot()).expect("publish delta");
    let publish = publish_start.elapsed();
    let delta_bytes = fs::metadata(&delta_path).unwrap().len();
    let reload_start = Instant::now();
    let loaded = load_delta_generation(&temp, 2).expect("load delta");
    let restored = DeltaOverlay::from_snapshot(&base, loaded);
    let reload = reload_start.elapsed();

    let cases = [
        (
            "created",
            MetadataSearchRequest::filename("create_09999"),
            1_usize,
        ),
        (
            "renamed_new",
            MetadataSearchRequest::path("root/renamed/renamed_09999.txt"),
            1,
        ),
        (
            "renamed_old",
            MetadataSearchRequest::path("root/dir_1807/file_0009999.txt"),
            0,
        ),
        (
            "deleted",
            MetadataSearchRequest::filename("file_0019999.txt"),
            0,
        ),
        (
            "base_rare",
            MetadataSearchRequest::filename("file_0999999.txt"),
            1,
        ),
    ];
    for (name, request, expected) in cases {
        let mut samples = Vec::with_capacity(repeats);
        let mut result_count = 0;
        for _ in 0..repeats {
            let start = Instant::now();
            let hits = restored.metadata_search(
                &base,
                MetadataSearchRequest {
                    filename: request.filename,
                    full_path: request.full_path,
                    case_sensitive: request.case_sensitive,
                    max_results: request.max_results,
                },
            );
            samples.push(start.elapsed());
            result_count = hits.len();
        }
        assert_eq!(result_count, expected, "case {name}");
        let p50 = percentile(&mut samples, 50);
        let max = *samples.iter().max().unwrap();
        println!(
            "SEARCH case={name} p50_ms={:.3} max_ms={:.3} hits={result_count}",
            p50.as_secs_f64() * 1000.0,
            max.as_secs_f64() * 1000.0
        );
    }

    println!(
        "STORM base_records={} base_bytes={} build_ms={:.3} create_count={} create_ms={:.3} rename_count={} rename_ms={:.3} delete_count={} delete_ms={:.3} delta_changes={} compact={} delta_bytes={} publish_ms={:.3} reload_ms={:.3}",
        count,
        base.persistent_bytes(),
        build.as_secs_f64() * 1000.0,
        storm,
        create.as_secs_f64() * 1000.0,
        storm,
        rename.as_secs_f64() * 1000.0,
        storm,
        delete.as_secs_f64() * 1000.0,
        restored.change_count(),
        restored.should_compact(count as usize),
        delta_bytes,
        publish.as_secs_f64() * 1000.0,
        reload.as_secs_f64() * 1000.0,
    );
    let _ = fs::remove_dir_all(temp);
}
