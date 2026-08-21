#[cfg(windows)]
fn main() {
    use personalrag_gui_bridge_core::{scan_files, ScanExclusions, ScannerMode};
    use std::{
        path::PathBuf,
        sync::{atomic::AtomicBool, Arc},
        time::Instant,
    };

    let root = std::env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().expect("current directory"));
    let rounds = std::env::var("PR_SCANNER_BENCH_ROUNDS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(7)
        .max(3);
    let config = ScanExclusions::default();

    let mut oracle = None;
    let mut mode_medians = Vec::new();
    for mode in [ScannerMode::WalkDir, ScannerMode::WindowsNative] {
        let mut samples = Vec::with_capacity(rounds);
        for _ in 0..rounds {
            let started = Instant::now();
            let report = scan_files(
                &root,
                0,
                mode,
                &config,
                Arc::new(AtomicBool::new(false)),
                Arc::new(|_| {}),
            )
            .expect("scan");
            samples.push(started.elapsed().as_secs_f64() * 1000.0);
            let mut paths = report
                .files
                .into_iter()
                .map(|file| (file.display_path, file.size_bytes, file.index_content))
                .collect::<Vec<_>>();
            paths.sort();
            if let Some(expected) = &oracle {
                assert_eq!(&paths, expected, "scanner result mismatch");
            } else {
                oracle = Some(paths);
            }
        }
        samples.sort_by(f64::total_cmp);
        mode_medians.push(samples[samples.len() / 2]);
    }
    let walk = mode_medians[0];
    let native = mode_medians[1];
    println!(
        "WINDOWS_NATIVE_SCANNER_BENCH root={} rounds={} walk_ms={walk:.3} native_ms={native:.3} speedup={:.3}",
        root.display(),
        rounds,
        walk / native
    );
}

#[cfg(not(windows))]
fn main() {
    println!("WINDOWS_NATIVE_SCANNER_BENCH WINDOWS_ONLY");
}
