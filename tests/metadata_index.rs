use personalrag_v2::{
    MetadataError, MetadataFileKind, MetadataIndex, MetadataRecord, MetadataSearchRequest,
    normalize_str,
};
use std::path::PathBuf;

fn records() -> Vec<MetadataRecord> {
    vec![
        MetadataRecord {
            file_id: 10,
            path: PathBuf::from("/Root/Docs/Straße Report.TXT"),
            source_root: 1,
            size: 100,
            modified_ns: 1,
            kind: MetadataFileKind::File,
            content_searchable: true,
            extractable: false,
        },
        MetadataRecord::file(20, "/Root/Cafe/cafe\u{301}.md", 200, 2),
        MetadataRecord::file(30, "/Root/日本語/設計資料.txt", 300, 3),
        MetadataRecord::file(40, "/Other/root/report.txt", 400, 4),
        MetadataRecord::file(50, "/Root/Docs/Other.bin", 500, 5),
    ]
}

fn naive_contains(haystack: &str, needle: &str, case_sensitive: bool) -> bool {
    let h = normalize_str(haystack, case_sensitive);
    let n = normalize_str(needle, case_sensitive);
    if n.bytes().is_empty() {
        return true;
    }
    h.bytes()
        .windows(n.bytes().len())
        .any(|window| window == n.bytes())
}

fn naive_ids(
    records: &[MetadataRecord],
    filename: Option<&str>,
    full_path: Option<&str>,
    case_sensitive: bool,
) -> Vec<u64> {
    let path_query = full_path.map(|value| value.replace('\\', "/"));
    records
        .iter()
        .filter_map(|record| {
            let path = record.path.to_str()?.replace('\\', "/");
            let name = record.path.file_name()?.to_str()?;
            if filename.is_some_and(|query| !naive_contains(name, query, case_sensitive)) {
                return None;
            }
            if path_query
                .as_deref()
                .is_some_and(|query| !naive_contains(&path, query, case_sensitive))
            {
                return None;
            }
            Some(record.file_id)
        })
        .collect()
}

fn actual_ids(index: &MetadataIndex, request: MetadataSearchRequest<'_>) -> Vec<u64> {
    index
        .search(request)
        .hits
        .into_iter()
        .map(|hit| hit.file_id)
        .collect()
}

#[test]
fn filename_and_path_queries_match_independent_unicode_oracle() {
    let records = records();
    let index = MetadataIndex::build(records.clone()).unwrap();
    for (filename, path, case_sensitive) in [
        (Some("STRASSE"), None, false),
        (Some("Straße"), None, true),
        (Some("strasse"), None, true),
        (Some("CAFÉ"), None, false),
        (Some("設計"), None, false),
        (None, Some(r"root\docs"), false),
        (Some("report"), Some("other"), false),
        (Some("txt"), Some("root"), false),
    ] {
        let expected = naive_ids(&records, filename, path, case_sensitive);
        let actual = actual_ids(
            &index,
            MetadataSearchRequest {
                filename,
                full_path: path,
                case_sensitive,
                max_results: 100,
            },
        );
        assert_eq!(actual, expected, "filename={filename:?} path={path:?}");
    }
}

#[test]
fn q4_q5_filter_reduces_common_q3_candidates_without_changing_result() {
    let mut records = Vec::new();
    for index in 0..5000_u64 {
        let filename = if index == 4242 {
            format!("abcd_target_{index:05}.txt")
        } else {
            format!("abc_x_bcd_x_cde_{index:05}.txt")
        };
        records.push(MetadataRecord::file(
            index,
            PathBuf::from(format!("/root/common/{filename}")),
            index,
            0,
        ));
    }
    let index = MetadataIndex::build(records).unwrap();
    let rare = index.search(MetadataSearchRequest {
        filename: Some("abcd"),
        max_results: 100,
        ..MetadataSearchRequest::default()
    });
    assert_eq!(
        rare.hits.iter().map(|hit| hit.file_id).collect::<Vec<_>>(),
        vec![4242]
    );
    assert!(rare.metrics.candidate_records <= 5000);

    let absent = index.search(MetadataSearchRequest {
        filename: Some("abcde"),
        max_results: 100,
        ..MetadataSearchRequest::default()
    });
    assert!(absent.hits.is_empty());
    assert!(absent.metrics.candidate_records <= 5000);

    let exact_q3_absent = index.search(MetadataSearchRequest {
        filename: Some("qzx"),
        max_results: 100,
        ..MetadataSearchRequest::default()
    });
    assert!(exact_q3_absent.hits.is_empty());
    assert_eq!(exact_q3_absent.metrics.candidate_records, 0);
    assert!(exact_q3_absent.metrics.global_absent_shortcut);
}

