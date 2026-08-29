use personalrag_v2::{
    Corpus, GlobalPresenceEncoding, PrototypeIndex, PrototypeVariant, RARE_TRIGRAM_MAX_DF,
    SPARSE_BUDGET_DENOMINATOR, SPARSE_BUDGET_NUMERATOR, naive_search,
};

fn assert_matches_oracle(
    corpus: &Corpus,
    index: &PrototypeIndex,
    query: &str,
    case_sensitive: bool,
) {
    let oracle = naive_search(corpus, query, case_sensitive);
    for variant in [
        PrototypeVariant::A,
        PrototypeVariant::B,
        PrototypeVariant::C,
        PrototypeVariant::D,
    ] {
        let actual = index
            .search_all(corpus, query, case_sensitive, variant)
            .hits;
        assert_eq!(actual, oracle, "variant={variant:?} query={query:?}");
    }
}

#[test]
fn literal_search_matches_oracle_for_short_long_case_japanese_and_overlaps() {
    let corpus = Corpus::from_documents([
        (
            "a.txt",
            b"CreateFileW alpha beta\nABC\nDEF\n\xe6\x97\xa5\xe6\x9c\xac\xe8\xaa\x9e\xe3\x83\x86\xe3\x82\xad\xe3\x82\xb9\xe3\x83\x88\naaaaa\n".to_vec(),
        ),
        (
            "b.txt",
            b"createfilew alphabet soup\nrare-sentinel-123\n".to_vec(),
        ),
    ]);
    let index = PrototypeIndex::build(&corpus);
    for query in [
        "a",
        "ab",
        "abc",
        "File",
        "rare-sentinel-123",
        "日本語テキスト",
        "never-present",
        "aaa",
    ] {
        assert_matches_oracle(&corpus, &index, query, false);
    }
    assert_matches_oracle(&corpus, &index, "CreateFileW", true);
    assert_matches_oracle(&corpus, &index, "createfilew", true);
}

#[test]
fn logical_line_boundaries_are_hard_boundaries() {
    let corpus = Corpus::from_documents([("boundary.txt", b"ABC\nDEF\n".to_vec())]);
    let index = PrototypeIndex::build(&corpus);
    assert_matches_oracle(&corpus, &index, "CDE", false);
    assert!(naive_search(&corpus, "CDE", false).is_empty());
}

#[test]
fn sparse_anchor_budget_is_never_exceeded_and_fallback_remains_exact() {
    let mut docs = Vec::new();
    for i in 0..80 {
        let body = format!("common abc|bcd|cde| block-{i:03} unique-{i:03}-XYZ\n");
        docs.push((format!("{i}.txt"), body.into_bytes()));
    }
    let corpus = Corpus::from_documents(docs);
    let index = PrototypeIndex::build(&corpus);
    for variant in [
        PrototypeVariant::B,
        PrototypeVariant::C,
        PrototypeVariant::D,
    ] {
        let report = index.capacity_report(&corpus, variant);
        let sparse_bytes = report.sparse_anchor_metadata_bytes
            + report.sparse_anchor_posting_bytes
            + report.higher_sparse_anchor_metadata_bytes
            + report.higher_sparse_anchor_posting_bytes;
        let budget = ((corpus.selected_source_bytes() as u128 * SPARSE_BUDGET_NUMERATOR as u128)
            / SPARSE_BUDGET_DENOMINATOR as u128) as usize;
        assert!(sparse_bytes <= budget);
    }
    assert_matches_oracle(&corpus, &index, "abcde", false);
    assert_matches_oracle(&corpus, &index, "unique-079-XYZ", false);
}

