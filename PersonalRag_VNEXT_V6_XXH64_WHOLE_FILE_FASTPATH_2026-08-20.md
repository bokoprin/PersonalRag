# PersonalRag vNext v6 XXH64 Whole-File FastPath

Date: 2026-08-20

## 1. Summary

直前正本 `PersonalRag_GUI_PortableCore_VNextV5ChecksumRadixFastPath_2026-08-20.zip` をbaselineとして、v5後に最大のCPU bottleneckになっていた segment whole-file checksum を高速化した。

採用変更は1点だけ。

- 新規segment writerを `PRSEG2A6` / version 6へ更新。
- v6 whole-file 64-bit integrity checksumを従来の逐次FNV-1aから streaming XXH64(seed=0)へ変更。
- v4/v5 readerは従来どおりFNV whole-file checksumをstrictに検証する。
- v5+ content sectionのstandalone checksum=0という仕様は維持。
- content以外の13 sectionのstandalone FNV checksumは維持。
- q1/q2/q3 dictionary/postings/content/path/layoutには変更なし。

破損検出を省いた高速化ではなく、64-bit whole-file integrity checkを維持したまま、アルゴリズムを64-bit CPU向けの高速streaming checksumへ更新したformat v6である。

## 2. Bottleneck observation

v5 baseline、20,000 docs × 4,096 B、4 segments、tmpfs profileでは各segment概ね:

```text
index_group_ms   17.8 - 21.0 ms
checksum_ms       1.5 -  1.6 ms
write_stream_ms  32.5 - 35.9 ms
```

v5ではcontent standalone FNVは既に削除済みだったため、write_stream中のwhole-file FNVが最大のCPU bottleneckになっていた。

## 3. v6 design

### 3.1 Format

```text
v4: PRSEG2A4 / footer PR2FTR04 / whole-file FNV
v5: PRSEG2A5 / footer PR2FTR05 / whole-file FNV
v6: PRSEG2A6 / footer PR2FTR06 / whole-file XXH64(seed=0)
```

Header/footerの既存64-bit checksum fieldをそのまま使用し、versionによって検証algorithmを決定する。

### 3.2 Backward compatibility

Reader:

- v6 magic -> XXH64 whole-file verify
- v5 magic -> legacy FNV whole-file verify
- v4 magic -> legacy FNV whole-file verify + legacy content section FNV verify

v4/v5を新formatへ強制migrationしなくても既存generationをopenできる。

### 3.3 XXH64 implementation validation

Rust実装はone-shotとstreaming stateの双方を持つ。

Reference vectors:

```text
len=0      ef46db3751d8e999
"a"        d24ec4f1a98c6e5b
"abc"      44bc2cf5ad770999
0..255     1facbe8406cd904b
10k vector 5e9f4f7f2b4b2cfc
```

さらに同一inputを1, 3, 7, 31, 32, 33, 127, 1024 byte chunkでstreaming updateし、全てone-shot/reference digestと一致するunit testを追加した。

## 4. Performance

Benchmark common conditions:

```text
PR_VNEXT_BENCH_DOCS=20000
PR_VNEXT_BENCH_SEGMENT_DOCS=5000
CPU affinity=0-3
primary CPU A/B root=/dev/shm
release profile
alternating execution order
```

### 4.1 4KiB, 21-pair tmpfs A/B

```text
v5 median    106.081 ms
v6 median     57.694 ms
improvement   45.61 %
pairwise median improvement 43.69 %
v6 wins       21 / 21
```

### 4.2 Payload matrix, 11 pairs each

```text
512 B : 50.611 -> 42.525 ms   15.98 % faster   11/11 wins
1 KiB : 63.571 -> 50.183 ms   21.06 % faster   11/11 wins
2 KiB : 71.647 -> 43.849 ms   38.80 % faster   11/11 wins
4 KiB :109.274 -> 61.542 ms   43.68 % faster   11/11 wins
```

実行区間により絶対値は変動するため、採否は交互A/Bのmedian/win countを重視する。

### 4.3 Normal disk + sync_all(), 4KiB, 7 pairs

```text
v5 median    271.801 ms
v6 median    228.064 ms
improvement   16.09 %
pairwise median improvement 18.94 %
v6 wins        7 / 7
```

### 4.4 v6 internal profile

Representative segment samples:

```text
write_stream_ms  9.9 - 12.7 ms
```

v5 baselineは約32.5 - 35.9msだったため、write/checksum hot pathが大幅に縮小した。

## 5. Actual v5 byte compatibility oracle

同一 5,000 docs × 4KiB inputから、旧v5 writerと新v6 writerで各1 segmentを生成。

両segment size:

```text
21,869,176 bytes
```

新v6 segmentについて変更したのは次だけ:

1. `PRSEG2A6` -> `PRSEG2A5`
2. header version 6 -> 5
3. `PR2FTR06` -> `PR2FTR05`
4. footer version 6 -> 5
5. whole-file checksumをlegacy project FNVで再計算

その結果:

```text
actual v5 SHA-256:
48e10f923e00ae1d1e4f62784be8e580848c76002f9f8d1522c2c002be95810e

v6 converted-to-v5 SHA-256:
48e10f923e00ae1d1e4f62784be8e580848c76002f9f8d1522c2c002be95810e

BYTE_IDENTICAL=true
```

したがって検索用serialized sectionsは旧v5と完全同一であり、v6の差はformat identityとwhole-file checksum algorithmだけである。

`CURRENT` とgeneration manifestも同一inputではv5/v6でbyte identicalだった。

## 6. Correctness / regression

Final gate:

```text
cargo fmt -- --check                                  PASS
cargo clippy --offline --all-targets -- -D warnings  PASS
cargo test --offline                                  162 / 162 PASS
cargo build --offline --release                       PASS
pr_portable self-test                                 SELF_TEST_PASS
```

Added tests:

- `xxh64_matches_reference_vectors_and_streaming_boundaries`
- `reader_accepts_v5_segment_with_fnv_whole_file_checksum`

Existing corruption/fail-closed tests remain PASS, including published-fast segment corruption rejection.

## 7. Source changes

Only:

```text
search-core/src/vnext_segment.rs
search-core/tests/vnext_segment.rs
```

No q1/q2/q3 search semantics change.

## 8. Known environment limitation

`bridge-core` still cannot complete `cargo check --offline` in this isolated environment because the crates.io cache lacks the pre-existing `ignore` crate. This occurs before compiling project code and is unrelated to this wave.

## 9. Rejected/avoided directions

This wave intentionally did not reintroduce prior losing experiments such as:

- whole-file FNV buffer-flush hashing
- FNV manual unroll
- large BufWriter sizing changes
- segment preallocation
- q1/q2/q3 full-loop fusion

The next bottleneck after v6 moves back toward content q3 / q1q2 CPU and actual durable `sync_all()` latency.
