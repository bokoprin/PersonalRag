use personalrag_portable_search::{
    AccelerationProfile, BuildMode, BuildOptions, DocumentInput, build_index_benchmark,
    build_index_unified_benchmark, fold_ascii,
};
use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

fn documents(n: usize) -> Vec<DocumentInput> {
    (0..n)
        .map(|id| {
            let name = format!("changed/{id:06}.txt");
            let mut body = String::with_capacity(1400);
            for round in 0..18 {
                body.push_str("common metadata timeout request response parser builder search ");
                body.push_str(&format!("doc_{id}_round_{round} unique_delta_marker_{id} "));
            }
            DocumentInput::new(
                name.clone(),
                name.clone(),
                fold_ascii(name.as_bytes()),
                fold_ascii(body.as_bytes()),
            )
        })
        .collect()
}

fn main() {
    let args = env::args().collect::<Vec<_>>();
    let mode = args.get(1).map(String::as_str).unwrap_or("adaptive");
    let n = args
        .get(2)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(100usize);
    let out = PathBuf::from(
        args.get(3)
            .cloned()
            .unwrap_or_else(|| format!("/tmp/pr-delta-{mode}-{n}")),
    );
    let _ = fs::remove_dir_all(&out);
    let docs = documents(n);
    let options = BuildOptions {
        mode: BuildMode::Direct,
        segment_docs: 5_000,
        workers: 4,
    };
    let started = Instant::now();
    match mode {
        "base" => {
            build_index_benchmark(&docs, &out, &options).unwrap();
        }
        "adaptive" => {
            build_index_unified_benchmark(
                &docs,
                &out,
                &options,
                AccelerationProfile::AdaptiveDelta,
            )
            .unwrap();
        }
        _ => panic!("mode"),
    }
    let mut sidecars = 0usize;
    let bytes = fs::read_dir(&out)
        .unwrap()
        .map(|entry| {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|ext| {
                ext == "q2c"
                    || ext == "pos2"
                    || ext == "pos3"
                    || ext.to_string_lossy().starts_with("pos-")
            }) {
                sidecars += 1;
            }
            fs::metadata(path).unwrap().len()
        })
        .sum::<u64>();
    println!(
        "mode={mode} docs={n} elapsed_ms={:.3} bytes={bytes} sidecars={sidecars}",
        started.elapsed().as_secs_f64() * 1000.0
    );
}
