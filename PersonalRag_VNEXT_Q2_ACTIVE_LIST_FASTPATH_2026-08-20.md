# PersonalRag vNext Q2 Active-List FastPath

Date: 2026-08-20

## 1. Summary

直前正本 `PersonalRag_GUI_PortableCore_VNextV6Xxh64FastPath_2026-08-20.zip` をbaselineとして、high-periodic contentで残っていたq2 radix scatterの固定コストを削減した。

採用変更は1点。

- flat16 q2かつq3 periodic proof coverageが75%以上のsegmentだけ、packed pair + 65,536-bucket radix経路の代わりにq2 keyごとのowner active-listへ直接appendする。
- active q2 keyだけをsortし、owner listをそのまま既存sparse q2 wire formatへencodeする。
- active owner listは実測した高反復workloadに合わせ初回2,048件をreserveする。
- gate非対象では従来radix関数を文字どおりそのまま呼ぶ。fallback hot loopへ追加分岐を入れない。
- segment formatはv6のまま。検索意味論、q1/q2 serialized bytes、whole-file XXH64、section layoutには変更なし。

## 2. Bottleneck

周期q3 proof適用後の4KiB workloadではq2のactive key数が少ない一方、従来経路はowner-local unique q2 pair全体をpacked `Vec<u32>`へ積み、65,536 bucket histogram/prefix/scatterを実行していた。

active-list経路では既にowner順が単調である性質を使い、各q2 keyのowner listを生成時点で完成させることで全pair radix copyを省く。

## 3. Adaptive gate

`periodic_q2_skip_from` の先頭 `block_count` ownerについて、exact periodic proofを持つownerが75%以上の場合のみactive-listを使う。

```text
proven_owners * 4 >= block_count * 3
```

以下では従来radix経路を維持する。

- sharded q2 workload
- proof coverage < 75%
- proof vectorがblock_countより短い
- empty segment

## 4. Correctness oracle

追加unit test:

- `periodic_q2_active_lists_are_byte_identical_to_radix_oracle`
  - 複数pattern、複数document、複数8KiB blockのhigh-periodic入力を生成。
  - q3 exact periodic proofを実際に構築。
  - active-list版と従来radix版のq1 bytes/stats、q2 bytes/statsを完全比較。
- `periodic_q2_active_list_gate_requires_three_quarters_exact_proof`
  - 75%境界、75%未満、短いproof vector、empty universeを固定。

既存 `q3_periodic_proof_reuse_preserves_q1_q2_bytes` もadaptive wrapper経由でPASS。

## 5. Performance

Common CPU profile conditions:

```text
docs=20,000
payload=4,096 bytes
segment docs=5,000
segments=4
CPU affinity=0-3
root=/dev/shm
release
PR_PROFILE_BUILD=1
7 runs each
```

### 5.1 q1/q2 hot path

```text
v6 baseline cq12_fused median : 7.4275 ms
active-list median             : 6.3970 ms
reduction                       : 13.87%
```

### 5.2 tmpfs end-to-end

```text
v6 baseline median : 72.229 ms
active-list median : 68.618 ms
reduction           : 5.00%
```

### 5.3 Normal disk + sync_all(), alternating 21 pairs

```text
v6 baseline median       148.061 ms
active-list median       144.846 ms
median reduction           2.17 %
pairwise median reduction  3.56 %
active-list wins          12 / 21
```

通常ディスクは`sync_all()`のvarianceが大きいため、CPU hot-path改善はtmpfs/internal profileを主判定にし、通常ディスクA/Bは退行確認として使用した。追加21組では改善側。

## 6. Durable byte identity

同一20,000 docs × 4KiB inputからbaseline/candidateでdurable generationを作り、以下6ファイルをSHA-256比較した。

- `CURRENT`
- `segment-00000.prseg2`
- `segment-00001.prseg2`
- `segment-00002.prseg2`
- `segment-00003.prseg2`
- `g0000000000000000-base.manifest`

```text
DURABLE_BYTE_IDENTITY=PASS
```

したがってv6 format、q1/q2 postings、q3、content、path、XXH64 footer、manifestは1bitも変更していない。

## 7. Final gate

```text
cargo fmt -- --check                                  PASS
cargo clippy --offline --all-targets -- -D warnings  PASS
cargo test --offline                                  164 / 164 PASS
cargo build --offline --release                       PASS
pr_portable self-test                                 SELF_TEST_PASS
```

## 8. Source changes

実装変更:

```text
search-core/src/vnext_fixed.rs
```

新規oracle testも同ファイル内のunit test moduleに追加した。

## 9. Rejected variants during development

- q3 hot loopでq1/q2も副産物生成: cache locality悪化で不採用。
- active-list末尾を`last_owner`代用: 改善なし。
- q3 sampling 16→4: 安定改善せず異種contentの判定余裕も減るため不採用。
- fallback hot loop内にactive-list分岐を混在: 512B退行を検出したため、入口で完全分離する現在設計へ修正。

## 10. Acceptance

このwaveの採用条件は、oracle byte identity、164/164 regression、fmt/clippy、durable全ファイルSHA identity、tmpfs hot-path改善、normal-disk非退行、release/self-testの全てを満たすこと。
