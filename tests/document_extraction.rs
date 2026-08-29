use personalrag_v2::extraction::{DocumentKind, ExtractorConfig, extract_document};
use personalrag_v2::incremental::{
    BundleManifest, ContentQueryKind, DeltaOverlay, DeltaSnapshot, IncrementalState,
    gc_bundles_with_verification, load_bundle_with_verification, write_bundle,
    write_delta_generation, write_metadata_generation, write_state_generation,
};
use personalrag_v2::usn::UsnCheckpoint;
use personalrag_v2::{
    MetadataFileKind, MetadataIndex, MetadataRecord, PersistentError,
    load_latest_with_verification, publish_generation_with_extraction,
};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn temp_dir(label: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "personalrag-step5-{label}-{}-{stamp}",
        std::process::id()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

fn escape_pdf_text(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('(', "\\(")
        .replace(')', "\\)")
}

fn write_pdf(path: &Path, lines: &[&str], padding: usize) -> io::Result<()> {
    let mut content = String::from("BT /F1 12 Tf 72 720 Td ");
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            content.push_str(" 0 -36 Td ");
        }
        content.push('(');
        content.push_str(&escape_pdf_text(line));
        content.push_str(") Tj");
    }
    content.push_str(" ET");
    let objects = [
        b"<< /Type /Catalog /Pages 2 0 R >>".to_vec(),
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>".to_vec(),
        format!("<< /Length {} >>\nstream\n{}\nendstream", content.len(), content).into_bytes(),
        b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec(),
        {
            let mut value = format!("<< /Length {padding} >>\nstream\n").into_bytes();
            value.extend(std::iter::repeat_n(b'A', padding));
            value.extend_from_slice(b"\nendstream");
            value
        },
    ];
    let mut out = b"%PDF-1.4\n".to_vec();
    let mut offsets = vec![0_usize];
    for (index, object) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", index + 1).as_bytes());
        out.extend_from_slice(object);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for offset in offsets.iter().skip(1) {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
            objects.len() + 1
        )
        .as_bytes(),
    );
    fs::write(path, out)
}

#[cfg(windows)]
fn create_zip_fixture(root: &Path, path: &Path) -> io::Result<std::process::ExitStatus> {
    Command::new("tar.exe")
        .current_dir(root)
        .args(["-a", "-c", "-f"])
        .arg(path)
        .arg(".")
        .status()
}

#[cfg(not(windows))]
fn create_zip_fixture(root: &Path, path: &Path) -> io::Result<std::process::ExitStatus> {
    Command::new("zip")
        .current_dir(root)
        .args(["-q", "-r"])
        .arg(path)
        .arg(".")
        .status()
}

fn write_zip(path: &Path, entries: &[(&str, &str)]) {
    let root = temp_dir("zip-source");
    for (name, content) in entries {
        let target = root.join(name);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, content).unwrap();
    }
    let status = create_zip_fixture(&root, path).unwrap();
    assert!(status.success());
    fs::remove_dir_all(root).unwrap();
}

#[cfg(windows)]
#[test]
fn windows_fixture_zip_generation_uses_native_tar() {
    let root = temp_dir("windows-fixture-zip");
    let archive = root.join("fixture.docx");
    write_zip(
        &archive,
        &[(
            "word/document.xml",
            r#"<?xml version="1.0"?><w:document xmlns:w="w"><w:body><w:p><w:r><w:t>WINDOWS_FIXTURE_ZIP</w:t></w:r></w:p></w:body></w:document>"#,
        )],
    );

    let output = Command::new("tar.exe")
        .args(["-tf"])
        .arg(&archive)
        .output()
        .unwrap();
    assert!(output.status.success());
    let entries = String::from_utf8(output.stdout).unwrap();
    assert!(entries.replace('\\', "/").contains("word/document.xml"));

    fs::remove_dir_all(root).unwrap();
}

