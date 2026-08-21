use personalrag_portable_search::{
    VNextDocumentInput, VNextSegmentReader, fold_ascii, write_vnext_segment_with_block_size,
};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);
fn temp_path(label: &str) -> PathBuf {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "personalrag-vnext-persist-{label}-{}-{id}.prseg2",
        std::process::id()
    ))
}
fn doc(id: u64, path: &str, content: &str) -> VNextDocumentInput {
    VNextDocumentInput::new(id, path, fold_ascii(content.as_bytes()))
}
fn naive_content(docs: &[VNextDocumentInput], q: &[u8]) -> Vec<u16> {
    let q = fold_ascii(q);
    docs.iter()
        .enumerate()
        .filter_map(|(i, d)| {
            d.normalized_content
                .windows(q.len())
                .any(|w| w == q)
                .then_some(i as u16)
        })
        .collect()
}
fn naive_path(docs: &[VNextDocumentInput], q: &[u8]) -> Vec<u16> {
    let q = fold_ascii(q);
    docs.iter()
        .enumerate()
        .filter_map(|(i, d)| {
            let p = fold_ascii(d.display_path.as_bytes());
            p.windows(q.len()).any(|w| w == q).then_some(i as u16)
        })
        .collect()
}
fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 1_469_598_103_934_665_603;
    const PRIME: u64 = 1_099_511_628_211;
    bytes.iter().fold(OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
    })
}

#[test]
fn vnext_persistent_q1_q2_match_oracle_including_block_boundary() {
    let path = temp_path("content-short");
    let docs = vec![
        doc(1, "a.txt", "abcdefghi"),
        doc(2, "b.txt", "zzABzz"),
        doc(3, "c.txt", "日本語検索"),
        doc(4, "d.txt", ""),
    ];
    write_vnext_segment_with_block_size(&path, &docs, 4).unwrap();
    let r = VNextSegmentReader::open(&path).unwrap();
    for q in [
        b"a".as_slice(),
        b"AB",
        b"de",
        b"ef",
        b"hi",
        "日".as_bytes(),
        b"qq",
    ] {
        assert_eq!(
            r.search_content(q).unwrap(),
            naive_content(&docs, q),
            "q={:?}",
            String::from_utf8_lossy(q)
        );
    }
    fs::remove_file(path).unwrap();
}

#[test]
fn vnext_persistent_path_q1_q2_q3plus_match_oracle() {
    let path = temp_path("path");
    let docs = (0..80usize)
        .map(|i| {
            doc(
                i as u64,
                &format!("Src/Group_{:02}/Module_{:03}_Needle.CPP", i % 7, i),
                "body",
            )
        })
        .collect::<Vec<_>>();
    write_vnext_segment_with_block_size(&path, &docs, 8192).unwrap();
    let r = VNextSegmentReader::open(&path).unwrap();
    for q in [
        b"s".as_slice(),
        b"SR",
        b"src",
        b"module_042",
        b"NEEDLE.cpp",
        b"group_03/module",
        b"missing",
    ] {
        assert_eq!(
            r.search_path(q).unwrap(),
            naive_path(&docs, q),
            "q={:?}",
            String::from_utf8_lossy(q)
        );
        assert_eq!(r.search_name(q).unwrap(), naive_path(&docs, q));
    }
    fs::remove_file(path).unwrap();
}

