use personalrag_v2::extraction::ExtractorConfig;
use personalrag_v2::product;
#[cfg(not(windows))]
use personalrag_v2::product::ProductError;
use std::ffi::OsString;
#[cfg(windows)]
use std::io::{self, Write};
use std::path::PathBuf;
#[cfg(windows)]
use std::thread;
#[cfg(windows)]
use std::time::Duration;

#[derive(Debug)]
struct CommonArgs {
    root: PathBuf,
    store: PathBuf,
    extractor: ExtractorConfig,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("ERROR: {error}");
        std::process::exit(2);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let Some(command) = args.next() else {
        return Err(usage().into());
    };
    let command = command.to_string_lossy();
    match command.as_ref() {
        "init" => {
            let common = parse_common(args.collect())?;
            let report = product::initialize_store(&common.root, &common.store, &common.extractor)?;
            println!(
                "INIT_OK bundle={} metadata={} searchable={} store_bytes={} usn_available={} journal_id={} next_usn={}",
                report.manifest.generation,
                report.metadata_records,
                report.searchable_records,
                report.store_bytes,
                report.usn_available,
                report.checkpoint.journal_id,
                report.checkpoint.next_usn
            );
        }
        "update" => {
            let common = parse_common(args.collect())?;
            let report =
                product::reconcile_store(&common.root, &common.store, &common.extractor, None)?;
            println!(
                "UPDATE_OK committed={} compacted={} bundle={} metadata={} delta_changes={}",
                report.committed,
                report.compacted,
                report.manifest.generation,
                report.metadata_records,
                report.delta_changes
            );
        }
        "status" => {
            let common = parse_common(args.collect())?;
            let loaded =
                product::load_product_bundle(&common.root, &common.store, &common.extractor)?;
            println!(
                "STATUS_OK bundle={} content={} metadata={} delta={} state={} metadata_records={} delta_changes={} journal_id={} next_usn={}",
                loaded.manifest.generation,
                loaded.manifest.content_generation,
                loaded.manifest.metadata_generation,
                loaded.manifest.delta_generation,
                loaded.manifest.state_generation,
                loaded.metadata.records().len(),
                loaded.delta.change_count(),
                loaded.state.checkpoint.journal_id,
                loaded.state.checkpoint.next_usn
            );
        }
        "helpers" => {
            if args.next().is_some() {
                return Err("helpers takes no arguments".into());
            }
            let config = ExtractorConfig::discover();
            print_helper("pdftotext", &config.pdftotext, "-v");
            print_helper("unzip", &config.unzip, "-v");
            print_helper("zstd", &config.zstd, "--version");
        }
        "watch" => run_watch(args.collect())?,
        "--help" | "-h" | "help" => println!("{}", usage()),
        other => return Err(format!("unknown command: {other}\n\n{}", usage()).into()),
    }
    Ok(())
}

