use personalrag_portable_search::{
    AccelerationProfile, BuildMode, BuildOptions, DocumentInput, PersistentIndex,
    VNextContentPlanMode, VNextDocumentInput, VNextSegmentReader, build_index_unified, fold_ascii,
    write_vnext_segment_with_block_size,
};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn temp_root(label: &str) -> PathBuf {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "personalrag-vnext-query-{label}-{}-{id}",
        std::process::id()
    ))
}

fn normalized_doc(logical_id: u64, path: &str, content: &str) -> VNextDocumentInput {
    VNextDocumentInput::new(logical_id, path, fold_ascii(content.as_bytes()))
}

fn naive_content(docs: &[VNextDocumentInput], query: &[u8]) -> Vec<u16> {
    let query = fold_ascii(query);
    if query.is_empty() {
        return Vec::new();
    }
    docs.iter()
        .enumerate()
        .filter_map(|(index, doc)| {
            doc.normalized_content
                .windows(query.len())
                .any(|window| window == query)
                .then_some(index as u16)
        })
        .collect()
}

fn naive_name(docs: &[VNextDocumentInput], query: &[u8]) -> Vec<u16> {
    let query = fold_ascii(query);
    if query.is_empty() {
        return Vec::new();
    }
    docs.iter()
        .enumerate()
        .filter_map(|(index, doc)| {
            let name = fold_ascii(doc.display_path.as_bytes());
            name.windows(query.len())
                .any(|window| window == query)
                .then_some(index as u16)
        })
        .collect()
}

