# PersonalRag vNext Content Checksum Fusion + Q3 Recent Cache Fast Path Report

**Date:** 2026-08-19  
**Scope:** `search-core` vNext durable segment full-build hot path  
**Baseline:** `PersonalRag_GUI_PortableCore_VNextQ12FusedFastPath_2026-08-19.zip`  
**Baseline SHA-256:** `14fbce3ed371931dc89ca5c248199e6f156f7bc6a3b7f92db5edd695b894bfc6`

## 1. Goal

前waveでq1/q2 hot pathを削った後に残ったbuild bottleneckを再計測し、serialized format・posting order・durability・検索意味論を変えずにさらに短縮する。

## 2. Pre-change gate

直前正本ZIPのSHA-256を確認して新規展開し、Rust 1.97.1環境で変更前回帰を実施した。

```text
cargo test --offline     153 / 153 PASS
```

4KiB / 20,000 docs / 4 segments のtmpfs profileでは、1 segmentあたり概ね次が支配的だった。

```text
q1+q2 fused      約 50-60 ms
content q3       約 30-61 ms
section checksum 約 26-34 ms
stream write     約 34-39 ms
```

section checksumでは約20MiB/segmentのcontent blobが支配的で、q1/q2 fused scanの後にcontent全体をもう一度FNV-1a走査していた。

## 3. Adopted optimization A: content checksum fusion

### Before

single-CPU / flat16 workloadでは、q1/q2 fused builderが全content byteを順番に走査した後、`compute_section_checksums()` がcontent blobを再度全走査してsection FNV-1aを計算していた。

### After

q1/q2 fused scanのbyte loopへ、既存formatと同じFNV-1a更新を融合した。

```text
content_checksum ^= byte
content_checksum *= FNV_PRIME
```

logical content blobのserialization順と同じ順序で更新するため、後段のcontent section checksumとしてその値をそのまま再利用できる。

sharded q2経路や2 CPU以上のparallel laneでは従来checksum計算へfallbackするため、既存経路は維持される。

### Component effect

同一4KiB workloadのtmpfs profile例:

```text
section checksum median
before  27.518 ms
after    1.597 ms
```

checksum融合単独のtmpfs交互A/B（7組）:

```text
baseline median   277.045 ms
candidate median  262.743 ms
improvement         5.16 %
pairs faster        6 / 7
```

## 4. Adopted optimization B: high-repetition q3 recent front cache

owner-local q3 dedupは、segment sampleで65%以上の重複が観測された高反復contentだけで有効になる。

### Before

すべてのq3 occurrenceがopen-addressed `LocalQ3Set`へ到達し、既出q3でもhash計算・table probeを行っていた。

### After

local dedupが有効なblockだけ、256-entry / 1KiBのdirect-mapped recent cacheを前段に置く。

```text
slot = key & 0xff
recent[slot] == key + 1 -> exact recent duplicate, skip hash table
otherwise                -> update recent slot, then existing LocalQ3Setへ
```

重要なのは、recent cacheは**positive hitだけを重複確定に使う**こと。異なるq3が同じslotへ衝突した場合は単なるcache missになり、既存のexact `LocalQ3Set`へfallbackする。そのためcache collisionで検索意味論やserialized outputが変わることはない。

### Component effect

checksum融合版に対するq3 recent cache追加のtmpfs交互A/B（7組）:

```text
checksum-only median  256.742 ms
+ q3 recent median    241.940 ms
incremental gain        5.77 %
pairs faster            6 / 7
```

q3 profile:

```text
content q3 emit median
before  43.291 ms
after   31.208 ms
```

## 5. Correctness tests

既存 `fused_q1_q2_flat_is_byte_identical_to_separate_builders` を拡張し、q1/q2 bytes/statsだけでなく、fused scan中に生成したcontent checksumがcontiguous content blobへ従来 `fnv1a()` を掛けた値と完全一致することを固定した。

追加test:

```text
recent_q3_front_cache_collisions_preserve_exact_bytes
```

同じlow-byteへ複数の異なるq3を意図的に衝突させ、recent cache使用版とglobal-only dedup版の以下が完全一致することを検証する。

