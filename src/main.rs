use personalrag_v2::{Corpus, PrototypeIndex, PrototypeVariant, benchmark_query};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Debug)]
struct Args {
    root: Option<PathBuf>,
    out_dir: PathBuf,
    repeats: usize,
    generate_synthetic_mib: Option<usize>,
    suite: String,
    variants: Vec<PrototypeVariant>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args()?;
    fs::create_dir_all(&args.out_dir)?;
    let root = if let Some(mib) = args.generate_synthetic_mib {
        let root = args.out_dir.join(format!("synthetic-{mib}mib"));
        generate_synthetic_corpus(&root, mib)?;
        root
    } else {
        args.root
            .clone()
            .ok_or("--root is required unless --generate-synthetic-mib is used")?
    };

    let corpus_started = Instant::now();
    let corpus = Corpus::from_directory(&root)?;
    let corpus_elapsed = corpus_started.elapsed();
    let build_started = Instant::now();
    let index = PrototypeIndex::build(&corpus);
    let build_elapsed = build_started.elapsed();

    println!(
        "CORPUS root={} files={} selected_bytes={} searchable_bytes={} blocks={} load_ms={:.3} build_ms={:.3} observed_trigrams={} sparse_anchors={} higher_sparse_anchors={} higher_filter_bytes={} adaptive_global_encoding={}",
        root.display(),
        corpus.file_count(),
        corpus.selected_source_bytes(),
        corpus.searchable_normalized_bytes(),
        corpus.block_count(),
        ms(corpus_elapsed),
        ms(build_elapsed),
        index.observed_trigram_count(),
        index.sparse_anchor_count(),
        index.higher_sparse_anchor_count(),
        index.higher_ngram_filter_bytes(),
        index.adaptive_global_encoding().as_str(),
    );

    for variant in args.variants.iter().copied() {
        let path = args
            .out_dir
            .join(format!("personalrag-v2-{}.prv2", variant.as_str()));
        let capacity = index.write_prototype_index(&corpus, variant, path)?;
        print_capacity(&capacity);
    }

    for case in suite(&args.suite) {
        for variant in args.variants.iter().copied() {
            let stats = benchmark_query(
                &index,
                &corpus,
                variant,
                case.query,
                case.case_sensitive,
                args.repeats,
            );
            let m = &stats.representative_metrics;
            println!(
                "BENCH variant={} case={} case_sensitive={} query={:?} p50_ms={:.3} p95_ms={:.3} p99_ms={:.3} max_ms={:.3} filter_p50_ms={:.3} verify_p50_ms={:.3} assembly_p50_ms={:.3} candidate_blocks={} candidate_bytes={} verification_bytes={} returned_files={} returned_snippets={} matched_seen={} global_absent={} anchor_df={} anchor_width={}",
                variant.as_str(),
                case.name,
                case.case_sensitive,
                case.query,
                ms(stats.p50),
                ms(stats.p95),
                ms(stats.p99),
                ms(stats.max),
                ms(stats.filter_p50),
                ms(stats.verify_p50),
                ms(stats.assembly_p50),
                m.candidate_blocks,
                m.candidate_bytes,
                m.verification_bytes,
                m.returned_files,
                m.returned_snippets,
                m.matched_locations_seen,
                m.global_absent_shortcut,
                m.selected_anchor_df
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "none".to_string()),
                m.selected_anchor_width
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "none".to_string())
            );
        }
    }

    println!(
        "SEMANTICS prototype_scope=plain_utf8_line_units unicode=15.1.0 nfc=true full_case_fold=true wildcard=true regex_nfa=true persistent_format=2 pdf_office_verification_store=true verification_format=1"
    );
    Ok(())
}