fn source_modified_ns(path: &Path) -> u128 {
    fs::metadata(path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
}

fn document_record(id: u64, relative: &str, full_path: &Path) -> MetadataRecord {
    MetadataRecord {
        file_id: id,
        path: PathBuf::from(relative),
        source_root: 0,
        size: fs::metadata(full_path).unwrap().len(),
        modified_ns: source_modified_ns(full_path),
        kind: MetadataFileKind::File,
        content_searchable: true,
        extractable: true,
    }
}

#[test]
fn pdf_docx_xlsx_pptx_extractors_produce_expected_hard_units() {
    let root = temp_dir("all-extractors");
    let config = ExtractorConfig::default();

    let pdf = root.join("sample.pdf");
    write_pdf(&pdf, &["PDF Alpha", "PDF Beta"], 4096).unwrap();
    let pdf_out = extract_document(&pdf, &config).unwrap();
    assert_eq!(pdf_out.kind, DocumentKind::Pdf);
    assert!(pdf_out.units.iter().any(|unit| unit.contains("PDF Alpha")));
    assert!(pdf_out.units.iter().any(|unit| unit.contains("PDF Beta")));

    let docx = root.join("sample.docx");
    write_zip(
        &docx,
        &[(
            "word/document.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="w"><w:body><w:p><w:r><w:t>DOCX Alpha</w:t></w:r></w:p><w:p><w:r><w:t>DOCX Beta &amp; Café</w:t></w:r></w:p></w:body></w:document>"#,
        )],
    );
    let docx_out = extract_document(&docx, &config).unwrap();
    assert_eq!(docx_out.kind, DocumentKind::Docx);
    assert_eq!(docx_out.units, vec!["DOCX Alpha", "DOCX Beta & Café"]);

    let xlsx = root.join("sample.xlsx");
    write_zip(
        &xlsx,
        &[
            (
                "xl/sharedStrings.xml",
                r#"<?xml version="1.0"?><sst><si><t>XLSX Shared</t></si></sst>"#,
            ),
            (
                "xl/worksheets/sheet1.xml",
                r#"<?xml version="1.0"?><worksheet><sheetData><row><c r="A1" t="s"><v>0</v></c><c r="B1" t="inlineStr"><is><t>XLSX Inline</t></is></c><c r="C1"><f>SUM(A1:B1)</f><v>42</v></c></row></sheetData></worksheet>"#,
            ),
        ],
    );
    let xlsx_out = extract_document(&xlsx, &config).unwrap();
    assert_eq!(xlsx_out.kind, DocumentKind::Xlsx);
    assert_eq!(
        xlsx_out.units,
        vec!["XLSX Shared", "XLSX Inline", "SUM(A1:B1) 42"]
    );

    let pptx = root.join("sample.pptx");
    write_zip(
        &pptx,
        &[(
            "ppt/slides/slide1.xml",
            r#"<?xml version="1.0"?><p:sld xmlns:p="p" xmlns:a="a"><p:cSld><a:p><a:r><a:t>PPTX Alpha</a:t></a:r></a:p><a:p><a:r><a:t>PPTX Beta</a:t></a:r></a:p></p:cSld></p:sld>"#,
        )],
    );
    let pptx_out = extract_document(&pptx, &config).unwrap();
    assert_eq!(pptx_out.kind, DocumentKind::Pptx);
    assert_eq!(pptx_out.units, vec!["PPTX Alpha", "PPTX Beta"]);

    fs::remove_dir_all(root).unwrap();
}

#[test]
fn persistent_pdf_search_uses_exact_verification_store_and_combined_capacity() {
    let root = temp_dir("pdf-persistent-root");
    let store = temp_dir("pdf-persistent-store");
    let pdf = root.join("manual.pdf");
    write_pdf(
        &pdf,
        &["PDF_SENTINEL Alpha Cafe", "BoundaryTail"],
        1024 * 1024,
    )
    .unwrap();
    let config = ExtractorConfig::default();
    let published = publish_generation_with_extraction(&root, &store, 1, 0, &config).unwrap();
    assert!(published.capacity.verification_bytes > 0);
    assert!(published.capacity.combined_source_ratio() <= 0.10);

    let index = load_latest_with_verification(&root, &store, &config).unwrap();
    assert_eq!(
        index.search_all("pdf_sentinel", false).unwrap().hits.len(),
        1
    );
    assert_eq!(
        index
            .search_regex_all("PDF_SENTINEL.*Alpha", true)
            .unwrap()
            .hits
            .len(),
        1
    );
    assert_eq!(
        index
            .search_wildcard_all("PDF_SENTINEL*BoundaryTail", true)
            .unwrap()
            .hits
            .len(),
        1
    );

    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(store).unwrap();
}

#[test]
fn extracted_docx_uses_frozen_unicode_nfc_and_full_fold_semantics() {
    let root = temp_dir("docx-unicode-root");
    let store = temp_dir("docx-unicode-store");
    let docx = root.join("unicode.docx");
    write_zip(
        &docx,
        &[(
            "word/document.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="w"><w:body><w:p><w:r><w:t>Straße café</w:t></w:r></w:p></w:body></w:document>"#,
        )],
    );
    fs::write(root.join("capacity-padding.log"), vec![b'x'; 1024 * 1024]).unwrap();
    let config = ExtractorConfig::default();
    publish_generation_with_extraction(&root, &store, 1, 0, &config).unwrap();
    let index = load_latest_with_verification(&root, &store, &config).unwrap();
    assert_eq!(index.search_all("STRASSE", false).unwrap().hits.len(), 1);
    assert_eq!(index.search_all("CAFÉ", false).unwrap().hits.len(), 1);
    assert_eq!(index.search_all("Straße", true).unwrap().hits.len(), 1);
    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(store).unwrap();
}

#[test]
fn corrupt_new_verification_generation_falls_back_without_reextracting_old_index() {
    let root = temp_dir("verify-fallback-root");
    let store = temp_dir("verify-fallback-store");
    let pdf = root.join("fallback.pdf");
    write_pdf(&pdf, &["FALLBACK_SENTINEL"], 1024 * 1024).unwrap();
    let config = ExtractorConfig::default();
    publish_generation_with_extraction(&root, &store, 1, 0, &config).unwrap();
    publish_generation_with_extraction(&root, &store, 2, 1, &config).unwrap();

    let verify2 = store.join("verify-00000000000000000002.prv2ver");
    let mut bytes = fs::read(&verify2).unwrap();
    let index = bytes.len() / 2;
    bytes[index] ^= 0x5A;
    fs::write(&verify2, bytes).unwrap();

    let loaded = load_latest_with_verification(&root, &store, &config).unwrap();
    assert_eq!(loaded.generation(), 1);
    assert_eq!(
        loaded
            .search_all("FALLBACK_SENTINEL", true)
            .unwrap()
            .hits
            .len(),
        1
    );

    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(store).unwrap();
}

#[test]
fn incremental_document_rename_reuses_base_verification_and_modify_reextracts() {
    let root = temp_dir("incremental-doc-root");
    let store = temp_dir("incremental-doc-store");
    let original = root.join("base.pdf");
    write_pdf(&original, &["OLD_PDF_SENTINEL"], 1024 * 1024).unwrap();
    let config = ExtractorConfig::default();
    publish_generation_with_extraction(&root, &store, 1, 0, &config).unwrap();
    let content = load_latest_with_verification(&root, &store, &config).unwrap();
    let base_record = document_record(100, "base.pdf", &original);
    let metadata = MetadataIndex::build(vec![base_record]).unwrap();
    let mut delta = DeltaOverlay::new(&metadata, 2, 1);

    let moved = root.join("moved.pdf");
    fs::rename(&original, &moved).unwrap();
    delta
        .rename(&metadata, 100, PathBuf::from("moved.pdf"))
        .unwrap();
    let renamed_hits = delta
        .content_search_first_batch_with_extraction(
            &root,
            &metadata,
            &content,
            ContentQueryKind::Literal("OLD_PDF_SENTINEL"),
            true,
            &config,
        )
        .unwrap();
    assert_eq!(renamed_hits.len(), 1);
    assert_eq!(renamed_hits[0].file_id, 100);

    write_pdf(&moved, &["NEW_PDF_SENTINEL"], 1024 * 1024 + 1024).unwrap();
    let changed = document_record(100, "moved.pdf", &moved);
    delta.upsert(&metadata, changed, true);
    assert!(
        delta
            .content_search_first_batch_with_extraction(
                &root,
                &metadata,
                &content,
                ContentQueryKind::Literal("OLD_PDF_SENTINEL"),
                true,
                &config,
            )
            .unwrap()
            .is_empty()
    );
    let new_hits = delta
        .content_search_first_batch_with_extraction(
            &root,
            &metadata,
            &content,
            ContentQueryKind::Literal("NEW_PDF_SENTINEL"),
            true,
            &config,
        )
        .unwrap();
    assert_eq!(new_hits.len(), 1);
    assert_eq!(new_hits[0].file_id, 100);

    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(store).unwrap();
}

#[test]
fn verification_capacity_hard_gate_fails_closed_before_publish() {
    let root = temp_dir("capacity-fail-root");
    let store = temp_dir("capacity-fail-store");
    let pdf = root.join("tiny.pdf");
    write_pdf(&pdf, &["TINY"], 0).unwrap();
    let config = ExtractorConfig::default();
    let error = publish_generation_with_extraction(&root, &store, 1, 0, &config).unwrap_err();
    assert!(matches!(error, PersistentError::CapacityExceeded(_)));
    assert!(!store.join("CURRENT").exists());
    assert!(!store.join("gen-00000000000000000001.prv2").exists());
    assert!(!store.join("verify-00000000000000000001.prv2ver").exists());
    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(store).unwrap();
}

#[test]
fn bundle_loader_falls_back_when_matching_verification_sidecar_is_corrupt() {
    let root = temp_dir("bundle-verify-root");
    let store = temp_dir("bundle-verify-store");
    let pdf = root.join("bundle.pdf");
    write_pdf(&pdf, &["BUNDLE_SENTINEL"], 1024 * 1024).unwrap();
    let config = ExtractorConfig::default();
    let metadata = MetadataIndex::build(vec![document_record(100, "bundle.pdf", &pdf)]).unwrap();

    for generation in [1_u64, 2] {
        publish_generation_with_extraction(
            &root,
            &store,
            generation,
            generation.saturating_sub(1),
            &config,
        )
        .unwrap();
        write_metadata_generation(&store, generation, &metadata).unwrap();
        write_delta_generation(
            &store,
            &DeltaSnapshot {
                generation,
                parent_generation: generation.saturating_sub(1),
                upserts: Vec::new(),
                tombstones: Vec::new(),
            },
        )
        .unwrap();
        write_state_generation(
            &store,
            &IncrementalState {
                generation,
                checkpoint: UsnCheckpoint {
                    journal_id: 7,
                    next_usn: generation as i64,
                },
                pending_renames: Vec::new(),
            },
        )
        .unwrap();
        write_bundle(
            &store,
            BundleManifest {
                generation,
                parent_generation: generation.saturating_sub(1),
                content_generation: generation,
                metadata_generation: generation,
                delta_generation: generation,
                state_generation: generation,
            },
        )
        .unwrap();
    }

    let verify2 = store.join("verify-00000000000000000002.prv2ver");
    let mut bytes = fs::read(&verify2).unwrap();
    bytes[80] ^= 0xA5;
    fs::write(&verify2, bytes).unwrap();

    let loaded = load_bundle_with_verification(&root, &store, &config).unwrap();
    assert_eq!(loaded.manifest.generation, 1);
    assert_eq!(
        loaded
            .content
            .search_all("BUNDLE_SENTINEL", true)
            .unwrap()
            .hits
            .len(),
        1
    );

    let removed = gc_bundles_with_verification(&root, &store, 2, &config).unwrap();
    assert!(
        removed
            .iter()
            .any(|path| path.file_name().unwrap() == "verify-00000000000000000002.prv2ver")
    );
    assert!(store.join("verify-00000000000000000001.prv2ver").exists());

    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(store).unwrap();
}

#[test]
fn document_source_drift_is_rejected_before_exact_verification() {
    let root = temp_dir("source-drift-root");
    let store = temp_dir("source-drift-store");
    let pdf = root.join("drift.pdf");
    write_pdf(&pdf, &["DRIFT_SENTINEL"], 1024 * 1024).unwrap();
    let config = ExtractorConfig::default();
    publish_generation_with_extraction(&root, &store, 1, 0, &config).unwrap();
    let index = load_latest_with_verification(&root, &store, &config).unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(&pdf)
        .unwrap()
        .write_all(b"changed")
        .unwrap();
    let error = index.search_all("DRIFT_SENTINEL", true).unwrap_err();
    assert!(matches!(error, PersistentError::SourceDrift(_)));
    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(store).unwrap();
}

#[test]
fn controlled_document_first_batch_stays_below_hard_slo() {
    let root = temp_dir("document-slo-root");
    let store = temp_dir("document-slo-store");
    for index in 0..8 {
        let path = root.join(format!("doc-{index}.pdf"));
        let line = if index == 5 {
            "RARE_DOCUMENT_SENTINEL"
        } else {
            "ordinary document text"
        };
        write_pdf(&path, &[line], 512 * 1024).unwrap();
    }
    let config = ExtractorConfig::default();
    let published = publish_generation_with_extraction(&root, &store, 1, 0, &config).unwrap();
    assert!(published.capacity.combined_source_ratio() <= 0.10);
    let index = load_latest_with_verification(&root, &store, &config).unwrap();
    let mut elapsed = Vec::new();
    let mut representative = None;
    for _ in 0..21 {
        let started = Instant::now();
        let outcome = index
            .search_first_batch("RARE_DOCUMENT_SENTINEL", true)
            .unwrap();
        elapsed.push(started.elapsed());
        assert_eq!(outcome.hits.len(), 1);
        representative = Some(outcome.metrics);
    }
    let cold = elapsed[0];
    elapsed.sort_unstable();
    let percentile = |percent: usize| {
        let index = (elapsed.len() - 1) * percent / 100;
        elapsed[index]
    };
    let p50 = percentile(50);
    let p95 = percentile(95);
    let p99 = percentile(99);
    let max = *elapsed.last().unwrap();
    let metrics = representative.unwrap();
    eprintln!(
        "STEP5_SLO selected_bytes={} verification_bytes={} combined_ratio={:.6} cold_ms={:.3} p50_ms={:.3} p95_ms={:.3} p99_ms={:.3} max_ms={:.3} candidate_blocks={} candidate_bytes={} verification_scan_bytes={}",
        published.capacity.selected_source_bytes,
        published.capacity.verification_bytes,
        published.capacity.combined_source_ratio(),
        cold.as_secs_f64() * 1000.0,
        p50.as_secs_f64() * 1000.0,
        p95.as_secs_f64() * 1000.0,
        p99.as_secs_f64() * 1000.0,
        max.as_secs_f64() * 1000.0,
        metrics.candidate_blocks,
        metrics.candidate_bytes,
        metrics.verification_bytes,
    );
    assert!(
        max <= Duration::from_millis(300),
        "document first-batch max {:?} exceeded 300 ms",
        max
    );
    fs::remove_dir_all(root).unwrap();
    fs::remove_dir_all(store).unwrap();
}