- shard directory
- dictionary
- postings
- key/posting statistics
- encoding statistics

## 6. Balanced tmpfs A/B

共有実行環境のCPU/I/O driftを減らすため、baseline→candidate / candidate→baselineの実行順を交互に反転し、各payloadで9組測定した。

条件:

```text
docs          20,000
segment_docs   5,000
segments           4
```

結果:

| payload/doc | Q12 baseline median | final median | improvement | pair wins |
|---:|---:|---:|---:|---:|
| 1 KiB | 111.090 ms | 107.310 ms | 3.40% | 6/9 |
| 2 KiB | 156.411 ms | 142.795 ms | 8.71% | 8/9 |
| 4 KiB | 269.854 ms | 241.406 ms | 10.54% | 7/9 |

512Bはq2 sharded / checksum non-fused側が中心で、今回変更の主対象外。短い5組A/Bでは約2%差で測定ノイズ帯だった。

## 7. Normal-disk 4KiB A/B

`/mnt/data` 上で直前正本とfinalを交互に7組実行した。

```text
baseline median   340.337 ms
final median      288.585 ms
improvement        15.21 %
pairs faster        5 / 7
```

共有環境のI/O spikeがcandidate側2回にも発生したため、採否の主判定には上記balanced tmpfs A/Bを使用した。通常ディスクでも中央値では明確な改善を確認した。

## 8. Durable byte identity

同一20k x 4KiB入力からbaseline binaryとfinal binaryでdurable storeを生成し、相対パスごとに全通常ファイルをSHA-256比較した。

```text
DURABLE_BYTE_IDENTITY=PASS
files compared: 6
```

一致したSHA-256:

```text
CURRENT
0f8efc0100361ee96e4755f94397039f875bdffb0363cc10c31f2149b019b363

components/base-g0000000000000000/segment-00000.prseg2
0917d6164be3a3da8c2a26cfc372e68faa8d29035e89c79374ca189d3e2e6326

components/base-g0000000000000000/segment-00001.prseg2
3d831852b475b0616ecc6eb1020609aaf0cee4a522aaf31a3745b9b854dcb25a

components/base-g0000000000000000/segment-00002.prseg2
895d9ca33d92f76b0abd621f93647158c5500c648256c18123a0cf252c023f98

components/base-g0000000000000000/segment-00003.prseg2
3b0547fc3de87a29aaa232d0f72c518b0239de058051f44bae1f4134aa1c5ef4

generations/g0000000000000000-base.manifest
accc68ab9fa2d5aeda8414bd385ff17be982624b20208b176db32820b8de9f6b
```

したがって今回もformat version、serialized bytes、section checksum値、whole-file checksum、posting orderはすべて不変。

## 9. Final quality gate

```text
cargo fmt -- --check                                  PASS
cargo clippy --offline --all-targets -- -D warnings  PASS
cargo test --offline                                  154 / 154 PASS
cargo build --offline --release                       PASS
./target/release/pr_portable self-test                 SELF_TEST_PASS
```

`bridge-core cargo check --offline`は前waveと同じく、隔離環境のcrates.io cacheに`ignore` crateが存在しないため依存解決前に停止する。今回のsearch-core変更とは無関係。

## 10. Rejected experiments

### q3 touched-slot clear

既存hash table全clearを「実際に触ったslotだけclear」に置き換えた。

```text
q3 emit median  42.744 ms -> 55.804 ms
```

LLVM/libcによる連続`fill(0)`が十分高速で、touched vector管理が上回ったため破棄。

### small adaptive q3 hash table

高反復blockでは小さいtableから開始しunique増加時だけgrowする案。

```text
checksum-only median  258.067 ms
small-table median    322.288 ms
regression             24.89 %
```

小tableのprobe collision増加がclear削減を大幅に上回ったため破棄。

## 11. Remaining bottlenecks

今回content section checksum再走査とq3 duplicate probeの一部を削った結果、次の主要候補は以下。

1. whole-file FNV-1aを伴うstream write
2. q1/q2 fused encode / q3 emitの残りCPU
3. durable write / filesystem sync（実環境依存）

次waveでは、format互換性を壊さずwhole-file hash/writeの二重コストを減らせるかを優先評価する。