#[test]
fn vnext_gate4_q1_q2_and_path_follow_perf12_ascii_fold_semantics() {
    let root = temp_root("short-path");
    fs::create_dir_all(&root).unwrap();
    let segment = root.join("segment.prseg2");
    let docs = vec![
        normalized_doc(1, "Src/Alpha.CPP", "AbCdEf"),
        normalized_doc(2, "src/日本語.TXT", "日本語検索"),
        normalized_doc(3, "docs/readme.md", "zzabyy"),
        normalized_doc(4, "empty.bin", ""),
    ];
    write_vnext_segment_with_block_size(&segment, &docs, 4).unwrap();
    let reader = VNextSegmentReader::open(&segment).unwrap();

    for query in [b"A".as_slice(), b"AB", b"dE", "日本".as_bytes(), b"qq"] {
        assert_eq!(
            reader.search_content(query).unwrap(),
            naive_content(&docs, query)
        );
    }
    for query in [
        b"SRC".as_slice(),
        b"alpha.cpp",
        "日本語".as_bytes(),
        b"README",
        b"missing",
    ] {
        assert_eq!(reader.search_path(query).unwrap(), naive_name(&docs, query));
        assert_eq!(reader.search_name(query).unwrap(), naive_name(&docs, query));
    }

    let (_, q1) = reader.search_content_with_diagnostics(b"A").unwrap();
    assert_eq!(q1.mode, VNextContentPlanMode::ShortIndex);
    let (_, q2) = reader.search_content_with_diagnostics(b"AB").unwrap();
    assert_eq!(q2.mode, VNextContentPlanMode::ShortIndex);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_gate4_rarest_anchor_and_exact_block_verification_are_exact() {
    let root = temp_root("anchor");
    fs::create_dir_all(&root).unwrap();
    let segment = root.join("segment.prseg2");
    let docs = vec![
        normalized_doc(1, "a.txt", "abcabcabcabc"),
        normalized_doc(2, "target.txt", "----abcxyz789----"),
        normalized_doc(3, "c.txt", "abcabcxyz000"),
        normalized_doc(4, "d.txt", "abcabcabcabc"),
    ];
    write_vnext_segment_with_block_size(&segment, &docs, 4).unwrap();
    let reader = VNextSegmentReader::open(&segment).unwrap();

    let query = b"ABCXYZ789";
    let (hits, diag) = reader.search_content_with_diagnostics(query).unwrap();
    assert_eq!(hits, naive_content(&docs, query));
    assert_eq!(hits, vec![1]);
    assert_eq!(diag.mode, VNextContentPlanMode::Q3Anchor);
    assert!(
        diag.anchor_offset > 0,
        "expected a later rare trigram anchor: {diag:?}"
    );
    assert!(diag.anchor_blocks > 0);
    assert!(diag.verified_blocks > 0);

    for query in [
        b"abc".as_slice(),
        b"bcxyz",
        b"xyz789",
        b"abcabcxyz000",
        b"not-present",
    ] {
        assert_eq!(
            reader.search_content(query).unwrap(),
            naive_content(&docs, query)
        );
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_gate4_long_query_crosses_multiple_block_boundaries_without_false_negative() {
    let root = temp_root("multi-boundary");
    fs::create_dir_all(&root).unwrap();
    let segment = root.join("segment.prseg2");
    let docs = vec![normalized_doc(
        1,
        "boundary.txt",
        "0123456789abcdefghijklmnop",
    )];
    write_vnext_segment_with_block_size(&segment, &docs, 4).unwrap();
    let reader = VNextSegmentReader::open(&segment).unwrap();

    for query in [
        b"3456789abc".as_slice(),
        b"789abcdefgh",
        b"defghijklmnop",
        b"23456789abcdefghijkl",
    ] {
        assert_eq!(
            reader.search_content(query).unwrap(),
            vec![0],
            "query={:?}",
            String::from_utf8_lossy(query)
        );
    }
    assert!(reader.search_content(b"3456789abX").unwrap().is_empty());
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_gate4_absent_trigram_is_exact_zero_hit() {
    let root = temp_root("zero-hit");
    fs::create_dir_all(&root).unwrap();
    let segment = root.join("segment.prseg2");
    let docs = vec![normalized_doc(1, "a.txt", "abcdef")];
    write_vnext_segment_with_block_size(&segment, &docs, 4).unwrap();
    let reader = VNextSegmentReader::open(&segment).unwrap();

    let (hits, diag) = reader
        .search_content_with_diagnostics(b"abcZZZdef")
        .unwrap();
    assert!(hits.is_empty());
    assert_eq!(diag.mode, VNextContentPlanMode::ZeroHit);
    assert_eq!(diag.anchor_blocks, 0);
    assert_eq!(diag.verified_blocks, 0);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_gate4_matches_perf12_and_naive_oracle_for_content_and_names() {
    let root = temp_root("perf12-oracle");
    let perf = root.join("perf12");
    fs::create_dir_all(&root).unwrap();
    let vnext_path = root.join("segment.prseg2");

    let source = [
        ("src/Alpha.cpp", "Return ERROR timeout abcdefghijk"),
        ("src/Beta.rs", "abcXYZ789 and common timeout"),
        ("docs/日本語.txt", "日本語検索とPersonalRag"),
        (
            "deep/module_needle.py",
            "prefix 0123456789abcdefghijkl suffix",
        ),
        ("empty.dat", ""),
        ("misc/readme.MD", "AB q2 marker"),
    ];
    let perf_docs = source
        .iter()
        .map(|(path, content)| {
            DocumentInput::new(
                *path,
                *path,
                fold_ascii(path.as_bytes()),
                fold_ascii(content.as_bytes()),
            )
        })
        .collect::<Vec<_>>();
    let vnext_docs = source
        .iter()
        .enumerate()
        .map(|(index, (path, content))| normalized_doc(index as u64, path, content))
        .collect::<Vec<_>>();

    let options = BuildOptions {
        mode: BuildMode::Adaptive,
        segment_docs: 64,
        workers: 1,
    };
    build_index_unified(&perf_docs, &perf, &options, AccelerationProfile::Full).unwrap();
    let perf_reader = PersistentIndex::open(&perf, true).unwrap();
    write_vnext_segment_with_block_size(&vnext_path, &vnext_docs, 4).unwrap();
    let vnext = VNextSegmentReader::open(&vnext_path).unwrap();

    let content_queries: &[&[u8]] = &[
        b"A",
        b"AB",
        b"ERROR",
        b"timeout",
        b"abcXYZ789",
        b"0123456789abc",
        "日本語検索".as_bytes(),
        b"PersonalRag",
        b"definitely-missing",
    ];
    for query in content_queries {
        let expected = naive_content(&vnext_docs, query)
            .into_iter()
            .map(u32::from)
            .collect::<Vec<_>>();
        assert_eq!(
            perf_reader.search_content(query).unwrap(),
            expected,
            "Perf12 content query={query:?}"
        );
        assert_eq!(
            vnext
                .search_content(query)
                .unwrap()
                .into_iter()
                .map(u32::from)
                .collect::<Vec<_>>(),
            expected,
            "vNext content query={query:?}"
        );
    }

    let name_queries: &[&[u8]] = &[
        b"SRC",
        b"alpha.CPP",
        b"module_needle",
        "日本語".as_bytes(),
        b".md",
        b"missing",
    ];
    for query in name_queries {
        let expected = naive_name(&vnext_docs, query)
            .into_iter()
            .map(u32::from)
            .collect::<Vec<_>>();
        assert_eq!(
            perf_reader.search_name(query).unwrap(),
            expected,
            "Perf12 name query={query:?}"
        );
        assert_eq!(
            vnext
                .search_path(query)
                .unwrap()
                .into_iter()
                .map(u32::from)
                .collect::<Vec<_>>(),
            expected,
            "vNext path query={query:?}"
        );
    }

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_gate4_randomized_substrings_match_naive_oracle() {
    let root = temp_root("random-oracle");
    fs::create_dir_all(&root).unwrap();
    let segment = root.join("segment.prseg2");
    let docs = (0..48usize)
        .map(|index| {
            let text = format!(
                "doc-{index:02}-abc{}-TIMEOUT-{}-日本語-{:08x}-tail",
                "xyz".repeat(index % 7 + 1),
                "0123456789".repeat(index % 5 + 1),
                index.wrapping_mul(2_654_435_761usize)
            );
            normalized_doc(index as u64, &format!("Src/Module_{index:02}.CPP"), &text)
        })
        .collect::<Vec<_>>();
    write_vnext_segment_with_block_size(&segment, &docs, 7).unwrap();
    let reader = VNextSegmentReader::open(&segment).unwrap();

    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    for round in 0..600usize {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let doc_index = (state as usize) % docs.len();
        let content = &docs[doc_index].normalized_content;
        let max_len = content.len().min(24);
        let len = 1 + ((state >> 17) as usize % max_len.max(1));
        let start = ((state >> 33) as usize) % (content.len() - len + 1);
        let mut query = content[start..start + len].to_vec();
        if round % 3 == 0 {
            query.make_ascii_uppercase();
        }
        assert_eq!(
            reader.search_content(&query).unwrap(),
            naive_content(&docs, &query),
            "round={round} doc={doc_index} start={start} len={len} query={:?}",
            String::from_utf8_lossy(&query)
        );
    }

    for query in [
        b"no-such-substring-1".as_slice(),
        b"ZZZ-no-such-substring-2",
        "存在しない検索語".as_bytes(),
    ] {
        assert_eq!(
            reader.search_content(query).unwrap(),
            naive_content(&docs, query)
        );
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_common_q3_uses_second_and_third_anchors_without_boundary_false_negatives() {
    let root = temp_root("multi-anchor-boundary");
    fs::create_dir_all(&root).unwrap();
    let segment = root.join("segment.prseg2");
    let mut docs = Vec::new();
    let mut id = 0u64;
    let mut push = |content: &str, count: usize| {
        for _ in 0..count {
            docs.push(normalized_doc(id, &format!("doc_{id:04}.txt"), content));
            id += 1;
        }
    };

    // With block_size=8 the exact hit starts at byte 7: `abc` belongs to block 0 while
    // `bcd`/`cde` belong to block 1. The false-positive groups make all three postings common,
    // but their intersection narrows 110 primary blocks to the 10 exact-hit blocks.
    push("xxxxxxxabcdezzz", 10);
    push("xxxxxxxabc___bcd", 40);
    push("xxxxxxxabc___cde", 40);
    push("xxxxxxxabc___qqq", 20);
    push("xxxxxxxbcd___qqq", 100);
    push("xxxxxxxcde___qqq", 120);

    write_vnext_segment_with_block_size(&segment, &docs, 8).unwrap();
    let reader = VNextSegmentReader::open(&segment).unwrap();
    let (hits, diag) = reader.search_content_with_diagnostics(b"abcde").unwrap();
    assert_eq!(hits, naive_content(&docs, b"abcde"));
    assert_eq!(hits.len(), 10);
    assert_eq!(diag.mode, VNextContentPlanMode::Q3Anchor);
    assert_eq!(diag.selected_anchor_count, 3, "{diag:?}");
    assert_eq!(diag.anchor_blocks, 110, "{diag:?}");
    assert_eq!(diag.candidate_blocks, 10, "{diag:?}");
    assert_eq!(diag.verified_blocks, 10, "{diag:?}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_common_q3_skips_extra_anchors_when_they_do_not_reduce_candidates() {
    let root = temp_root("multi-anchor-no-benefit");
    fs::create_dir_all(&root).unwrap();
    let segment = root.join("segment.prseg2");
    let docs = (0..128u64)
        .map(|id| normalized_doc(id, &format!("doc_{id:03}.txt"), "prefix timeout suffix"))
        .collect::<Vec<_>>();
    write_vnext_segment_with_block_size(&segment, &docs, 8192).unwrap();
    let reader = VNextSegmentReader::open(&segment).unwrap();

    let (hits, diag) = reader.search_content_with_diagnostics(b"timeout").unwrap();
    assert_eq!(hits, naive_content(&docs, b"timeout"));
    assert_eq!(hits.len(), 128);
    assert_eq!(diag.selected_anchor_count, 1, "{diag:?}");
    assert_eq!(diag.candidate_blocks, diag.anchor_blocks, "{diag:?}");

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn vnext_high_hit_single_block_parallel_path_matches_naive_oracle() {
    let root = temp_root("high-hit-parallel");
    fs::create_dir_all(&root).unwrap();
    let segment = root.join("segment.prseg2");
    let docs = (0..8_200u64)
        .map(|id| normalized_doc(id, &format!("doc_{id:05}.txt"), "prefix timeout suffix"))
        .collect::<Vec<_>>();
    write_vnext_segment_with_block_size(&segment, &docs, 8192).unwrap();
    let reader = VNextSegmentReader::open(&segment).unwrap();

    let (hits, diag) = reader.search_content_with_diagnostics(b"timeout").unwrap();
    assert_eq!(hits, naive_content(&docs, b"timeout"));
    assert_eq!(hits.len(), 8_200);
    assert_eq!(diag.selected_anchor_count, 1, "{diag:?}");
    assert!(diag.candidate_blocks >= 8_192, "{diag:?}");
    assert_eq!(diag.verified_blocks, diag.candidate_blocks, "{diag:?}");

    fs::remove_dir_all(root).unwrap();
}
