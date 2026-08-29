use personalrag_v2::{
    Corpus, PatternError, PrototypeIndex, PrototypeVariant, RegexPattern, WildcardPattern,
    naive_search,
};

#[test]
fn unicode_nfc_full_fold_and_original_offsets_are_frozen() {
    let corpus = Corpus::from_documents([(
        "unicode.txt",
        "xxStraße yy é zz e\u{301} Σςσ Ａ A\n".as_bytes().to_vec(),
    )]);
    let index = PrototypeIndex::build(&corpus);

    let strasse = index.search_all(&corpus, "STRASSE", false, PrototypeVariant::D);
    assert_eq!(strasse.hits.len(), 1);
    assert_eq!(strasse.hits[0].byte_offset_in_line, 2);

    let sharp_s = index.search_all(&corpus, "s", false, PrototypeVariant::D);
    let offsets = sharp_s
        .hits
        .iter()
        .map(|hit| hit.byte_offset_in_line)
        .collect::<Vec<_>>();
    assert_eq!(offsets.iter().filter(|&&offset| offset == 6).count(), 1);

    let precomposed = index.search_all(&corpus, "é", true, PrototypeVariant::D);
    let decomposed = index.search_all(&corpus, "e\u{301}", true, PrototypeVariant::D);
    assert_eq!(precomposed.hits, decomposed.hits);
    assert_eq!(precomposed.hits.len(), 2);

    assert_eq!(
        index
            .search_all(&corpus, "σ", false, PrototypeVariant::D)
            .hits
            .len(),
        3
    );
    let compat = Corpus::from_documents([("compat.txt", "Ａ A\n".as_bytes().to_vec())]);
    let compat_index = PrototypeIndex::build(&compat);
    assert_eq!(
        compat_index
            .search_all(&compat, "a", false, PrototypeVariant::D)
            .hits
            .len(),
        1
    );
    assert_eq!(
        compat_index
            .search_all(&compat, "Ａ", false, PrototypeVariant::D)
            .hits
            .len(),
        1
    );
}

#[test]
fn unicode_literal_index_matches_naive_oracle() {
    let corpus = Corpus::from_documents([
        (
            "a.txt",
            "Cafe\u{301} Straße 日本語 Σς\n".as_bytes().to_vec(),
        ),
        ("b.txt", "CAFÉ STRASSE 日本語 σ\n".as_bytes().to_vec()),
    ]);
    let index = PrototypeIndex::build(&corpus);
    for (query, case_sensitive) in [
        ("café", false),
        ("STRASSE", false),
        ("σ", false),
        ("日本語", true),
        ("CAFÉ", true),
    ] {
        assert_eq!(
            index
                .search_all(&corpus, query, case_sensitive, PrototypeVariant::D)
                .hits,
            naive_search(&corpus, query, case_sensitive),
            "query={query:?} case_sensitive={case_sensitive}"
        );
    }
}

#[test]
fn regex_and_wildcard_use_same_unicode_semantics_and_hard_line_boundaries() {
    let corpus = Corpus::from_documents([(
        "patterns.txt",
        "xxCreateDirectoryW yy\nERROR_1234\nStraße\né\nNEXT\n"
            .as_bytes()
            .to_vec(),
    )]);
    let index = PrototypeIndex::build(&corpus);

    assert_eq!(
        index
            .search_regex_all(
                &corpus,
                r"Create(File|Directory)W",
                false,
                PrototypeVariant::D
            )
            .unwrap()
            .hits
            .len(),
        1
    );
    assert_eq!(
        index
            .search_regex_all(&corpus, r"ERROR_[0-9]{4}", true, PrototypeVariant::D)
            .unwrap()
            .hits
            .len(),
        1
    );
    assert_eq!(
        index
            .search_regex_all(&corpus, r"e\u{301}", true, PrototypeVariant::D)
            .unwrap()
            .hits
            .len(),
        1
    );
    assert_eq!(
        index
            .search_wildcard_all(&corpus, "STR*E", false, PrototypeVariant::D)
            .unwrap()
            .hits
            .len(),
        1
    );
    assert!(
        index
            .search_regex_all(&corpus, "é.*NEXT", true, PrototypeVariant::D)
            .unwrap()
            .hits
            .is_empty()
    );
}

#[test]
fn empty_match_end_anchor_reports_original_utf8_end_offset() {
    let corpus = Corpus::from_documents([("end.txt", "e\u{301}".as_bytes().to_vec())]);
    let index = PrototypeIndex::build(&corpus);
    let outcome = index
        .search_regex_all(&corpus, "$", true, PrototypeVariant::D)
        .unwrap();
    assert_eq!(outcome.hits.len(), 1);
    assert_eq!(outcome.hits[0].byte_offset_in_line, "e\u{301}".len() as u32);
}

#[test]
fn pattern_safety_rejections_are_explicit() {
    for pattern in [r"(a)\1", r"(?=a)", r"\bword\b", r"a+?"] {
        assert!(RegexPattern::compile(pattern, false).is_err(), "{pattern}");
    }
    assert!(WildcardPattern::compile("dangling\\", false).is_err());
    let class_expansion = RegexPattern::compile("[ß]", false);
    assert!(matches!(class_expansion, Err(PatternError(_))));
}

#[test]
fn mandatory_literal_reduces_regex_and_wildcard_candidates() {
    let common = "common abc|bcd|cde filler filler filler filler\n";
    let mut docs = Vec::new();
    for file_id in 0..3 {
        let mut body = common.repeat(12_000);
        if file_id == 1 {
            body.push_str("UNIQUE_V2_SENTINEL_A1F4\n");
        }
        docs.push((format!("{file_id}.txt"), body.into_bytes()));
    }
    let corpus = Corpus::from_documents(docs);
    assert!(corpus.block_count() >= 2);
    let index = PrototypeIndex::build(&corpus);

    let regex = index
        .search_regex_all(
            &corpus,
            r"UNIQUE_V2_SENTINEL_[0-9A-F]{4}",
            false,
            PrototypeVariant::D,
        )
        .unwrap();
    let wildcard = index
        .search_wildcard_all(&corpus, "UNIQUE_V2_SENTINEL_*", false, PrototypeVariant::D)
        .unwrap();
    assert_eq!(regex.hits.len(), 1);
    assert_eq!(wildcard.hits.len(), 1);
    assert!(regex.metrics.candidate_blocks < corpus.block_count());
    assert!(wildcard.metrics.candidate_blocks < corpus.block_count());
}
