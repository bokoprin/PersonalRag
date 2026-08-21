# PersonalRag vNext Periodic Q3 Reuse FastPath

**更新日:** 2026-08-20  
**対象:** `search-core` vNext durable segment build  
**目的:** `VNextQ1FromQ2FastPath` 後に残った content q3 owner-local dedup CPU bottleneck を、永続format・posting order・checksumを変更せず高速化する。

## 1. A/B基準

変更前oracle:

```text
PersonalRag_GUI_PortableCore_VNextQ1FromQ2FastPath_2026-08-19.zip
SHA-256:
4a2b457f46c30bd588253e1dff2cecc5649c9f88287471e08c4e7f7a3e9e6621
```

測定環境:

```text
Rust      1.97.1
CPU       Intel Xeon Platinum 8573C
visible   5 CPU
cgroup    4 CPU equivalent
CPU pin   taskset -c 0-3
main A/B  /dev/shm (tmpfs)
docs      20,000
segment   5,000 docs / segment, 4 segments
```

共有実行環境のCPU/I/O外れ値が大きいため、単発値ではなく交互A/B・中央値・pair勝敗を使用する。

## 2. ボトルネック

変更前の4KiB profileでは、owner-local dedupが有効なcontent q3に対して、segmentあたり約20.47M logical q3 startsを走査していた。

高反復contentではglobal radixへ残るunique/owner pairは約0.44M程度であり、大半の約20M q3 startsがowner-local dedupで捨てられていた。つまり「捨てることが分かっている短周期反復suffixを1 startずつhash判定すること」が主要なCPU wasteだった。

## 3. 採用変更

### 3.1 Exact periodic suffix skip

owner-local dedup中、recent q3 cacheのexact hitから短周期候補を得る。ただし候補だけではskipしない。

以下をすべて満たす場合だけfast pathを試す。

```text
owner block q3 occurrences >= 768
probe count < 8
period lag <= 256 bytes
remaining q3 starts >= 256
```

その後、残りsuffix全体をslice equalityで完全比較する。

```text
bytes[current .. q3_owner_end+2]
==
bytes[current-lag .. q3_owner_end+2-lag]
```

**残り全byteがexact一致した場合だけ**、残りq3 startsは既に現れたq3の周期反復であると証明できるため、一括skipする。

比較失敗時は従来のexact `LocalQ3Set` 経路へ戻る。近似判定・hash-only proof・false positiveを許す判定は使用していない。

### 3.2 LocalQ3Set generation-stamp reset

大きいowner-local hash tableでは、blockごとの全table `fill(0)` を廃止し、parallel `u16` generation stampを使用する。

- table entry >= 4096: generation stamp
- table entry < 4096: 従来のzero-sentinel + `fill(0)`
- generation wrap時だけstamp全体をclear

小tableは従来経路を維持し、generation indirectionの固定costを払わない。

### 3.3 q3 exact proofをq1/q2 fused buildへ再利用

q3 periodic proofが成立したowner blockでは、proof開始offsetを**build-only metadata**としてq1/q2 builderへ渡す。

このmetadataは`.prseg2`へserializeしない。

q1/q2 fused buildでは:

1. proof開始前までは従来どおりq2 owner stamp + q1 presence + content FNVを処理
2. proof開始後はcontent FNVだけ継続
3. document-final byteのq1補完は従来どおり実施

q3 proofはsuffix byte列そのものの周期一致を証明しているため、そのoffset以降のq2 startsも既出q2の反復であることが保証される。

q2側で周期を再検出しないので、独立probeのoverheadを追加しない。

### 3.4 Small-block adaptive protection

512B級の短いowner blockではperiodic probeの固定costを回収できないため、q3 occurrences < 768は従来recent-cache + exact set pathをそのまま使う。

また、q3 proofが1件も成立しない場合はq1/q2再利用用vectorを確保しない。

## 4. correctness tests

追加済みの主要oracle:

```text
periodic_suffix_skip_is_byte_equivalent_to_global_only_dedup
periodic_suffix_probe_fails_closed_on_late_mutation
local_q3_set_handles_zero_max_key_and_generation_wrap
q3_periodic_proof_reuse_preserves_q1_q2_bytes_and_content_checksum
```

