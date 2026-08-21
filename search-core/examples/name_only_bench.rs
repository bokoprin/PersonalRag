use personalrag_portable_search::{
    BuildMode, BuildOptions, DocumentInput, build_index_benchmark, fold_ascii,
};
use std::{env, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = env::args().collect::<Vec<_>>();
    let docs: usize = args.get(1).map_or(Ok(100_000usize), |v| v.parse())?;
    let output = args
        .get(2)
        .map_or_else(|| PathBuf::from("target/name-only-bench"), PathBuf::from);
    let segment_docs: usize = args.get(3).map_or(Ok(50_000usize), |v| v.parse())?;
    let workers: usize = args.get(4).map_or(Ok(1usize), |v| v.parse())?;
    let mut documents = Vec::with_capacity(docs);
    for index in 0..docs {
        let display = format!(
            "assets/group_{:04}/component_{:05}/repeated_component_component_{:06}.png",
            index % 4096,
            index % 65536,
            index
        );
        documents.push(DocumentInput::new(
            display.clone(),
            display.clone(),
            fold_ascii(display.as_bytes()),
            Vec::new(),
        ));
    }
    let report = build_index_benchmark(
        &documents,
        &output,
        &BuildOptions {
            mode: BuildMode::Adaptive,
            segment_docs,
            workers,
        },
    )?;
    println!(
        "NAME_ONLY_BENCH docs={} segments={} elapsed_ms={:.3} index_bytes={}",
        report.docs,
        report.segments,
        report.elapsed.as_secs_f64() * 1000.0,
        report.index_bytes
    );
    Ok(())
}