#[test]
fn rare_anchor_reduces_candidate_blocks_without_changing_results() {
    let mut docs = Vec::new();
    for i in 0..(RARE_TRIGRAM_MAX_DF as usize + 20) {
        let mut body = String::from("common abc bcd cde payload payload payload\n");
        if i == 7 {
            body.push_str("UNIQUE_ANCHOR_9F3A\n");
        }
        docs.push((format!("{i}.txt"), body.into_bytes()));
    }
    let corpus = Corpus::from_documents(docs);
    let index = PrototypeIndex::build(&corpus);
    let a = index.search_all(&corpus, "UNIQUE_ANCHOR_9F3A", false, PrototypeVariant::A);
    for variant in [
        PrototypeVariant::B,
        PrototypeVariant::C,
        PrototypeVariant::D,
    ] {
        let accelerated = index.search_all(&corpus, "UNIQUE_ANCHOR_9F3A", false, variant);
        assert_eq!(a.hits, accelerated.hits);
        assert!(accelerated.metrics.candidate_blocks <= a.metrics.candidate_blocks);
    }
}

#[test]
fn dense_and_adaptive_global_presence_shortcut_same_zero_hits() {
    let corpus = Corpus::from_documents([("a.txt", b"alpha beta gamma\n".to_vec())]);
    let index = PrototypeIndex::build(&corpus);
    for variant in [
        PrototypeVariant::B,
        PrototypeVariant::C,
        PrototypeVariant::D,
    ] {
        let outcome = index.search_all(&corpus, "QZXJ-never", false, variant);
        assert!(outcome.hits.is_empty());
        assert!(outcome.metrics.global_absent_shortcut);
        assert_eq!(outcome.metrics.candidate_blocks, 0);
    }
}

#[test]
fn adaptive_presence_reduces_small_corpus_fixed_overhead() {
    let corpus = Corpus::from_documents([
        ("a.txt", b"alpha beta gamma delta\n".repeat(1000)),
        (
            "b.txt",
            "CreateFileW 日本語 rare-sentinel\n".as_bytes().repeat(1000),
        ),
    ]);
    let index = PrototypeIndex::build(&corpus);
    let b = index.capacity_report(&corpus, PrototypeVariant::B);
    let c = index.capacity_report(&corpus, PrototypeVariant::C);
    assert!(c.global_trigram_presence_bytes < b.global_trigram_presence_bytes);
    assert!(c.total_persistent_bytes < b.total_persistent_bytes);
    assert_ne!(c.global_trigram_encoding, GlobalPresenceEncoding::Dense);
}

#[test]
fn deterministic_pseudorandom_substrings_match_naive_oracle() {
    let mut state = 0x1234_5678_9abc_def0_u64;
    let mut docs = Vec::new();
    for file_id in 0..12 {
        let mut bytes = Vec::new();
        for _ in 0..1200 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let byte = b'a' + ((state >> 32) % 8) as u8;
            bytes.push(byte);
            if bytes.len() % 80 == 79 {
                bytes.push(b'\n');
            }
        }
        docs.push((format!("random-{file_id}.txt"), bytes));
    }
    let corpus = Corpus::from_documents(docs);
    let index = PrototypeIndex::build(&corpus);
    for len in 1_usize..=12 {
        for sample in 0..20 {
            let file = (sample * 7 + len) % 12;
            let query = format!(
                "{}{}{}",
                (b'a' + (file % 8) as u8) as char,
                (b'a' + (sample % 8) as u8) as char,
                "a".repeat(len.saturating_sub(2))
            );
            assert_matches_oracle(&corpus, &index, &query, false);
        }
    }
}

#[test]
fn serialized_capacity_matches_actual_file_size() {
    let corpus = Corpus::from_documents([
        ("a.txt", b"alpha beta gamma\n".repeat(5000)),
        ("b.txt", b"delta epsilon zeta\n".repeat(5000)),
    ]);
    let index = PrototypeIndex::build(&corpus);
    let temp = std::env::temp_dir().join(format!(
        "personalrag-personalrag-v2-{}.bin",
        std::process::id()
    ));
    for variant in [
        PrototypeVariant::A,
        PrototypeVariant::B,
        PrototypeVariant::C,
        PrototypeVariant::D,
    ] {
        let report = index
            .write_prototype_index(&corpus, variant, &temp)
            .unwrap();
        let actual = std::fs::metadata(&temp).unwrap().len() as usize;
        assert_eq!(actual, report.total_persistent_bytes);
    }
    let _ = std::fs::remove_file(temp);
}