fn print_capacity(report: &personalrag_v2::CapacityReport) {
    println!(
        "CAPACITY variant={} selected_bytes={} searchable_bytes={} blocks={} catalog={} block_map={} unigram={} bigram={} global_trigram={} global_encoding={} observed_trigrams={} sparse_meta={} sparse_postings={} higher_filter={} higher_sparse_meta={} higher_sparse_postings={} total={} index_source_ratio={:.6} index_searchable_ratio={:.6}",
        report.variant.as_str(),
        report.selected_source_bytes,
        report.searchable_normalized_bytes,
        report.block_count,
        report.file_catalog_bytes,
        report.block_map_bytes,
        report.unigram_bytes,
        report.bigram_bytes,
        report.global_trigram_presence_bytes,
        report.global_trigram_encoding.as_str(),
        report.observed_trigram_count,
        report.sparse_anchor_metadata_bytes,
        report.sparse_anchor_posting_bytes,
        report.higher_ngram_filter_bytes,
        report.higher_sparse_anchor_metadata_bytes,
        report.higher_sparse_anchor_posting_bytes,
        report.total_persistent_bytes,
        report.index_source_ratio(),
        report.index_searchable_ratio()
    );
}

#[derive(Clone, Copy)]
struct BenchCase {
    name: &'static str,
    query: &'static str,
    case_sensitive: bool,
}

fn suite(name: &str) -> Vec<BenchCase> {
    match name {
        "q45" => vec![
            BenchCase {
                name: "rare-trigram-one-hit",
                query: "abd",
                case_sensitive: false,
            },
            BenchCase {
                name: "rare-q4-one-hit",
                query: "wxyz",
                case_sensitive: false,
            },
            BenchCase {
                name: "rare-q5-one-hit",
                query: "klmno",
                case_sensitive: false,
            },
            BenchCase {
                name: "adversarial-common-grams",
                query: "abcde",
                case_sensitive: false,
            },
        ],
        "synthetic" => vec![
            BenchCase {
                name: "q1-common",
                query: "a",
                case_sensitive: false,
            },
            BenchCase {
                name: "q2-common",
                query: "ab",
                case_sensitive: false,
            },
            BenchCase {
                name: "q3-common",
                query: "abc",
                case_sensitive: false,
            },
            BenchCase {
                name: "long-common",
                query: "personalrag-v2 path=/tmp/cache value=12345",
                case_sensitive: false,
            },
            BenchCase {
                name: "zero-hit",
                query: "QZXJ_V2_ABSENT_7719",
                case_sensitive: false,
            },
            BenchCase {
                name: "rare-trigram-one-hit",
                query: "abd",
                case_sensitive: false,
            },
            BenchCase {
                name: "rare-long-one-hit",
                query: "UNIQUE_V2_SENTINEL_9F3A",
                case_sensitive: false,
            },
            BenchCase {
                name: "rare-q4-one-hit",
                query: "wxyz",
                case_sensitive: false,
            },
            BenchCase {
                name: "rare-q5-one-hit",
                query: "klmno",
                case_sensitive: false,
            },
            BenchCase {
                name: "japanese",
                query: "日本語テキスト",
                case_sensitive: false,
            },
            BenchCase {
                name: "case-insensitive",
                query: "createfilew",
                case_sensitive: false,
            },
            BenchCase {
                name: "case-sensitive",
                query: "CreateFileW",
                case_sensitive: true,
            },
            BenchCase {
                name: "adversarial-common-grams",
                query: "abcde",
                case_sensitive: false,
            },
        ],
        _ => vec![
            BenchCase {
                name: "q1",
                query: "p",
                case_sensitive: false,
            },
            BenchCase {
                name: "q2",
                query: "pr",
                case_sensitive: false,
            },
            BenchCase {
                name: "q3",
                query: "pub",
                case_sensitive: false,
            },
            BenchCase {
                name: "project-name",
                query: "PersonalRag",
                case_sensitive: false,
            },
            BenchCase {
                name: "architecture",
                query: "V2_SEARCH_ARCHITECTURE",
                case_sensitive: false,
            },
            BenchCase {
                name: "zero-hit",
                query: "QZXJ_V2_ABSENT_7719",
                case_sensitive: false,
            },
            BenchCase {
                name: "japanese",
                query: "日本語",
                case_sensitive: false,
            },
            BenchCase {
                name: "case-sensitive",
                query: "CreateFileW",
                case_sensitive: true,
            },
        ],
    }
}