#[test]
fn vnext_persistent_fixed_index_corruption_fails_closed_even_with_repaired_checksums() {
    let original = temp_path("fixed-corrupt-original");
    let docs = vec![doc(1, "alpha.txt", "abcabc")];
    write_vnext_segment_with_block_size(&original, &docs, 4).unwrap();
    let mut bytes = fs::read(&original).unwrap();
    // Slot 7 is content q1. Corrupt key 'a' encoding to an unknown value, then repair checksums.
    let entry = 128 + 7 * 32;
    let off = u64::from_le_bytes(bytes[entry + 8..entry + 16].try_into().unwrap()) as usize;
    let len = u64::from_le_bytes(bytes[entry + 16..entry + 24].try_into().unwrap()) as usize;
    let meta = off + 32 + (b'a' as usize) * 16;
    bytes[meta] = 0x7f;
    let section_checksum = fnv1a(&bytes[off..off + len]);
    bytes[entry + 24..entry + 32].copy_from_slice(&section_checksum.to_le_bytes());
    let footer = u64::from_le_bytes(bytes[48..56].try_into().unwrap()) as usize;
    let file_checksum = fnv1a(&bytes[..footer]);
    bytes[footer + 24..footer + 32].copy_from_slice(&file_checksum.to_le_bytes());
    let corrupt = temp_path("fixed-corrupt-repaired");
    fs::write(&corrupt, bytes).unwrap();
    assert!(VNextSegmentReader::open(&corrupt).is_err());
    fs::remove_file(original).unwrap();
    fs::remove_file(corrupt).unwrap();
}

#[test]
fn vnext_persistent_q2_uses_sparse_fixed_format_while_q1_stays_dense() {
    let path = temp_path("fixed-format");
    let docs = vec![
        doc(1, "Src/Alpha/Module_42.cpp", "timeout alpha beta"),
        doc(2, "Src/Beta/Other_07.cpp", "gamma delta timeout"),
    ];
    write_vnext_segment_with_block_size(&path, &docs, 8192).unwrap();
    let bytes = fs::read(&path).unwrap();
    let section_magic = |slot: usize| -> &[u8] {
        let entry = 128 + slot * 32;
        let off = u64::from_le_bytes(bytes[entry + 8..entry + 16].try_into().unwrap()) as usize;
        &bytes[off..off + 8]
    };
    assert_eq!(section_magic(7), b"PRFIX001", "content q1 remains dense");
    assert_eq!(
        section_magic(8),
        b"PRFIX002",
        "content q2 uses sparse metadata"
    );
    assert_eq!(section_magic(9), b"PRFIX001", "path q1 remains dense");
    assert_eq!(
        section_magic(10),
        b"PRFIX002",
        "path q2 uses sparse metadata"
    );
    fs::remove_file(path).unwrap();
}

#[test]
fn vnext_persistent_sparse_q2_corruption_fails_closed_with_repaired_checksums() {
    let original = temp_path("sparse-q2-corrupt-original");
    let docs = vec![doc(1, "alpha.txt", "abcabc timeout")];
    write_vnext_segment_with_block_size(&original, &docs, 8192).unwrap();
    let mut bytes = fs::read(&original).unwrap();
    // Slot 8 is content q2. Its sparse metadata begins at section+32; corrupt the first
    // present-key encoding while repairing both checksums so structure validation must catch it.
    let entry = 128 + 8 * 32;
    let off = u64::from_le_bytes(bytes[entry + 8..entry + 16].try_into().unwrap()) as usize;
    let len = u64::from_le_bytes(bytes[entry + 16..entry + 24].try_into().unwrap()) as usize;
    assert_eq!(&bytes[off..off + 8], b"PRFIX002");
    bytes[off + 32 + 2] = 0x7f;
    let section_checksum = fnv1a(&bytes[off..off + len]);
    bytes[entry + 24..entry + 32].copy_from_slice(&section_checksum.to_le_bytes());
    let footer = u64::from_le_bytes(bytes[48..56].try_into().unwrap()) as usize;
    let file_checksum = fnv1a(&bytes[..footer]);
    bytes[footer + 24..footer + 32].copy_from_slice(&file_checksum.to_le_bytes());
    let corrupt = temp_path("sparse-q2-corrupt-repaired");
    fs::write(&corrupt, bytes).unwrap();
    assert!(VNextSegmentReader::open(&corrupt).is_err());
    fs::remove_file(original).unwrap();
    fs::remove_file(corrupt).unwrap();
}