fn make_higher_ngram_corpus() -> Corpus {
    let common = "abc|bcd|cde wxy|xyz klmn|lmno filler filler filler\n";
    let mut docs = Vec::new();
    for file_id in 0..4 {
        let mut body = common.repeat(14_000);
        if file_id == 2 {
            body.push_str("rare-q4=wxyz rare-q5=klmno\n");
        }
        docs.push((format!("higher-{file_id}.txt"), body.into_bytes()));
    }
    Corpus::from_documents(docs)
}

#[test]
fn adaptive_q4_q5_filters_preserve_oracle_and_reduce_candidates() {
    let corpus = make_higher_ngram_corpus();
    assert!(corpus.block_count() >= 2);
    let index = PrototypeIndex::build(&corpus);

    for query in ["wxyz", "klmno"] {
        let oracle = naive_search(&corpus, query, false);
        let c = index.search_all(&corpus, query, false, PrototypeVariant::C);
        let d = index.search_all(&corpus, query, false, PrototypeVariant::D);
        assert_eq!(c.hits, oracle);
        assert_eq!(d.hits, oracle);
        assert!(
            d.metrics.candidate_blocks < c.metrics.candidate_blocks,
            "query={query}"
        );
        assert!(matches!(d.metrics.selected_anchor_width, Some(4 | 5)));
    }
}

#[test]
fn adaptive_q4_q5_global_filter_shortcuts_adversarial_zero_hit() {
    let corpus = make_higher_ngram_corpus();
    let index = PrototypeIndex::build(&corpus);
    let c = index.search_all(&corpus, "abcde", false, PrototypeVariant::C);
    let d = index.search_all(&corpus, "abcde", false, PrototypeVariant::D);
    assert!(c.hits.is_empty());
    assert!(d.hits.is_empty());
    assert!(c.metrics.candidate_blocks > 0);
    assert_eq!(d.metrics.candidate_blocks, 0);
    assert!(d.metrics.global_absent_shortcut);
}

#[test]
fn variant_d_capacity_respects_hard_and_sparse_budgets() {
    let corpus = make_higher_ngram_corpus();
    let index = PrototypeIndex::build(&corpus);
    let report = index.capacity_report(&corpus, PrototypeVariant::D);
    assert!(report.index_source_ratio() <= 0.10);
    let sparse = report.sparse_anchor_metadata_bytes
        + report.sparse_anchor_posting_bytes
        + report.higher_sparse_anchor_metadata_bytes
        + report.higher_sparse_anchor_posting_bytes;
    let budget = ((corpus.selected_source_bytes() as u128 * SPARSE_BUDGET_NUMERATOR as u128)
        / SPARSE_BUDGET_DENOMINATOR as u128) as usize;
    assert!(sparse <= budget);
    assert!(report.higher_ngram_filter_bytes > 0);
}

#[test]
fn variant_d_actual_q4_q5_substrings_never_false_negative() {
    let corpus = Corpus::from_documents([
        (
            "a.txt",
            b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ\n".repeat(8000),
        ),
        (
            "b.txt",
            b"the-quick-brown-fox-jumps-over-the-lazy-dog-9876543210\n".repeat(8000),
        ),
    ]);
    let index = PrototypeIndex::build(&corpus);
    for query in [
        "0123", "45678", "abcd", "klmno", "wxyz", "ABCD", "QRSTU", "quick", "brown", "lazy-",
    ] {
        let oracle = naive_search(&corpus, query, false);
        assert!(!oracle.is_empty(), "fixture query must exist: {query}");
        let actual = index
            .search_all(&corpus, query, false, PrototypeVariant::D)
            .hits;
        assert_eq!(actual, oracle, "query={query}");
    }
}