fn generate_synthetic_corpus(root: &Path, mib: usize) -> io::Result<()> {
    if root.exists() {
        fs::remove_dir_all(root)?;
    }
    fs::create_dir_all(root)?;
    let target_per_file = 1024 * 1024;
    let template = concat!(
        "INFO alpha beta gamma delta abc|bcd|cde|ab|bc|cd|de|bd| wxy|xyz klmn|lmno ",
        "CreateFileW createfilew personalrag-v2 path=/tmp/cache value=12345 ",
        "日本語テキスト status=ok module=prototype worker=7 payload=qwertyuiopasdfghjklzxcvbnm0123456789\n"
    );
    for file_index in 0..mib.max(1) {
        let mut file = File::create(root.join(format!("corpus-{file_index:04}.log")))?;
        let mut written = 0_usize;
        if file_index == 17.min(mib.saturating_sub(1)) {
            let sentinel = b"UNIQUE_V2_SENTINEL_9F3A CaseSensitiveToken rare-gram=abd rare-q4=wxyz rare-q5=klmno\n";
            file.write_all(sentinel)?;
            written += sentinel.len();
        }
        while written + template.len() <= target_per_file {
            file.write_all(template.as_bytes())?;
            written += template.len();
        }
        if written < target_per_file {
            let tail = &template.as_bytes()[..target_per_file - written];
            file.write_all(tail)?;
        }
    }
    Ok(())
}

fn parse_args() -> Result<Args, Box<dyn std::error::Error>> {
    let mut root = None;
    let mut out_dir = PathBuf::from("target/personalrag-v2-report");
    let mut repeats = 21_usize;
    let mut generate_synthetic_mib = None;
    let mut suite = String::from("source");
    let mut variants = vec![
        PrototypeVariant::A,
        PrototypeVariant::B,
        PrototypeVariant::C,
        PrototypeVariant::D,
    ];
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => root = Some(PathBuf::from(args.next().ok_or("missing --root value")?)),
            "--out-dir" => out_dir = PathBuf::from(args.next().ok_or("missing --out-dir value")?),
            "--repeats" => repeats = args.next().ok_or("missing --repeats value")?.parse()?,
            "--generate-synthetic-mib" => {
                generate_synthetic_mib = Some(args.next().ok_or("missing synthetic MiB")?.parse()?);
            }
            "--suite" => suite = args.next().ok_or("missing --suite value")?,
            "--variants" => {
                let value = args.next().ok_or("missing --variants value")?;
                variants = parse_variants(&value)?;
            }
            "--help" | "-h" => {
                println!(
                    "personalrag-v2-bench [--root PATH | --generate-synthetic-mib N] [--out-dir PATH] [--repeats N] [--suite source|synthetic|q45] [--variants A,B,C,D]"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }
    Ok(Args {
        root,
        out_dir,
        repeats,
        generate_synthetic_mib,
        suite,
        variants,
    })
}

fn parse_variants(value: &str) -> Result<Vec<PrototypeVariant>, Box<dyn std::error::Error>> {
    let mut variants = Vec::new();
    for item in value.split(',') {
        let variant = match item.trim().to_ascii_uppercase().as_str() {
            "A" => PrototypeVariant::A,
            "B" => PrototypeVariant::B,
            "C" => PrototypeVariant::C,
            "D" => PrototypeVariant::D,
            other => return Err(format!("unknown variant: {other}").into()),
        };
        if !variants.contains(&variant) {
            variants.push(variant);
        }
    }
    if variants.is_empty() {
        return Err("at least one variant is required".into());
    }
    Ok(variants)
}

fn ms(value: std::time::Duration) -> f64 {
    value.as_secs_f64() * 1000.0
}
