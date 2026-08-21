#[allow(dead_code)]
#[rustfmt::skip]
#[path = "../../bridge-core/src/extractor.rs"]
mod extractor;
#[allow(dead_code)]
#[rustfmt::skip]
#[path = "../../bridge-core/src/office_cache.rs"]
mod office_cache;

use extractor::{ExtractionBudget, ExtractorRegistry, PreparedContent};
use office_cache::{
    OfficeExtractionConfig, OfficeExtractionRequest, OfficeExtractionService, OfficePreparedContent,
};
use personalrag_portable_search::{VNextDocumentInput, write_vnext_segment};
use std::{collections::BTreeMap, fs, sync::atomic::AtomicBool, time::Duration};

#[test]
fn production_cache_root_is_shared_across_publish_temp_and_final_but_arbitrary_indexes_are_isolated()
 {
    let app = std::path::Path::new("/tmp/personalrag-app");
    let final_index = app.join("portable-index");
    let build_index = app.join("portable-index-build-12345");
    let final_cache = OfficeExtractionService::cache_root_for_index_path(&final_index);
    let build_cache = OfficeExtractionService::cache_root_for_index_path(&build_index);
    assert_eq!(final_cache, app.join("office-extraction-cache"));
    assert_eq!(build_cache, final_cache);

    let arbitrary_a = app.join("test-index-a");
    let arbitrary_b = app.join("test-index-b");
    let cache_a = OfficeExtractionService::cache_root_for_index_path(&arbitrary_a);
    let cache_b = OfficeExtractionService::cache_root_for_index_path(&arbitrary_b);
    assert_eq!(cache_a, arbitrary_a.with_extension("office-cache"));
    assert_eq!(cache_b, arbitrary_b.with_extension("office-cache"));
    assert_ne!(cache_a, cache_b);
}

fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let offset = bytes.len() as u32;
        bytes.extend_from_slice(&0x04034b50u32.to_le_bytes());
        bytes.extend_from_slice(&20u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(name.len() as u16).to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(data);

        central.extend_from_slice(&0x02014b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(data.len() as u32).to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes());
        central.extend_from_slice(&0u32.to_le_bytes());
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let central_offset = bytes.len() as u32;
    let central_size = central.len() as u32;
    bytes.extend_from_slice(&central);
    bytes.extend_from_slice(&0x06054b50u32.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    bytes.extend_from_slice(&central_size.to_le_bytes());
    bytes.extend_from_slice(&central_offset.to_le_bytes());
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes
}

fn config() -> OfficeExtractionConfig {
    OfficeExtractionConfig {
        max_workers: 4,
        memory_budget_bytes: 4 * 1024 * 1024,
        cache_soft_limit_bytes: 1024 * 1024,
        cache_target_bytes: 512 * 1024,
        cache_grace: Duration::ZERO,
    }
}

#[test]
fn cache_reuses_searchable_xml_and_ignores_media_only_changes() {
    let root = std::env::temp_dir().join(format!("pr-office-cache-{}-1", std::process::id()));
    let source = root.join("input.docx");
    let cache = root.join("cache");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        &source,
        stored_zip(&[
            (
                "word/document.xml",
                br#"<w:document><w:t>Cache Marker</w:t></w:document>"#,
            ),
            ("word/media/image1.png", b"image-A"),
        ]),
    )
    .unwrap();
    let service = OfficeExtractionService::new(
        cache,
        ExtractionBudget::from_max_file_bytes(1024 * 1024),
        config(),
    );
    let req = OfficeExtractionRequest {
        source_index: 0,
        path: source.clone(),
        source_bytes: fs::metadata(&source).unwrap().len(),
    };
    let cancel = AtomicBool::new(false);
    let (first, report1) = service.prepare_many(std::slice::from_ref(&req), &cancel);
    assert_eq!(report1.cache_hits, 0);
    assert_eq!(report1.cache_misses, 1);
    let first_key = match &first[0] {
        OfficePreparedContent::Cached {
            cache_key,
            cache_hit,
            path,
            ..
        } => {
            assert!(!cache_hit);
            assert!(fs::read_to_string(path).unwrap().contains("Cache Marker"));
            cache_key.clone()
        }
        other => panic!("unexpected first result: {other:?}"),
    };
    let (second, report2) = service.prepare_many(std::slice::from_ref(&req), &cancel);
    assert_eq!(report2.cache_hits, 1);
    let second_key = match &second[0] {
        OfficePreparedContent::Cached {
            cache_key,
            cache_hit,
            ..
        } => {
            assert!(cache_hit);
            cache_key.clone()
        }
        other => panic!("unexpected second result: {other:?}"),
    };
    assert_eq!(first_key, second_key);

    fs::write(
        &source,
        stored_zip(&[
            (
                "word/document.xml",
                br#"<w:document><w:t>Cache Marker</w:t></w:document>"#,
            ),
            ("word/media/image1.png", b"image-B-is-different"),
        ]),
    )
    .unwrap();
    let req2 = OfficeExtractionRequest {
        source_index: 0,
        path: source.clone(),
        source_bytes: fs::metadata(&source).unwrap().len(),
    };
    let (third, report3) = service.prepare_many(std::slice::from_ref(&req2), &cancel);
    assert_eq!(report3.cache_hits, 1);
    let third_key = match &third[0] {
        OfficePreparedContent::Cached { cache_key, .. } => cache_key.clone(),
        other => panic!("unexpected third result: {other:?}"),
    };
    assert_eq!(first_key, third_key);

    fs::write(
        &source,
        stored_zip(&[(
            "word/document.xml",
            br#"<w:document><w:t>Changed Search Text</w:t></w:document>"#,
        )]),
    )
    .unwrap();
    let req3 = OfficeExtractionRequest {
        source_index: 0,
        path: source.clone(),
        source_bytes: fs::metadata(&source).unwrap().len(),
    };
    let (fourth, report4) = service.prepare_many(std::slice::from_ref(&req3), &cancel);
    assert_eq!(report4.cache_hits, 0);
    let fourth_key = match &fourth[0] {
        OfficePreparedContent::Cached {
            cache_key, path, ..
        } => {
            assert!(
                fs::read_to_string(path)
                    .unwrap()
                    .contains("Changed Search Text")
            );
            cache_key.clone()
        }
        other => panic!("unexpected fourth result: {other:?}"),
    };
    assert_ne!(first_key, fourth_key);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn parallel_results_are_source_ordered_and_live_round_trips() {
    let root = std::env::temp_dir().join(format!("pr-office-cache-{}-2", std::process::id()));
    let cache = root.join("cache");
    fs::create_dir_all(&root).unwrap();
    let mut requests = Vec::new();
    for index in 0..8usize {
        let source = root.join(format!("{index:02}.docx"));
        let xml = format!("<w:document><w:t>marker-{index}</w:t></w:document>");
        fs::write(
            &source,
            stored_zip(&[("word/document.xml", xml.as_bytes())]),
        )
        .unwrap();
        requests.push(OfficeExtractionRequest {
            source_index: index,
            source_bytes: fs::metadata(&source).unwrap().len(),
            path: source,
        });
    }
    let service = OfficeExtractionService::new(
        cache,
        ExtractionBudget::from_max_file_bytes(1024 * 1024),
        config(),
    );
    let (prepared, report) = service.prepare_many(&requests, &AtomicBool::new(false));
    assert!(report.workers > 1);
    assert_eq!(prepared.len(), requests.len());
    assert_eq!(
        prepared
            .iter()
            .map(OfficePreparedContent::source_index)
            .collect::<Vec<_>>(),
        (0..8).collect::<Vec<_>>()
    );
    let mut live = BTreeMap::new();
    live.insert(
        "a.docx".to_owned(),
        "0123456789abcdef0123456789abcdef".to_owned(),
    );
    service.publish_live(&live).unwrap();
    assert_eq!(service.load_live(), live);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn corrupt_cached_text_is_treated_as_miss_and_repaired() {
    let root = std::env::temp_dir().join(format!("pr-office-cache-{}-3", std::process::id()));
    let source = root.join("input.docx");
    let cache = root.join("cache");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        &source,
        stored_zip(&[(
            "word/document.xml",
            br#"<w:document><w:t>Repair Marker</w:t></w:document>"#,
        )]),
    )
    .unwrap();
    let service = OfficeExtractionService::new(
        cache,
        ExtractionBudget::from_max_file_bytes(1024 * 1024),
        config(),
    );
    let request = OfficeExtractionRequest {
        source_index: 0,
        path: source.clone(),
        source_bytes: fs::metadata(&source).unwrap().len(),
    };
    let cancel = AtomicBool::new(false);
    let (first, _) = service.prepare_many(std::slice::from_ref(&request), &cancel);
    let cached = match &first[0] {
        OfficePreparedContent::Cached { path, .. } => path.clone(),
        other => panic!("unexpected cache result: {other:?}"),
    };
    fs::write(&cached, b"corrupt-cache-bytes").unwrap();
    let (second, report) = service.prepare_many(std::slice::from_ref(&request), &cancel);
    assert_eq!(report.cache_hits, 0);
    assert_eq!(report.cache_misses, 1);
    let repaired = match &second[0] {
        OfficePreparedContent::Cached {
            path, cache_hit, ..
        } => {
            assert!(!cache_hit);
            fs::read_to_string(path).unwrap()
        }
        other => panic!("unexpected repaired result: {other:?}"),
    };
    assert!(repaired.contains("Repair Marker"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn cache_failure_falls_back_to_in_memory_extraction() {
    let root = std::env::temp_dir().join(format!("pr-office-cache-{}-4", std::process::id()));
    let source = root.join("input.docx");
    let bad_cache_root = root.join("not-a-directory");
    fs::create_dir_all(&root).unwrap();
    fs::write(
        &source,
        stored_zip(&[(
            "word/document.xml",
            br#"<w:document><w:t>Fallback Marker</w:t></w:document>"#,
        )]),
    )
    .unwrap();
    fs::write(&bad_cache_root, b"block cache directory creation").unwrap();
    let service = OfficeExtractionService::new(
        bad_cache_root,
        ExtractionBudget::from_max_file_bytes(1024 * 1024),
        config(),
    );
    let request = OfficeExtractionRequest {
        source_index: 0,
        path: source.clone(),
        source_bytes: fs::metadata(&source).unwrap().len(),
    };
    let (prepared, report) = service.prepare_many(&[request], &AtomicBool::new(false));
    assert_eq!(report.cache_write_fallbacks, 1);
    match &prepared[0] {
        OfficePreparedContent::Extracted { text, .. } => assert!(text.contains("Fallback Marker")),
        other => panic!("unexpected fallback result: {other:?}"),
    }
    let _ = fs::remove_dir_all(root);
}

#[test]
fn gc_never_deletes_live_object_and_reclaims_unreferenced_cache() {
    let root = std::env::temp_dir().join(format!("pr-office-cache-{}-5", std::process::id()));
    let cache = root.join("cache");
    fs::create_dir_all(&root).unwrap();
    let mut cfg = config();
    cfg.cache_soft_limit_bytes = 1;
    cfg.cache_target_bytes = 1;
    cfg.cache_grace = Duration::ZERO;
    let service = OfficeExtractionService::new(
        cache,
        ExtractionBudget::from_max_file_bytes(1024 * 1024),
        cfg,
    );
    let mut requests = Vec::new();
    for index in 0..2usize {
        let source = root.join(format!("gc-{index}.docx"));
        let xml = format!("<w:document><w:t>gc-marker-{index}</w:t></w:document>");
        fs::write(
            &source,
            stored_zip(&[("word/document.xml", xml.as_bytes())]),
        )
        .unwrap();
        requests.push(OfficeExtractionRequest {
            source_index: index,
            source_bytes: fs::metadata(&source).unwrap().len(),
            path: source,
        });
    }
    let (prepared, _) = service.prepare_many(&requests, &AtomicBool::new(false));
    let (live_key, live_path) = match &prepared[0] {
        OfficePreparedContent::Cached {
            cache_key, path, ..
        } => (cache_key.clone(), path.clone()),
        other => panic!("unexpected live cache result: {other:?}"),
    };
    let dead_path = match &prepared[1] {
        OfficePreparedContent::Cached { path, .. } => path.clone(),
        other => panic!("unexpected dead cache result: {other:?}"),
    };
    let mut live = BTreeMap::new();
    live.insert("gc-0.docx".to_owned(), live_key);
    service.publish_live(&live).unwrap();
    let report = service.gc(&live).unwrap();
    assert!(report.deleted_objects >= 1);
    assert!(live_path.exists());
    assert!(!dead_path.exists());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn cached_office_text_and_vnext_segment_are_byte_identical_to_legacy_extraction() {
    let root = std::env::temp_dir().join(format!("pr-office-cache-{}-6", std::process::id()));
    let cache = root.join("cache");
    fs::create_dir_all(&root).unwrap();
    let fixtures: [(&str, Vec<u8>); 3] = [
        (
            "a.docx",
            stored_zip(&[(
                "word/document.xml",
                br#"<w:document><w:t>Alpha Office Marker</w:t></w:document>"#,
            )]),
        ),
        (
            "b.xlsx",
            stored_zip(&[(
                "xl/worksheets/sheet1.xml",
                br#"<worksheet><sheetData><row><c><is><t>Beta Sheet Marker</t></is></c></row></sheetData></worksheet>"#,
            )]),
        ),
        (
            "c.pptx",
            stored_zip(&[(
                "ppt/slides/slide1.xml",
                br#"<p:sld><a:t>Gamma Slide Marker</a:t></p:sld>"#,
            )]),
        ),
    ];
    let mut requests = Vec::new();
    for (index, (name, bytes)) in fixtures.iter().enumerate() {
        let path = root.join(name);
        fs::write(&path, bytes).unwrap();
        requests.push(OfficeExtractionRequest {
            source_index: index,
            source_bytes: fs::metadata(&path).unwrap().len(),
            path,
        });
    }
    let budget = ExtractionBudget::from_max_file_bytes(1024 * 1024);
    let registry = ExtractorRegistry::new();
    let mut legacy_docs = Vec::new();
    for (index, request) in requests.iter().enumerate() {
        let PreparedContent::Extracted(document) = registry.prepare(&request.path, budget).unwrap()
        else {
            panic!("expected legacy Office extraction");
        };
        let mut content = document.text.into_bytes();
        content.make_ascii_lowercase();
        legacy_docs.push(VNextDocumentInput::new(
            index as u64 + 1,
            fixtures[index].0,
            content,
        ));
    }

    let service = OfficeExtractionService::new(cache, budget, config());
    let (prepared, report) = service.prepare_many(&requests, &AtomicBool::new(false));
    assert_eq!(report.cache_misses, 3);
    let mut cached_docs = Vec::new();
    for item in prepared {
        let source_index = item.source_index();
        let mut content = match item {
            OfficePreparedContent::Cached { path, .. } => fs::read(path).unwrap(),
            OfficePreparedContent::Extracted { text, .. } => text.into_bytes(),
            OfficePreparedContent::Failed { error, .. } => {
                panic!("cache extraction failed: {error}")
            }
        };
        content.make_ascii_lowercase();
        cached_docs.push(VNextDocumentInput::new(
            source_index as u64 + 1,
            fixtures[source_index].0,
            content,
        ));
    }
    cached_docs.sort_by_key(|doc| doc.logical_id);
    assert_eq!(legacy_docs.len(), cached_docs.len());
    for (legacy, cached) in legacy_docs.iter().zip(&cached_docs) {
        assert_eq!(legacy.logical_id, cached.logical_id);
        assert_eq!(legacy.display_path, cached.display_path);
        assert_eq!(legacy.normalized_content, cached.normalized_content);
    }

    let legacy_segment = root.join("legacy.prseg2");
    let cached_segment = root.join("cached.prseg2");
    write_vnext_segment(&legacy_segment, &legacy_docs).unwrap();
    write_vnext_segment(&cached_segment, &cached_docs).unwrap();
    assert_eq!(
        fs::read(legacy_segment).unwrap(),
        fs::read(cached_segment).unwrap()
    );
    let _ = fs::remove_dir_all(root);
}
