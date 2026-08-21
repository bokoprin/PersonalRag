# PersonalRag Segment Format vNext - Gate 2 Report

Date: 2026-08-17
Parent source: `PersonalRag_GUI_PortableCore_SegmentVNext_Gate1_2026-08-17.zip`
Parent SHA-256: `8acab1a7486ac0c0bf801f041162883e8f0735e604166bd91ea24940eca66ac2`
Environment: ChatGPT Linux x86_64 / Rust 1.97.1

## Status

Gate 2 complete for the isolated `.prseg2` prototype. Perf12 remains unchanged as production/correctness oracle.

Implemented:

- 8 KiB default content blocks
- q3 ownership by gram start position
- up to 2-byte look-ahead across a block boundary inside the same document
- no q3 generation across document boundaries
- first-byte 256-way q3 shards
- packed occurrence `(BC:u16, block_id:u16) -> u32`
- two-pass 16-bit radix sort
- per-block q3 deduplication
- sparse active-shard dictionary
- 65,536-bit suffix presence bitmap per active shard
- 257-entry rank directory per active shard
- RawU16 block-ID postings
- mmap lookup without allocating a posting Vec
- zero-hit rejection from the presence bitmap
- structural fail-closed validation for dictionary/rank/posting metadata

## `.prseg2` prototype format version 2

Gate 2 advances the prototype magic/version to:

```text
magic   = PRSEG2A2
version = 2
```

Sections:

```text
1 Document SoA
2 Path Blob
3 Block Table
4 Normalized Content Blob
5 Q3 Shard Directory
6 Q3 Dictionary
7 Q3 RawU16 Postings
```

### Q3 shard directory

Fixed 256 entries, 16 bytes per first-byte shard:

```text
dict_offset : u32
dict_length : u32
key_count   : u32
reserved    : u32
```

Only active shards allocate dictionary payload.

### Active shard dictionary

```text
presence bitmap : 8192 bytes
rank directory  : 257 * u32
PostingMeta[]   : { posting_byte_offset:u32, posting_len:u32 }
```

The presence bitmap provides the zero-hit fast path. The rank directory narrows rank work to one 256-key bucket.

### Posting payload

```text
block_id : little-endian u16
```

IDs are strictly increasing and deduplicated within each q3 posting.

## Boundary correctness

For a block size of 8 and content:

```text
abcdefghijk
```

expected ownership is verified as:

```text
ghi -> block 0
hij -> block 0   # starts at final byte of block 0 and looks two bytes ahead
ijk -> block 1
```

The implementation scans each document independently, so a q3 never crosses from the end of one document into the start of the next document.

## Correctness tests

Gate 2 dedicated tests: 12/12 PASS.

Added coverage includes:

- q3 block-boundary ownership and look-ahead
- per-block posting deduplication
- no document-boundary false gram
- active/inactive shard zero-hit
- RawU16 sorted postings
- every present q3 compared with a naive block oracle
- Japanese UTF-8 byte trigrams in the naive-oracle corpus
- q3 directory corruption rejected even after repairing section/file checksums
- Gate 1 roundtrip/deterministic/truncation/LE tests retained

## Full regression after Gate 2

```text
Search Core unit tests      5/5 PASS
Production oracle tests    35/35 PASS
vNext tests                12/12 PASS
Doc tests                         PASS
cargo fmt --check                 PASS
Clippy -D warnings                PASS
release pr_portable               PASS
SELF_TEST_PASS                    PASS
```

Production Perf12 tests remain unchanged and pass in full.

## Gate 2 build benchmark

Corpus is the same synthetic text-heavy 20k generator used by `unified_full_bench`.

### Perf12 Unified Full, three repeated runs

```text
969.901 ms
972.541 ms
945.144 ms
median = 969.901 ms
mean   = 962.529 ms
index  = 76,733,278 bytes
```

Measured peak RSS in the same environment:

```text
295,552 KiB
```

### Gate 2 vNext q3 prototype, three repeated runs

```text
636.159 ms
611.558 ms
611.828 ms
median = 611.828 ms
mean   = 619.848 ms
file   = 53,876,384 bytes
```

Prototype content statistics:

```text
source_bytes    = 24,054,644
blocks          = 20,000
q3_keys         = 34,801
q3_posting_ids  = 14,033,692
active_shards   = 49
```

Measured peak RSS:

```text
184,128 KiB
```

### Radix optimization A/B inside Gate 2

Before replacing comparison sort:

```text
elapsed_ms = 867.716
peak RSS   = 174,840 KiB
```

After 2-pass radix sort:

```text
elapsed_ms = 649.382   # first measured radix run
peak RSS   = 184,128 KiB
```

The repeated radix median settled at 611.828 ms.

### Observed comparison

Using repeated median build times:

```text
Perf12 / Gate2 = 1.585x
```

Gate 2 file bytes are about 29.8% below the current Perf12 full index and measured peak RSS is about 37.7% below the Perf12 run.

Important: this is NOT a production-switch result. Gate 2 does not yet implement the full q1/q2, filename/path, Query Planner, exact query path, delta or posting-specialization feature set. The numbers are useful as the first Gate 2 structural benchmark only.

## Review findings and repairs

1. Initial comparison-sort q3 build had insufficient margin relative to Perf12.
2. Packed q3 occurrences were changed to two-pass 16-bit radix sort.
3. All q3 bytes/statistics remained identical while build time improved substantially.
4. Corrupt-file offset arithmetic was hardened with checked addition.
5. Full regression and Clippy were rerun after the repairs.

## Gate 3 candidate

Gate 2 intentionally stores every posting as RawU16.

The next gate may specialize postings only where measurement justifies it:

```text
Singleton
RawU16
DenseBitmap
```

For example, the 20k corpus has 19,674 candidate blocks for trigram `tim`, so dense/common postings are an obvious Gate 3 measurement target. Do not add more complex compression before A/B evidence.