fn parse_common(values: Vec<OsString>) -> Result<CommonArgs, Box<dyn std::error::Error>> {
    let mut root = std::env::var_os("PERSONALRAG_ROOT").map(PathBuf::from);
    let mut store = std::env::var_os("PERSONALRAG_STORE").map(PathBuf::from);
    let mut extractor = ExtractorConfig::discover();
    let mut args = values.into_iter();
    while let Some(arg) = args.next() {
        match arg.to_string_lossy().as_ref() {
            "--root" => root = Some(PathBuf::from(args.next().ok_or("--root requires a path")?)),
            "--store" => store = Some(PathBuf::from(args.next().ok_or("--store requires a path")?)),
            "--pdftotext" => {
                extractor.override_pdftotext(args.next().ok_or("--pdftotext requires a path")?)
            }
            "--unzip" => extractor.override_unzip(args.next().ok_or("--unzip requires a path")?),
            "--zstd" => extractor.override_zstd(args.next().ok_or("--zstd requires a path")?),
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    Ok(CommonArgs {
        root: root.ok_or("--root is required")?,
        store: store.ok_or("--store is required")?,
        extractor,
    })
}

fn run_watch(values: Vec<OsString>) -> Result<(), Box<dyn std::error::Error>> {
    let mut interval_ms = 250_u64;
    let mut once = false;
    let mut common_values = Vec::new();
    let mut iter = values.into_iter();
    while let Some(arg) = iter.next() {
        match arg.to_string_lossy().as_ref() {
            "--interval-ms" => {
                interval_ms = iter
                    .next()
                    .ok_or("--interval-ms requires a number")?
                    .to_string_lossy()
                    .parse()?;
            }
            "--once" => once = true,
            _ => {
                common_values.push(arg);
                if matches!(
                    common_values.last().unwrap().to_string_lossy().as_ref(),
                    "--root" | "--store" | "--pdftotext" | "--unzip" | "--zstd"
                ) {
                    common_values.push(iter.next().ok_or("missing value for option")?);
                }
            }
        }
    }
    let common = parse_common(common_values)?;
    run_watch_platform(common, interval_ms.max(25), once)
}

#[cfg(windows)]
fn run_watch_platform(
    common: CommonArgs,
    interval_ms: u64,
    once: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut producer = personalrag_v2::product::WindowsWatchProducer::open(
        &common.root,
        &common.store,
        common.extractor,
    )?;
    let checkpoint = producer.checkpoint();
    println!(
        "WATCH_READY mode={} journal_id={} next_usn={} interval_ms={} fallback_reason={:?}",
        producer.mode().as_str(),
        checkpoint.journal_id,
        checkpoint.next_usn,
        interval_ms,
        producer.fallback_reason()
    );
    io::stdout().flush()?;
    loop {
        if let Some(report) = producer.poll_once()? {
            println!(
                "WATCH_UPDATE committed={} compacted={} bundle={} metadata={} delta_changes={}",
                report.committed,
                report.compacted,
                report.manifest.generation,
                report.metadata_records,
                report.delta_changes
            );
            io::stdout().flush()?;
        }
        if once {
            break;
        }
        thread::sleep(Duration::from_millis(interval_ms));
    }
    Ok(())
}

#[cfg(not(windows))]
fn run_watch_platform(
    _common: CommonArgs,
    _interval_ms: u64,
    _once: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    Err(ProductError::Unsupported(
        "watch requires native Windows; it prefers NTFS USN and falls back to directory notifications when raw USN access is unavailable".to_string(),
    )
    .into())
}

fn print_helper(name: &str, path: &std::path::Path, version_arg: &str) {
    let outcome = std::process::Command::new(path).arg(version_arg).output();
    match outcome {
        Ok(output) => {
            let version = if output.stdout.is_empty() {
                String::from_utf8_lossy(&output.stderr).trim().to_string()
            } else {
                String::from_utf8_lossy(&output.stdout).trim().to_string()
            };
            println!(
                "HELPER name={} available=true path={} version={:?}",
                name,
                path.display(),
                version.lines().next().unwrap_or("")
            );
        }
        Err(error) => println!(
            "HELPER name={} available=false path={} error={:?}",
            name,
            path.display(),
            error.to_string()
        ),
    }
}

fn usage() -> String {
    concat!(
        "PersonalRag V2 index lifecycle\n",
        "Usage:\n",
        "  personalrag-v2-indexer init   --root <indexed-root> --store <index-store> [helper overrides]\n",
        "  personalrag-v2-indexer update --root <indexed-root> --store <index-store> [helper overrides]\n",
        "  personalrag-v2-indexer watch  --root <indexed-root> --store <index-store> [--interval-ms 250] [--once] [helper overrides]\n",
        "  personalrag-v2-indexer status --root <indexed-root> --store <index-store> [helper overrides]\n",
        "  personalrag-v2-indexer helpers\n",
        "Helper overrides: --pdftotext <path> --unzip <path> --zstd <path>\n",
        "Environment: PERSONALRAG_ROOT, PERSONALRAG_STORE, PERSONALRAG_PDFTOTEXT, PERSONALRAG_UNZIP, PERSONALRAG_ZSTD"
    )
    .to_string()
}
