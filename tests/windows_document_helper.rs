#![cfg(windows)]

use personalrag_v2::extraction::{ExtractorConfig, extract_document};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_dir(tag: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "personalrag-windows-doc-{tag}-{}-{stamp}",
        std::process::id()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn native_zip_reader_handles_verbatim_windows_document_paths() {
    let base = temp_dir("zip");
    let source = base.join("source");
    let word = source.join("word");
    fs::create_dir_all(&word).unwrap();
    fs::write(
        word.join("document.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="w"><w:body><w:p><w:r><w:t>PR_WINDOWS_DOC_NATIVE_ZIP</w:t></w:r></w:p></w:body></w:document>"#,
    )
    .unwrap();
    let docx = base.join("fixture.docx");

    let status = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Add-Type -AssemblyName System.IO.Compression.FileSystem; [IO.Compression.ZipFile]::CreateFromDirectory($env:PR_ZIP_SOURCE,$env:PR_ZIP_TARGET)",
        ])
        .env("PR_ZIP_SOURCE", &source)
        .env("PR_ZIP_TARGET", &docx)
        .status()
        .unwrap();
    assert!(status.success());

    let config = ExtractorConfig::discover();
    let zip_path = config.unzip.to_string_lossy().to_ascii_lowercase();
    assert!(
        !zip_path.contains(r"git\usr\bin\unzip.exe"),
        "MSYS Git unzip must not be auto-selected: {}",
        config.unzip.display()
    );

    let canonical = fs::canonicalize(&docx).unwrap();
    let extracted = extract_document(&canonical, &config).unwrap();
    assert_eq!(extracted.units, vec!["PR_WINDOWS_DOC_NATIVE_ZIP"]);

    fs::remove_dir_all(base).unwrap();
}