既存のrecent-cache collision testやbounded-parallel byte-equivalence testも継続PASSしている。

near-periodic dataはsuffix末尾を1byte崩すとproof不成立になり、従来exact pathへfail-closedすることを確認している。

## 5. 最終性能

### 5.1 payload別 non-profile end-to-end

CPU 0-3固定、tmpfs、実行順を交互反転したA/B。

| payload / doc | 変更前 median | 変更後 median | 改善 | pair wins |
|---:|---:|---:|---:|---:|
| 512 B | 57.472 ms | 56.596 ms | 1.52% faster | 6 / 11 |
| 1 KiB | 76.432 ms | 75.539 ms | 1.17% faster | 4 / 9 |
| 2 KiB | 119.350 ms | 105.960 ms | 11.22% faster | 7 / 9 |
| 4 KiB | 164.454 ms | 157.835 ms | 4.02% faster | 15 / 21 |

4KiB 21-pairでは時間変動をpair内で相殺した改善率の中央値は **7.68% faster**、平均は **7.12% faster**。

512Bはadaptive gate導入前に約1.5%の中央値退行が見えたため保護対象にし、最終版では中央値が改善側へ戻った。

### 5.2 内部profile

`PR_PROFILE_BUILD=1 PR_PROFILE_Q3=1`、4KiB、7組A/B。

```text
end-to-end profile median
189.329 -> 167.450 ms
11.56 % faster

content q3 emit median (28 segment samples)
29.7315 -> 7.854 ms
73.58 % faster
```

profile instrumentation自体にcostがあるため、このend-to-end値はnon-profile benchmark値と混同しない。ここではhot phaseの変化確認を主目的とする。

候補版では代表segmentごとに約20.03M q3 occurrencesがexact periodic proofによりskipされている。

## 6. Durable byte identity

直前正本と最終候補で同一20k × 4KiB generationを生成し、全6 durable filesをSHA-256比較した。

```text
DURABLE_BYTE_IDENTITY=PASS
```

一致したSHA-256:

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

したがって今回変更によるdurable format、posting order、section checksum、whole-file checksumの差はない。

## 7. 最終品質ゲート

```text
cargo fmt -- --check                                  PASS
cargo clippy --offline --all-targets -- -D warnings  PASS
cargo test --offline                                  158 / 158 PASS
cargo build --offline --release                       PASS
pr_portable self-test                                 SELF_TEST_PASS
```

`bridge-core`は隔離環境のcrates.io cacheに`ignore` crateが存在しないため、従来どおり`cargo check --offline`が依存解決前に停止する。search-core変更とは無関係。

## 8. 今回試して不採用にした案

### q3 rolling key

隣接q3の2byte共有を使ってkey生成loadを減らす案。q3 emit単体は小幅に改善したが、end-to-endでは改善が安定せず不採用。

### q2独自periodic detection

q3とは別にq1/q2側で周期を再検出する案。FNV checksum処理は残る一方、probe/cache管理が増え、q1/q2 hot loopとend-to-endが悪化したため破棄。

### 以前の不採用案を再導入しない

今回も以下は再導入していない。

- q3 24-bit巨大bitmap
- q3 owner-stamp巨大table
- touched-slot clear vector
- q1/q2/q3全面1-pass融合
- writer pre-size
- BufWriter容量変更
- whole-file FNV flush-hash

これらは過去A/Bで不利だった結果を既存高速化レポートに保持する。

## 9. 変更ファイル

```text
search-core/src/vnext_q3.rs
search-core/src/vnext_fixed.rs
search-core/src/vnext_segment.rs
```

## 10. 次のボトルネック

q3 CPUは大きく減ったため、今後の4KiB full buildでは相対的に以下の比重が上がる。

1. q1/q2 flat fusedの残存CPU
2. q3 radix/dedup/encodeの残存部分
3. durable write / `sync_all()`

特に通常ディスクでは`sync_all()`の揺らぎが大きく、CPU最適化がend-to-end測定で隠れやすい。次waveでもtmpfs hot-phaseと通常ディスクdurabilityの両方を分けて観測する。

Windows production acceptanceはLinux作業環境では完結できないため、最終切替時にはWindows上で既存のproduction-switch validation scriptを実行する。