#[test]
fn snapshot_round_trip_preserves_results_and_rejects_corruption_and_overwrite() {
    let records = records();
    let index = MetadataIndex::build(records.clone()).unwrap();
    let dir = std::env::temp_dir().join(format!("personalrag-metadata-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let snapshot = dir.join("catalog.prv2meta");
    let written = index.write_snapshot(&snapshot).unwrap();
    assert_eq!(
        written,
        std::fs::metadata(&snapshot).unwrap().len() as usize
    );
    assert!(matches!(
        index.write_snapshot(&snapshot),
        Err(MetadataError::SnapshotExists(_))
    ));

    let loaded = MetadataIndex::load_snapshot(&snapshot).unwrap();
    for (filename, path) in [
        (Some("strasse"), None),
        (None, Some("日本語")),
        (Some("report"), Some("other")),
    ] {
        let request = MetadataSearchRequest {
            filename,
            full_path: path,
            case_sensitive: false,
            max_results: 100,
        };
        assert_eq!(
            actual_ids(&loaded, request.clone()),
            naive_ids(&records, filename, path, false)
        );
    }

    let mut bytes = std::fs::read(&snapshot).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x55;
    let corrupt = dir.join("corrupt.prv2meta");
    std::fs::write(&corrupt, bytes).unwrap();
    assert!(matches!(
        MetadataIndex::load_snapshot(&corrupt),
        Err(MetadataError::ChecksumMismatch)
    ));
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn unix_non_utf8_path_identity_round_trips_even_when_not_searchable() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    let path = PathBuf::from(OsString::from_vec(b"root/invalid-\xff.bin".to_vec()));
    let record = MetadataRecord::file(77, path.clone(), 1, 2);
    let index = MetadataIndex::build(vec![record]).unwrap();
    let dir = std::env::temp_dir().join(format!(
        "personalrag-metadata-nonutf8-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let snapshot = dir.join("catalog.prv2meta");
    index.write_snapshot(&snapshot).unwrap();
    let loaded = MetadataIndex::load_snapshot(&snapshot).unwrap();
    assert_eq!(loaded.records()[0].path, path);
    assert!(
        loaded
            .search(MetadataSearchRequest::filename("invalid"))
            .hits
            .is_empty()
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn metadata_snapshot_rejects_format_and_semantic_mismatch() {
    let index = MetadataIndex::build(records()).unwrap();
    let dir = std::env::temp_dir().join(format!(
        "personalrag-metadata-version-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let original = dir.join("original.prv2meta");
    index.write_snapshot(&original).unwrap();
    let bytes = std::fs::read(&original).unwrap();

    let mut bad_format = bytes.clone();
    bad_format[8..12].copy_from_slice(&999_u32.to_le_bytes());
    let bad_format_path = dir.join("bad-format.prv2meta");
    std::fs::write(&bad_format_path, bad_format).unwrap();
    assert!(matches!(
        MetadataIndex::load_snapshot(&bad_format_path),
        Err(MetadataError::FormatVersion(999))
    ));

    let mut bad_semantic = bytes;
    bad_semantic[12..16].copy_from_slice(&0xdead_beef_u32.to_le_bytes());
    let bad_semantic_path = dir.join("bad-semantic.prv2meta");
    std::fs::write(&bad_semantic_path, bad_semantic).unwrap();
    assert!(matches!(
        MetadataIndex::load_snapshot(&bad_semantic_path),
        Err(MetadataError::SemanticVersion(0xdead_beef))
    ));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn duplicate_file_ids_are_rejected() {
    let records = vec![
        MetadataRecord::file(1, "a.txt", 0, 0),
        MetadataRecord::file(1, "b.txt", 0, 0),
    ];
    assert!(matches!(
        MetadataIndex::build(records),
        Err(MetadataError::DuplicateFileId(1))
    ));
}
