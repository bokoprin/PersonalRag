use personalrag_portable_search::{
    VNextDocumentInput, VNextQ3PostingEncoding, VNextSegmentReader,
    write_vnext_segment_with_block_size,
};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn temp_path(label: &str) -> PathBuf {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "personalrag-vnext-{label}-{}-{id}.prseg2",
        std::process::id()
    ))
}

fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 1_469_598_103_934_665_603;
    const PRIME: u64 = 1_099_511_628_211;
    bytes.iter().fold(OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(PRIME)
    })
}

fn convert_to_v5_for_legacy_checksum_repair(bytes: &mut [u8]) {
    bytes[0..8].copy_from_slice(b"PRSEG2A5");
    bytes[8..12].copy_from_slice(&5u32.to_le_bytes());
    let footer_off = u64::from_le_bytes(bytes[48..56].try_into().unwrap()) as usize;
    bytes[footer_off..footer_off + 8].copy_from_slice(b"PR2FTR05");
    bytes[footer_off + 8..footer_off + 12].copy_from_slice(&5u32.to_le_bytes());
}

fn docs() -> Vec<VNextDocumentInput> {
    vec![
        VNextDocumentInput::new(42, "src/日本語.txt", b"abcdefghijklmno".to_vec()),
        VNextDocumentInput::new(99, "src/empty.rs", Vec::new()),
        VNextDocumentInput::new(123_456_789, "src/third.cpp", b"xyz123".to_vec()),
    ]
}

#[test]
fn vnext_roundtrip_maps_documents_blocks_and_content() {
    let path = temp_path("roundtrip");
    let report = write_vnext_segment_with_block_size(&path, &docs(), 8).unwrap();
    assert_eq!(report.docs, 3);
    assert_eq!(report.blocks, 3);
    assert_eq!(report.block_size, 8);
    assert!(report.q3_keys > 0);
    assert!(report.q3_posting_ids > 0);
    assert!(report.q3_active_shards > 0);

    let reader = VNextSegmentReader::open(&path).unwrap();
    assert_eq!(reader.doc_count(), 3);
    assert_eq!(reader.block_count(), 3);
    assert_eq!(reader.block_size(), 8);

    assert_eq!(reader.logical_id(0).unwrap(), 42);
    assert_eq!(reader.display_path(0).unwrap(), "src/日本語.txt");
    assert_eq!(reader.first_block(0).unwrap(), 0);
    assert_eq!(reader.document_block_count(0).unwrap(), 2);
    assert_eq!(reader.normalized_content(0).unwrap(), b"abcdefghijklmno");
    assert_eq!(reader.block_content(0).unwrap(), b"abcdefgh");
    assert_eq!(reader.block_content(1).unwrap(), b"ijklmno");

    assert_eq!(reader.logical_id(1).unwrap(), 99);
    assert_eq!(reader.document_block_count(1).unwrap(), 0);
    assert_eq!(reader.normalized_content(1).unwrap(), b"");

    assert_eq!(reader.logical_id(2).unwrap(), 123_456_789);
    assert_eq!(reader.display_path(2).unwrap(), "src/third.cpp");
    assert_eq!(reader.first_block(2).unwrap(), 2);
    assert_eq!(reader.document_block_count(2).unwrap(), 1);
    assert_eq!(reader.normalized_content(2).unwrap(), b"xyz123");
    assert_eq!(reader.block(2).unwrap().doc_id, 2);

    fs::remove_file(path).unwrap();
}

#[test]
fn vnext_serialization_is_deterministic() {
    let first = temp_path("deterministic-a");
    let second = temp_path("deterministic-b");
    write_vnext_segment_with_block_size(&first, &docs(), 8).unwrap();
    write_vnext_segment_with_block_size(&second, &docs(), 8).unwrap();
    assert_eq!(fs::read(&first).unwrap(), fs::read(&second).unwrap());
    fs::remove_file(first).unwrap();
    fs::remove_file(second).unwrap();
}

