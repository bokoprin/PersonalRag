# PersonalRag vNext Q2 Owner-Stamp + Q1/Q2 Fused Fast Path Report

**Date:** 2026-08-19  
**Scope:** `search-core` vNext durable segment full-build hot path  
**Baseline:** `PersonalRag_GUI_PortableCore_VNextQ1BitsetFastPath_2026-08-19.zip`  
**Baseline SHA-256:** `34113689a6fb8f112ab674940efdff7d6756075616a5b033ec94cd0a97c3c8e8`

## 1. Goal

前waveで高速化したcontent q1の次に残るbuild bottleneckを実測し、serialized format・検索意味論・durabilityを変えずにさらに短縮する。

## 2. Pre-change gate

Q1 Bitset baselineを新規展開し、Rust 1.97.1環境で変更前回帰を実施した。

```text
cargo fmt -- --check                         PASS
cargo clippy --offline --all-targets -- -D warnings  PASS
cargo test --offline                         152 / 152 PASS
cargo build --offline --release              PASS
```

代表4KiB profileでは1 segmentあたり概ね以下が支配的だった。

```text
content q1      約 45-55 ms
content q2      約 24-45 ms（flat16）
content q3      約 30-60 ms（負荷変動あり）
checksum        約 25-29 ms
write/fsync     I/O環境依存
```

## 3. Adopted optimization A: flat16 q2 owner stamp

### Before

flat16 q2はowner blockごとに8KiB membership bitmapを使い、各q2でword/mask判定し、block終了時にtouched wordをclearしていた。

### After

segment内block IDは一意で `u16::MAX` 未満であることを利用し、65,536要素の`u16`表へ「そのq2 keyを最後に出力したowner」を保存する。

```text
last_owner[q2] != owner -> emit + last_owner[q2] = owner
last_owner[q2] == owner -> duplicate, skip
```

これにより以下をhot loopから除去した。

- `key / 64`, `key % 64` 相当のbit addressing
- bitmap read-modify-write
- touched-word list管理
- blockごとのclear

ownerは単調増加するため、生成する `(q2 key, owner)` の集合と順序は従来と同一。

このfast pathは既存adaptive gateによりlarge flat16 workloadだけで使用される。6M logical occurrences以下のsharded8x8経路は変更しない。

## 4. Adopted optimization B: single-CPU flat16 q1/q2 fused scan

4 segmentを4 effective CPUsで同時buildする現在の代表構成では、各segment writerは`cpu_budget=1`となる。
従来は同じcontentをq2用に1回、q1用にもう1回走査していた。

large flat16かつsingle-CPU segmentに限り、1回のblock scanで以下を同時に生成する。

- q1: 256-bit block-local presence set
- q2: owner-stamp dedup + packed pair stream

2 CPU以上のsegment writerでは既存q1/q2/q3並列laneを維持する。
small/sharded q2 workloadではfused pathを使わず従来経路へ戻る。

## 5. Correctness tests added

新規unit test:

```text
fused_q1_q2_flat_is_byte_identical_to_separate_builders
```

randomized binary content、複数document、block boundaryを含む入力で、fused q1/q2の`bytes`と`stats`がseparate buildersと完全一致することを検証する。

既存q1 byte identity、q2 sharded/flat oracle、production oracle群も全PASS。

## 6. Component-level measurements

同一4KiB workloadのprofile例ではflat q2が次のように短縮した。

```text
slow segment: cq2 45.0 ms -> 24.4 ms
fast segment: cq2 23.8 ms -> 14.8 ms
```

q2 stamp単独に対しq1/q2 fused scanを追加したtmpfs交互A/B:

```text
q2 stamp only median    286.676 ms
q1/q2 fused median      276.417 ms
incremental improvement   3.58 %
```

## 7. End-to-end normal-disk A/B

条件:

```text
docs          20,000
payload       4,096 bytes/doc
segment_docs  5,000
segments      4
serialized    87,495,475 bytes
```

直前Q1 Bitset baselineと最終候補を交互に各6回実行。

```text
Baseline ms: 398.470, 352.939, 375.473, 348.562, 395.001, 364.923
Final ms:    346.502, 332.683, 349.612, 333.092, 320.743, 316.453

Baseline median: 370.198 ms
Final median:    332.888 ms
Improvement:      10.08 %
```

6/6のpairで最終候補がbaselineより高速だった。

## 8. Durable byte identity

同一20k x 4KiB入力からbaseline binaryと最終binaryでdurable storeを生成し、全通常ファイルを相対パス単位でSHA-256比較した。

```text
DURABLE_BYTE_IDENTITY=PASS
files compared: 6
```

一致した対象:

- `CURRENT`
- 4 x `.prseg2`
- base generation manifest

したがって今回の高速化によるformat version変更、serialized-byte変更、posting-order変更はない。

## 9. Final quality gate

```text
cargo fmt -- --check                         PASS
cargo clippy --offline --all-targets -- -D warnings  PASS
cargo test --offline                         153 / 153 PASS
cargo build --offline --release              PASS
./target/release/pr_portable self-test        SELF_TEST_PASS
```

`bridge-core cargo check --offline`は既知の隔離環境制約で`ignore` crateがcacheに存在せず依存解決前に停止する。search-core変更とは無関係。

## 10. Rejected experiments

### q3 24-bit dense bitmap local dedup

q3 emit単体では改善するケースがあったが、4-segment end-to-end tmpfs中央値は約1%悪化したため破棄。

### CPU remainder distribution

visible CPUは5だがcgroup quotaは4 CPU equivalentで、Rust `available_parallelism()`は4。現在の4 segment workersに対して挙動が変わらないため破棄。

### q3 owner-stamp shards

q2-onlyから中央値約1.68%改善した一方、平均値はほぼ同等で、worst-case 32MiB/workerの追加表を持ち得る。費用対効果が弱いため今回は採用しない。

## 11. Remaining bottlenecks

今回q2と重複content scanを削った結果、次の主要候補は以下。

1. content q3 owner-local emit/dedup
2. section + whole-file FNV checksum（二重走査/二重hash）
3. durable write / sync

次waveでは、q3の追加メモリを厳密にboundしたfast path、またはchecksum format互換性を維持したままの計算削減を優先評価する。
