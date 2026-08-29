use personalrag_v2::{MetadataIndex, MetadataRecord, MetadataSearchRequest};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn percentile(values: &mut [Duration], pct: usize) -> Duration {
    values.sort_unstable();
    let index = ((values.len() - 1) * pct).div_ceil(100);
    values[index.min(values.len() - 1)]
}

fn ms(value: Duration) -> f64 {
    value.as_secs_f64() * 1000.0
}

fn current_rss_kb() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/status").ok()?;
    text.lines().find_map(|line| {
        let value = line.strip_prefix("VmRSS:")?.trim();
        value.split_whitespace().next()?.parse().ok()
    })
}
fn make_records(count: usize) -> Vec<MetadataRecord> {
    let mut records = Vec::with_capacity(count);
    for i in 0..count {
        let project = (i / 1000) % 1000;
        let module = (i / 100) % 100;
        let filename = if i == 42 {
            format!("Straße_設計_{i:08}.txt")
        } else if i + 1 == count {
            format!("rare_wxyz_klmno_{i:08}.txt")
        } else {
            format!("report_{i:08}_alpha_beta.txt")
        };
        let path = PathBuf::from(format!(
            "C:/Users/Test/Documents/project_{project:03}/module_{module:02}/{filename}"
        ));
        let mut record =
            MetadataRecord::file(i as u64 + 1000, path, (i % 1_000_000) as u64, i as u128);
        record.source_root = 1;
        record.content_searchable = true;
        records.push(record);
    }
    records
}

fn run_case(index: &MetadataIndex, label: &str, request: MetadataSearchRequest<'_>) {
    let warm = index.search(request.clone());
    let mut elapsed = Vec::with_capacity(21);
    let mut representative = warm.metrics.clone();
    for _ in 0..21 {
        let start = Instant::now();
        let outcome = index.search(request.clone());
        elapsed.push(start.elapsed());
        representative = outcome.metrics;
    }
    let p50 = percentile(&mut elapsed.clone(), 50);
    let max = *elapsed.iter().max().unwrap();
    println!(
        "case={label} p50_ms={:.3} max_ms={:.3} returned={} candidates={} verified={} anchor_width={:?} anchor_df={:?} absent={}",
        ms(p50),
        ms(max),
        representative.returned_records,
        representative.candidate_records,
        representative.verified_records,
        representative.selected_anchor_width,
        representative.selected_anchor_df,
        representative.global_absent_shortcut,
    );
}

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    if args.get(1).is_some_and(|value| value == "load") {
        let path = PathBuf::from(args.get(2).expect("load path"));
        let start = Instant::now();
        let index = MetadataIndex::load_snapshot(&path).expect("load metadata snapshot");
        println!(
            "load_only_ms={:.3} loaded_records={} persistent_bytes={} bytes_per_record={:.3}",
            ms(start.elapsed()),
            index.records().len(),
            index.persistent_bytes(),
            index.bytes_per_record(),
        );
        if let Some(rss) = current_rss_kb() {
            println!("steady_rss_kb={rss}");
        }
        run_case(
            &index,
            "load_only_zero",
            MetadataSearchRequest {
                filename: Some("QZX_NEVER_PRESENT"),
                max_results: 100,
                ..MetadataSearchRequest::default()
            },
        );
        return;
    }
    let count = args
        .get(1)
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(100_000);
    let output = args.get(2).map(PathBuf::from).unwrap_or_else(|| {
        std::env::temp_dir().join(format!("personalrag-metadata-{count}.prv2meta"))
    });
    let _ = std::fs::remove_file(&output);

    println!("records={count}");
    let records = make_records(count);
    let build_start = Instant::now();
    let index = MetadataIndex::build(records).expect("metadata build");
    let build = build_start.elapsed();
    println!(
        "build_ms={:.3} persistent_bytes={} bytes_per_record={:.3}",
        ms(build),
        index.persistent_bytes(),
        index.bytes_per_record()
    );

    let publish_start = Instant::now();
    let written = index
        .write_snapshot(&output)
        .expect("write metadata snapshot");
    let publish = publish_start.elapsed();
    drop(index);
    let load_start = Instant::now();
    let index = MetadataIndex::load_snapshot(&output).expect("load metadata snapshot");
    let load = load_start.elapsed();
    println!(
        "publish_ms={:.3} load_ms={:.3} written_bytes={} loaded_records={}",
        ms(publish),
        ms(load),
        written,
        index.records().len()
    );

    let rare_id = count.saturating_sub(1);
    let rare_query = format!("{rare_id:08}");
    run_case(
        &index,
        "rare_filename",
        MetadataSearchRequest {
            filename: Some(&rare_query),
            max_results: 100,
            ..MetadataSearchRequest::default()
        },
    );
    run_case(
        &index,
        "unicode_filename",
        MetadataSearchRequest {
            filename: Some("STRASSE"),
            max_results: 100,
            ..MetadataSearchRequest::default()
        },
    );
    run_case(
        &index,
        "zero_hit_q3",
        MetadataSearchRequest {
            filename: Some("QZX_NEVER_PRESENT"),
            max_results: 100,
            ..MetadataSearchRequest::default()
        },
    );
    run_case(
        &index,
        "zero_one_char",
        MetadataSearchRequest {
            filename: Some("~"),
            max_results: 100,
            ..MetadataSearchRequest::default()
        },
    );
    run_case(
        &index,
        "zero_two_char",
        MetadataSearchRequest {
            filename: Some("@@"),
            max_results: 100,
            ..MetadataSearchRequest::default()
        },
    );
    run_case(
        &index,
        "common_one_char",
        MetadataSearchRequest {
            filename: Some("a"),
            max_results: 100,
            ..MetadataSearchRequest::default()
        },
    );
    run_case(
        &index,
        "common_two_char",
        MetadataSearchRequest {
            filename: Some("re"),
            max_results: 100,
            ..MetadataSearchRequest::default()
        },
    );
    run_case(
        &index,
        "path",
        MetadataSearchRequest {
            full_path: Some("project_042/module_27"),
            max_results: 100,
            ..MetadataSearchRequest::default()
        },
    );
    run_case(
        &index,
        "filename_path_and",
        MetadataSearchRequest {
            filename: Some("report"),
            full_path: Some("project_042/module_27"),
            max_results: 100,
            ..MetadataSearchRequest::default()
        },
    );

    if std::env::var_os("PR_META_KEEP").is_none() {
        let _ = std::fs::remove_file(output);
    }
}
