# PersonalRag vNext Q1-from-Q2 Fast Path Report

**Date:** 2026-08-19  
**Scope:** `search-core` vNext durable full-build, single-CPU/segment flat16 q1/q2 fused path  
**Baseline:** `PersonalRag_GUI_PortableCore_VNextChecksumQ3CacheFastPath_2026-08-19.zip`  
**Baseline SHA-256:** `6ea25057b18ad260b3239e62af9354182d44a89b4dd292e77a7e39d8ca47b10a`

## 1. Goal

checksum再走査と高反復q3 probeを前waveで削った後、残っているbuild CPU bottleneckを再計測し、format・posting order・checksum・durabilityを変えずに短縮する。

変更前回帰はRust 1.97.1環境で **154/154 PASS**。

## 2. Bottleneck

4KiB / 20,000 docs / 4 segmentsでは、各segmentのCPU budgetは1となり、large q2はflat16、q1/q2はfused scanを使用する。

このfused loopは各content byteについて次を実施していた。

1. content FNV-1a更新
2. q1 block-local 256-bit presence set更新
3. q2 key生成
4. owner-local q2重複判定
5. 初出q2 pair追加

q2が高反復なblockでもq1 bitset更新だけは全byteで必ず発生していた。

## 3. Adopted optimization: derive q1 presence from unique q2

同じowner blockにおいて、document最終byte以外の各byteは必ず「その位置から始まるq2」のhigh byteになる。

したがってq1 presenceは次で完全に復元できる。

- owner-localで初めて出たq2を追加するとき、そのq2のhigh byteをq1 presenceへ追加する
- documentの最終byteだけはq2 startを持たないため、最終blockで1回だけq1 presenceへ追加する

同じq2がowner内で繰り返す場合、そのhigh byteは初回q2で既にq1へ追加済みなので、重複q2でq1を再更新する必要はない。

これにより高反復contentではq1 bitsetのper-byte read/modify/writeを大幅に削減する。

sharded q2経路（小規模content）とq1/q2を別laneで作るmulti-CPU/segment経路は変更していない。

## 4. Correctness

既存oracle:

```text
fused_q1_q2_flat_is_byte_identical_to_separate_builders
```

でfused q1/q2 bytes・stats・content checksumが個別builderと完全一致することを再確認した。

追加test:

```text
q1_derived_from_unique_q2_preserves_final_and_boundary_bytes
```

以下を明示的に含む。

- empty document
- 1-byte document
- repeated q2
- block boundary
- document final byte
- `0x00`, `0xff` を含むbinary content

q1/q2 serialized bytes・stats・content checksumが従来oracleと完全一致する。

## 5. Hot-loop A/B

条件:

```text
docs          20,000
payload/doc     4 KiB
segment_docs    5,000
segments            4
output          tmpfs
CPU affinity    0-3
runs            7 alternating pairs
```

`PR_PROFILE_BUILD=1` の全segment `cq12_fused_ms` を集計した。

```text
baseline q1/q2 fused median   77.547 ms
candidate median              54.362 ms
improvement                    29.90 %
```

end-to-end:

```text
baseline median   276.152 ms
candidate median  235.632 ms
improvement         14.67 %
pair wins             7 / 7
```

## 6. Payload check

```text
1 KiB  109.715 -> 109.274 ms   0.40 % faster, 4/7 wins
2 KiB  148.160 -> 144.688 ms   2.34 % faster, 5/7 wins
4 KiB  276.152 -> 235.632 ms  14.67 % faster, 7/7 wins
```

1KiBはq2 sharded経路で今回のq1-from-q2 fused変更を通らないため、差はnoise帯として扱う。

## 7. Normal-disk A/B

4KiB / 20k docsを `/mnt/data` で7組交互実行。

```text
baseline median   335.251 ms
candidate median  324.803 ms
improvement          3.12 %
pair wins             6 / 7
```

filesystem syncの揺らぎを含んでも中央値改善を確認した。採否の主判定はhot-loop profileとtmpfs 7/7勝ちを使用する。

## 8. Durable byte identity

同一20k x 4KiB入力からbaseline binaryとcandidate binaryでdurable storeを生成し、全6ファイルをSHA-256比較した。

```text
DURABLE_BYTE_IDENTITY=PASS
```

一致SHA-256:

```text
CURRENT
0f8efc0100361ee96e4755f94397039f875bdffb0363cc10c31f2149b019b363

segment-00000.prseg2
0917d6164be3a3da8c2a26cfc372e68faa8d29035e89c79374ca189d3e2e6326

segment-00001.prseg2
3d831852b475b0616ecc6eb1020609aaf0cee4a522aaf31a3745b9b854dcb25a

segment-00002.prseg2
895d9ca33d92f76b0abd621f93647158c5500c648256c18123a0cf252c023f98

segment-00003.prseg2
3b0547fc3de87a29aaa232d0f72c518b0239de058051f44bae1f4134aa1c5ef4

g0000000000000000-base.manifest
accc68ab9fa2d5aeda8414bd385ff17be982624b20208b176db32820b8de9f6b
```

format version、section checksums、whole-file checksum、posting order、manifestは不変。

## 9. Final quality gate

```text
cargo fmt -- --check                                  PASS
cargo clippy --offline --all-targets -- -D warnings  PASS
cargo test --offline                                  155 / 155 PASS
cargo build --offline --release                       PASS
./target/release/pr_portable self-test                SELF_TEST_PASS
```

`bridge-core cargo check --offline` は既知の環境制約により `ignore` crateがoffline cacheに無く、依存解決前で停止する。今回のsearch-core変更とは無関係。

## 10. Rejected experiments in this wave

### File pre-sizing

`File::set_len(file_size)` をstream前に実行。

```text
305.731 -> 315.429 ms
3.17 % regression
```

不採用。

### BufWriter 4 MiB

```text
316.587 -> 339.994 ms
7.39 % regression
```

4 segment同時write時のcache/memory pressureが増えたため不採用。

### BufWriter 256 KiB

```text
708.472 -> 708.567 ms
no measurable gain
```

不採用。

### q1/q2/q3/checksum one-pass fusion

q1/q2/q3 bytesはoracleと完全一致したが、end-to-end 9組A/Bでは:

```text
254.468 -> 258.815 ms
1.71 % regression
```

1 loopの命令密度が高くなり、別tight loopよりCPU実行効率が落ちたため破棄。

### whole-file FNV at BufWriter flush

一時的なend-to-end測定では改善に見える区間があったが、writerだけを切り出した1 CPU microbenchmarkを31回ずつ行うと一貫して遅かった。

```text
payload  old median  new median  result
1 KiB      7.112 ms    7.254 ms   2.00 % slower
2 KiB     17.157 ms   18.230 ms   6.26 % slower
3.5 KiB   26.172 ms   27.618 ms   5.53 % slower
4 KiB     32.716 ms   34.334 ms   4.95 % slower
```

end-to-endの見かけ上の改善は共有環境ノイズと判断し、関連変更はすべて破棄した。

## 11. Remaining bottlenecks

q1/q2 fused CPUを約30%削った後の主要候補は次。

1. content q3 emit / owner-local dedupの残存CPU
2. whole-file FNV-1aを含むstream write
3. `sync_all()` / durable filesystem latency
4. q1/q2 fused内のcontent checksum + q2 lookupそのもの

次waveでは、既に負けたq3 bitmap/stamp/table-size案を繰り返さず、q3 key生成/probe局所性またはdurabilityを保ったwrite/sync構造を優先評価する。
