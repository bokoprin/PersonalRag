use personalrag_v2::{load_latest, publish_generation};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn percentile(values: &mut [Duration], percentile: usize) -> Duration {
    values.sort_unstable();
    let index = ((values.len() - 1) * percentile).div_ceil(100);
    values[index.min(values.len() - 1)]
}

fn ms(value: Duration) -> f64 {
    value.as_secs_f64() * 1000.0
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(
        args.next()
            .ok_or("usage: persistent-bench ROOT STORE [REPEATS]")?,
    );
    let store = PathBuf::from(args.next().ok_or("missing STORE")?);
    let repeats = args
        .next()
        .map(|value| value.parse())
        .transpose()?
        .unwrap_or(21_usize)
        .max(1);
    if store.exists() {
        std::fs::remove_dir_all(&store)?;
    }
    let publish_started = Instant::now();
    let published = publish_generation(&root, &store, 1, 0)?;
    let publish_time = publish_started.elapsed();
    let load_started = Instant::now();
    let index = load_latest(&root, &store)?;
    let load_time = load_started.elapsed();
    println!(
        "source_bytes={} index_bytes={} ratio={:.6}% q45_bytes={} blocks={} publish_ms={:.3} load_ms={:.3}",
        published.capacity.selected_source_bytes,
        published.capacity.total_index_bytes,
        published.capacity.index_source_ratio() * 100.0,
        published.capacity.q45_bytes,
        published.capacity.block_count,
        ms(publish_time),
        ms(load_time),
    );
    for query in ["abd", "wxyz", "klmno", "abcde", "日本語", "STRASSE", "CAFÉ"] {
        let warm = index.search_first_batch(query, false)?;
        let mut samples = Vec::with_capacity(repeats);
        let mut representative = warm.metrics;
        for run in 0..repeats {
            let started = Instant::now();
            let outcome = index.search_first_batch(query, false)?;
            samples.push(started.elapsed());
            if run == 0 {
                representative = outcome.metrics;
            }
        }
        let max = *samples.iter().max().unwrap();
        let p50 = percentile(&mut samples, 50);
        println!(
            "query={query:?} p50_ms={:.3} max_ms={:.3} candidate_blocks={} candidate_bytes={} verification_bytes={} absent={} anchor_df={:?} anchor_width={:?}",
            ms(p50),
            ms(max),
            representative.candidate_blocks,
            representative.candidate_bytes,
            representative.verification_bytes,
            representative.global_absent_shortcut,
            representative.selected_anchor_df,
            representative.selected_anchor_width,
        );
    }

    for (kind, pattern) in [
        ("regex", r"UNIQUE_V2_SENTINEL_[0-9A-F]{4}"),
        ("wildcard", "UNIQUE_V2_SENTINEL_*"),
    ] {
        let warm = if kind == "regex" {
            index.search_regex_first_batch(pattern, false)?
        } else {
            index.search_wildcard_first_batch(pattern, false)?
        };
        let mut samples = Vec::with_capacity(repeats);
        let mut representative = warm.metrics;
        for run in 0..repeats {
            let started = Instant::now();
            let outcome = if kind == "regex" {
                index.search_regex_first_batch(pattern, false)?
            } else {
                index.search_wildcard_first_batch(pattern, false)?
            };
            samples.push(started.elapsed());
            if run == 0 {
                representative = outcome.metrics;
            }
        }
        let max = *samples.iter().max().unwrap();
        let p50 = percentile(&mut samples, 50);
        println!(
            "kind={kind} pattern={pattern:?} p50_ms={:.3} max_ms={:.3} candidate_blocks={} candidate_bytes={} verification_bytes={} absent={} anchor_df={:?} anchor_width={:?}",
            ms(p50),
            ms(max),
            representative.candidate_blocks,
            representative.candidate_bytes,
            representative.verification_bytes,
            representative.global_absent_shortcut,
            representative.selected_anchor_df,
            representative.selected_anchor_width,
        );
    }
    Ok(())
}
