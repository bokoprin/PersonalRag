use personalrag_portable_search::{
    BuildMode, BuildOptions, DocumentInput, PersistentIndex, VNextDocumentInput,
    VNextSegmentReader, build_index_benchmark, fold_ascii, write_vnext_segment,
};
use std::fs;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn make_name(index: usize) -> String {
    format!(
        "assets/group_{:04}/component_{:05}/repeated_component_component_{:06}.png",
        index % 4096,
        index % 65536,
        index
    )
}

fn percentile(samples: &mut [Duration], p: f64) -> f64 {
    samples.sort_unstable();
    let i = ((samples.len() - 1) as f64 * p).round() as usize;
    samples[i].as_secs_f64() * 1000.0
}

fn bench<F: FnMut() -> Vec<u32>>(rounds: usize, mut f: F) -> (Vec<u32>, f64, f64) {
    let expected = f();
    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let t = Instant::now();
        black_box(f());
        samples.push(t.elapsed());
    }
    let mut p50s = samples.clone();
    (
        expected,
        percentile(&mut p50s, 0.5),
        percentile(&mut samples, 0.95),
    )
}

fn build_perf(n: usize, root: &Path) {
    let docs = (0..n)
        .map(|i| {
            let name = make_name(i);
            DocumentInput::new(
                name.clone(),
                name.clone(),
                fold_ascii(name.as_bytes()),
                Vec::new(),
            )
        })
        .collect::<Vec<_>>();
    let out = root.join("perf12");
    let _ = fs::remove_dir_all(&out);
    let r = build_index_benchmark(
        &docs,
        &out,
        &BuildOptions {
            mode: BuildMode::Adaptive,
            segment_docs: 50_000,
            workers: 4,
        },
    )
    .unwrap();
    println!(
        "PERF_NAME100K docs={} segments={} elapsed_ms={:.3} bytes={}",
        r.docs,
        r.segments,
        r.elapsed.as_secs_f64() * 1000.0,
        r.index_bytes
    );
}

fn build_vnext(n: usize, root: &Path) {
    let docs = (0..n)
        .map(|i| VNextDocumentInput::new(i as u64, make_name(i), Vec::<u8>::new()))
        .collect::<Vec<_>>();
    let out = root.join("vnext");
    let _ = fs::remove_dir_all(&out);
    fs::create_dir_all(&out).unwrap();
    let t = Instant::now();
    let reports = std::thread::scope(|scope| {
        let handles = docs
            .chunks(50_000)
            .enumerate()
            .map(|(seg, chunk)| {
                let path = out.join(format!("segment-{seg:04}.prseg2"));
                scope.spawn(move || write_vnext_segment(&path, chunk))
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|h| h.join().unwrap().unwrap())
            .collect::<Vec<_>>()
    });
    let elapsed = t.elapsed().as_secs_f64() * 1000.0;
    let bytes = reports.iter().map(|r| r.file_bytes).sum::<u64>();
    println!(
        "VNEXT_NAME100K docs={n} segments={} elapsed_ms={elapsed:.3} bytes={bytes}",
        reports.len()
    );
}

fn compare(n: usize, rounds: usize, root: &Path) {
    let perf = PersistentIndex::open(root.join("perf12"), true).unwrap();
    let vdir = root.join("vnext");
    let mut paths = fs::read_dir(&vdir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect::<Vec<_>>();
    paths.sort();
    let readers = paths
        .iter()
        .map(|p| VNextSegmentReader::open(p).unwrap())
        .collect::<Vec<_>>();
    for q in [
        b"component_00042".as_slice(),
        b"group_0042",
        b"repeated_component_component_099999",
        b"png",
        b"missing_name_marker",
    ] {
        let (ph, pp50, pp95) = bench(rounds, || perf.search_name(q).unwrap());
        let (vh, vp50, vp95) = bench(rounds, || {
            let mut out = Vec::new();
            let mut base = 0u32;
            for r in &readers {
                out.extend(
                    r.search_path(q)
                        .unwrap()
                        .into_iter()
                        .map(|id| base + u32::from(id)),
                );
                base += r.doc_count();
            }
            out
        });
        assert_eq!(
            ph,
            vh,
            "name query mismatch {:?}",
            String::from_utf8_lossy(q)
        );
        println!(
            "NAME_QUERY q={} hits={} perf_p50_ms={pp50:.6} perf_p95_ms={pp95:.6} vnext_p50_ms={vp50:.6} vnext_p95_ms={vp95:.6}",
            String::from_utf8_lossy(q),
            ph.len()
        );
    }
    let _ = n;
}

fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    let mode = args.get(1).map(String::as_str).unwrap_or("all");
    let n = args
        .get(2)
        .and_then(|v| v.parse().ok())
        .unwrap_or(100_000usize);
    let root = PathBuf::from(
        args.get(3)
            .cloned()
            .unwrap_or_else(|| "/tmp/pr-vnext-name100k".into()),
    );
    let rounds = args.get(4).and_then(|v| v.parse().ok()).unwrap_or(31usize);
    fs::create_dir_all(&root).unwrap();
    match mode {
        "perf" => build_perf(n, &root),
        "vnext" => build_vnext(n, &root),
        "compare" => compare(n, rounds, &root),
        "all" => {
            build_perf(n, &root);
            build_vnext(n, &root);
            compare(n, rounds, &root);
        }
        _ => panic!("mode must be perf|vnext|compare|all"),
    }
}