#[test]
fn vnext_reader_fails_closed_on_truncation_corruption_and_bad_magic() {
    let original = temp_path("corrupt-original");
    write_vnext_segment_with_block_size(&original, &docs(), 8).unwrap();
    let bytes = fs::read(&original).unwrap();

    let truncated = temp_path("truncated");
    fs::write(&truncated, &bytes[..bytes.len() - 1]).unwrap();
    assert!(VNextSegmentReader::open(&truncated).is_err());

    let corrupt = temp_path("corrupt-byte");
    let mut corrupt_bytes = bytes.clone();
    let content_section_entry = 128 + 3 * 32;
    let content_off = u64::from_le_bytes(
        corrupt_bytes[content_section_entry + 8..content_section_entry + 16]
            .try_into()
            .unwrap(),
    ) as usize;
    corrupt_bytes[content_off] ^= 0x5a;
    fs::write(&corrupt, &corrupt_bytes).unwrap();
    assert!(VNextSegmentReader::open(&corrupt).is_err());

    let bad_magic = temp_path("bad-magic");
    let mut magic_bytes = bytes;
    magic_bytes[0] ^= 0xff;
    fs::write(&bad_magic, &magic_bytes).unwrap();
    assert!(VNextSegmentReader::open(&bad_magic).is_err());

    for path in [original, truncated, corrupt, bad_magic] {
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn vnext_reader_rejects_structurally_malformed_file_even_with_valid_checksum() {
    let original = temp_path("malformed-original");
    write_vnext_segment_with_block_size(&original, &docs(), 8).unwrap();
    let mut bytes = fs::read(&original).unwrap();
    convert_to_v5_for_legacy_checksum_repair(&mut bytes);

    // Claim one extra document while keeping every checksum valid. The reader must
    // reject the inconsistent SoA shape rather than trusting integrity alone.
    bytes[24..28].copy_from_slice(&4u32.to_le_bytes());
    let footer_off = u64::from_le_bytes(bytes[48..56].try_into().unwrap()) as usize;
    let checksum = fnv1a(&bytes[..footer_off]);
    bytes[footer_off + 24..footer_off + 32].copy_from_slice(&checksum.to_le_bytes());

    let malformed = temp_path("malformed-valid-checksum");
    fs::write(&malformed, bytes).unwrap();
    assert!(VNextSegmentReader::open(&malformed).is_err());

    fs::remove_file(original).unwrap();
    fs::remove_file(malformed).unwrap();
}

#[test]
fn vnext_format_uses_explicit_little_endian_fields() {
    let path = temp_path("little-endian");
    write_vnext_segment_with_block_size(&path, &docs(), 0x1122_3344).unwrap();
    let bytes = fs::read(&path).unwrap();

    assert_eq!(&bytes[0..8], b"PRSEG2A6");
    assert_eq!(&bytes[8..12], &6u32.to_le_bytes());
    assert_eq!(&bytes[20..24], &14u32.to_le_bytes());
    assert_eq!(&bytes[12..16], &0x0102_0304u32.to_le_bytes());
    assert_eq!(&bytes[24..28], &3u32.to_le_bytes());
    assert_eq!(&bytes[32..36], &0x1122_3344u32.to_le_bytes());

    let doc_soa_off = u64::from_le_bytes(bytes[136..144].try_into().unwrap()) as usize;
    assert_eq!(&bytes[doc_soa_off..doc_soa_off + 8], &42u64.to_le_bytes());

    VNextSegmentReader::open(&path).unwrap();
    fs::remove_file(path).unwrap();
}

#[test]
fn vnext_default_writer_uses_8k_blocks_and_supports_empty_segment() {
    use personalrag_portable_search::write_vnext_segment;

    let populated = temp_path("default-block-size");
    write_vnext_segment(&populated, &docs()).unwrap();
    assert_eq!(
        VNextSegmentReader::open(&populated).unwrap().block_size(),
        8192
    );

    let empty = temp_path("empty");
    let report = write_vnext_segment(&empty, &[]).unwrap();
    assert_eq!(report.docs, 0);
    assert_eq!(report.blocks, 0);
    let reader = VNextSegmentReader::open(&empty).unwrap();
    assert_eq!(reader.doc_count(), 0);
    assert_eq!(reader.block_count(), 0);

    fs::remove_file(populated).unwrap();
    fs::remove_file(empty).unwrap();
}

#[test]
fn vnext_writer_rejects_zero_block_size() {
    let path = temp_path("zero-block");
    assert!(write_vnext_segment_with_block_size(&path, &docs(), 0).is_err());
    assert!(!path.exists());
}

#[test]
fn vnext_q3_postings_use_start_block_and_look_across_block_boundary() {
    let path = temp_path("q3-boundary");
    let docs = vec![VNextDocumentInput::new(
        1,
        "boundary.txt",
        b"abcdefghijk".to_vec(),
    )];
    write_vnext_segment_with_block_size(&path, &docs, 8).unwrap();
    let reader = VNextSegmentReader::open(&path).unwrap();

    assert_eq!(
        reader
            .q3_posting(*b"ghi")
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        vec![0]
    );
    assert_eq!(
        reader
            .q3_posting(*b"hij")
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        vec![0]
    );
    assert_eq!(
        reader
            .q3_posting(*b"ijk")
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        vec![1]
    );

    fs::remove_file(path).unwrap();
}

#[test]
fn vnext_q3_postings_dedup_blocks_and_never_cross_document_boundaries() {
    let path = temp_path("q3-dedup-doc-boundary");
    let docs = vec![
        VNextDocumentInput::new(1, "a.txt", b"abcabcabc".to_vec()),
        VNextDocumentInput::new(2, "b.txt", b"xy".to_vec()),
        VNextDocumentInput::new(3, "c.txt", b"zdef".to_vec()),
    ];
    let report = write_vnext_segment_with_block_size(&path, &docs, 16).unwrap();
    assert!(report.q3_keys > 0);
    let reader = VNextSegmentReader::open(&path).unwrap();

    assert_eq!(
        reader
            .q3_posting(*b"abc")
            .unwrap()
            .iter()
            .collect::<Vec<_>>(),
        vec![0]
    );
    assert!(reader.q3_posting(*b"xyz").unwrap().is_empty());
    assert!(reader.q3_posting(*b"abz").unwrap().is_empty());
    assert!(reader.q3_posting([0xff, 0x00, 0x01]).unwrap().is_empty());

    fs::remove_file(path).unwrap();
}

#[test]
fn vnext_q3_postings_are_sorted_across_blocks() {
    let path = temp_path("q3-multiblock");
    let docs = vec![VNextDocumentInput::new(1, "many.txt", vec![b'a'; 20])];
    write_vnext_segment_with_block_size(&path, &docs, 8).unwrap();
    let reader = VNextSegmentReader::open(&path).unwrap();
    let posting = reader.q3_posting(*b"aaa").unwrap();

    assert_eq!(posting.len(), 3);
    assert_eq!(posting.get(0), Some(0));
    assert_eq!(posting.get(1), Some(1));
    assert_eq!(posting.get(2), Some(2));
    assert_eq!(posting.iter().collect::<Vec<_>>(), vec![0, 1, 2]);

    fs::remove_file(path).unwrap();
}

#[test]
fn vnext_reader_rejects_malformed_q3_directory_even_with_valid_checksums() {
    let original = temp_path("q3-malformed-original");
    write_vnext_segment_with_block_size(&original, &docs(), 8).unwrap();
    let mut bytes = fs::read(&original).unwrap();
    convert_to_v5_for_legacy_checksum_repair(&mut bytes);

    // Q3 shard directory is section slot 4. Corrupt the reserved field of the
    // first active shard, then repair both the section checksum and file checksum.
    let entry = 128 + 4 * 32;
    let section_off = u64::from_le_bytes(bytes[entry + 8..entry + 16].try_into().unwrap()) as usize;
    let section_len =
        u64::from_le_bytes(bytes[entry + 16..entry + 24].try_into().unwrap()) as usize;
    let shard_dir = &bytes[section_off..section_off + section_len];
    let active = (0..256)
        .find(|shard| {
            let off = shard * 16;
            u32::from_le_bytes(shard_dir[off + 4..off + 8].try_into().unwrap()) != 0
        })
        .unwrap();
    bytes[section_off + active * 16 + 12..section_off + active * 16 + 16]
        .copy_from_slice(&1u32.to_le_bytes());
    let section_checksum = fnv1a(&bytes[section_off..section_off + section_len]);
    bytes[entry + 24..entry + 32].copy_from_slice(&section_checksum.to_le_bytes());
    let footer_off = u64::from_le_bytes(bytes[48..56].try_into().unwrap()) as usize;
    let file_checksum = fnv1a(&bytes[..footer_off]);
    bytes[footer_off + 24..footer_off + 32].copy_from_slice(&file_checksum.to_le_bytes());

    let malformed = temp_path("q3-malformed-valid-checksum");
    fs::write(&malformed, bytes).unwrap();
    assert!(VNextSegmentReader::open(&malformed).is_err());

    fs::remove_file(original).unwrap();
    fs::remove_file(malformed).unwrap();
}

#[test]
fn vnext_q3_index_matches_naive_block_oracle_for_every_present_gram() {
    use std::collections::{BTreeMap, BTreeSet};

    let path = temp_path("q3-naive-oracle");
    let block_size = 4usize;
    let docs = vec![
        VNextDocumentInput::new(1, "a.txt", b"abcdefghij".to_vec()),
        VNextDocumentInput::new(2, "b.txt", b"aaaaaa".to_vec()),
        VNextDocumentInput::new(3, "short.txt", b"xy".to_vec()),
        VNextDocumentInput::new(4, "empty.txt", Vec::new()),
        VNextDocumentInput::new(5, "jp.txt", "日本語検索".as_bytes().to_vec()),
    ];

    let mut expected = BTreeMap::<[u8; 3], BTreeSet<u16>>::new();
    let mut first_block = 0usize;
    for doc in &docs {
        if doc.normalized_content.len() >= 3 {
            for start in 0..=doc.normalized_content.len() - 3 {
                let gram = [
                    doc.normalized_content[start],
                    doc.normalized_content[start + 1],
                    doc.normalized_content[start + 2],
                ];
                let owner = u16::try_from(first_block + start / block_size).unwrap();
                expected.entry(gram).or_default().insert(owner);
            }
        }
        first_block += doc.normalized_content.len().div_ceil(block_size);
    }

    write_vnext_segment_with_block_size(&path, &docs, block_size as u32).unwrap();
    let reader = VNextSegmentReader::open(&path).unwrap();
    for (gram, blocks) in expected {
        assert_eq!(
            reader.q3_posting(gram).unwrap().iter().collect::<Vec<_>>(),
            blocks.into_iter().collect::<Vec<_>>(),
            "gram={gram:?}"
        );
    }
    for absent in [*b"zzz", *b"xyz", [0xff, 0xfe, 0xfd]] {
        assert!(
            reader.q3_posting(absent).unwrap().is_empty(),
            "gram={absent:?}"
        );
    }

    fs::remove_file(path).unwrap();
}

fn posting_mix_docs(abc_docs: usize, total_docs: usize) -> Vec<VNextDocumentInput> {
    (0..total_docs)
        .map(|index| {
            let content = if index < abc_docs { b"abc" } else { b"xyz" };
            VNextDocumentInput::new(index as u64, format!("doc-{index}.txt"), content.to_vec())
        })
        .collect()
}

#[test]
fn vnext_q3_gate3_uses_singleton_inline_without_posting_bytes() {
    let path = temp_path("q3-singleton");
    let docs = vec![
        VNextDocumentInput::new(1, "one.txt", b"abc".to_vec()),
        VNextDocumentInput::new(2, "two.txt", b"xyz".to_vec()),
    ];
    let report = write_vnext_segment_with_block_size(&path, &docs, 16).unwrap();
    let reader = VNextSegmentReader::open(&path).unwrap();
    let posting = reader.q3_posting(*b"abc").unwrap();

    assert_eq!(posting.encoding(), VNextQ3PostingEncoding::Singleton);
    assert_eq!(posting.encoded_bytes(), 0);
    assert_eq!(posting.iter().collect::<Vec<_>>(), vec![0]);
    assert!(posting.contains(0));
    assert!(!posting.contains(1));
    assert_eq!(report.q3_singleton_keys, 2);
    assert_eq!(report.q3_raw_u16_keys, 0);
    assert_eq!(report.q3_dense_bitmap_keys, 0);

    fs::remove_file(path).unwrap();
}

#[test]
fn vnext_q3_gate3_uses_raw_u16_when_not_larger_than_dense_bitmap() {
    let path = temp_path("q3-raw-threshold");
    // 64 blocks => dense bitmap is 8 bytes. Four u16 IDs are also 8 bytes,
    // so the tie deliberately stays RawU16 rather than paying bitmap scan cost.
    let docs = posting_mix_docs(4, 64);
    let report = write_vnext_segment_with_block_size(&path, &docs, 16).unwrap();
    let reader = VNextSegmentReader::open(&path).unwrap();
    let posting = reader.q3_posting(*b"abc").unwrap();

    assert_eq!(posting.encoding(), VNextQ3PostingEncoding::RawU16);
    assert_eq!(posting.encoded_bytes(), 8);
    assert_eq!(posting.iter().collect::<Vec<_>>(), vec![0, 1, 2, 3]);
    assert!(posting.contains(0));
    assert!(posting.contains(3));
    assert!(!posting.contains(4));
    assert!(report.q3_raw_u16_keys > 0);

    fs::remove_file(path).unwrap();
}

#[test]
fn vnext_q3_gate3_uses_dense_bitmap_only_when_it_is_smaller() {
    let path = temp_path("q3-dense");
    // 64 blocks => 8-byte bitmap versus 128-byte RawU16 posting.
    let docs = posting_mix_docs(64, 64);
    let report = write_vnext_segment_with_block_size(&path, &docs, 16).unwrap();
    let reader = VNextSegmentReader::open(&path).unwrap();
    let posting = reader.q3_posting(*b"abc").unwrap();

    assert_eq!(posting.encoding(), VNextQ3PostingEncoding::DenseBitmap);
    assert_eq!(posting.encoded_bytes(), 8);
    assert_eq!(posting.len(), 64);
    assert_eq!(posting.get(0), Some(0));
    assert_eq!(posting.get(63), Some(63));
    assert!(posting.contains(0));
    assert!(posting.contains(63));
    assert!(!posting.contains(64));
    assert_eq!(
        posting.iter().collect::<Vec<_>>(),
        (0u16..64).collect::<Vec<_>>()
    );
    assert!(report.q3_dense_bitmap_keys > 0);

    fs::remove_file(path).unwrap();
}

#[test]
fn vnext_q3_gate3_reader_rejects_unknown_encoding_with_repaired_checksums() {
    let original = temp_path("q3-bad-encoding-original");
    write_vnext_segment_with_block_size(&original, &posting_mix_docs(4, 64), 16).unwrap();
    let mut bytes = fs::read(&original).unwrap();
    convert_to_v5_for_legacy_checksum_repair(&mut bytes);

    // Section slot 4 = shard dir, slot 5 = q3 dictionary. Find the first active
    // shard and corrupt its first posting-meta encoding while repairing checksums.
    let shard_entry = 128 + 4 * 32;
    let shard_off =
        u64::from_le_bytes(bytes[shard_entry + 8..shard_entry + 16].try_into().unwrap()) as usize;
    let dict_entry = 128 + 5 * 32;
    let dict_section_off =
        u64::from_le_bytes(bytes[dict_entry + 8..dict_entry + 16].try_into().unwrap()) as usize;
    let dict_section_len =
        u64::from_le_bytes(bytes[dict_entry + 16..dict_entry + 24].try_into().unwrap()) as usize;
    let active = (0..256)
        .find(|shard| {
            let off = shard_off + shard * 16;
            u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()) != 0
        })
        .unwrap();
    let active_dir = shard_off + active * 16;
    let active_dict_rel =
        u32::from_le_bytes(bytes[active_dir..active_dir + 4].try_into().unwrap()) as usize;
    let first_meta = dict_section_off + active_dict_rel + 8192 + 257 * 4;
    bytes[first_meta] = 0xff;

    let section_checksum = fnv1a(&bytes[dict_section_off..dict_section_off + dict_section_len]);
    bytes[dict_entry + 24..dict_entry + 32].copy_from_slice(&section_checksum.to_le_bytes());
    let footer_off = u64::from_le_bytes(bytes[48..56].try_into().unwrap()) as usize;
    let file_checksum = fnv1a(&bytes[..footer_off]);
    bytes[footer_off + 24..footer_off + 32].copy_from_slice(&file_checksum.to_le_bytes());

    let malformed = temp_path("q3-bad-encoding-valid-checksum");
    fs::write(&malformed, bytes).unwrap();
    assert!(VNextSegmentReader::open(&malformed).is_err());

    fs::remove_file(original).unwrap();
    fs::remove_file(malformed).unwrap();
}

#[test]
fn vnext_q3_gate3_reader_rejects_dense_bitmap_cardinality_corruption() {
    let original = temp_path("q3-bitmap-corrupt-original");
    write_vnext_segment_with_block_size(&original, &posting_mix_docs(64, 64), 16).unwrap();
    let mut bytes = fs::read(&original).unwrap();
    convert_to_v5_for_legacy_checksum_repair(&mut bytes);

    // The corpus has one dense "abc" posting. Flip one bitmap bit, then repair
    // both integrity checks. Structural validation must still catch cardinality.
    let posting_entry = 128 + 6 * 32;
    let posting_off = u64::from_le_bytes(
        bytes[posting_entry + 8..posting_entry + 16]
            .try_into()
            .unwrap(),
    ) as usize;
    let posting_len = u64::from_le_bytes(
        bytes[posting_entry + 16..posting_entry + 24]
            .try_into()
            .unwrap(),
    ) as usize;
    assert!(posting_len >= 8);
    bytes[posting_off] &= !1u8;

    let posting_checksum = fnv1a(&bytes[posting_off..posting_off + posting_len]);
    bytes[posting_entry + 24..posting_entry + 32].copy_from_slice(&posting_checksum.to_le_bytes());
    let footer_off = u64::from_le_bytes(bytes[48..56].try_into().unwrap()) as usize;
    let file_checksum = fnv1a(&bytes[..footer_off]);
    bytes[footer_off + 24..footer_off + 32].copy_from_slice(&file_checksum.to_le_bytes());

    let malformed = temp_path("q3-bitmap-corrupt-valid-checksum");
    fs::write(&malformed, bytes).unwrap();
    assert!(VNextSegmentReader::open(&malformed).is_err());

    fs::remove_file(original).unwrap();
    fs::remove_file(malformed).unwrap();
}
