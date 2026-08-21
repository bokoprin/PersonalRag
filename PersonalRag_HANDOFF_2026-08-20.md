# PersonalRag 開発引き継ぎ 2026-08-20

## 0. この文書の目的

このZIPを新しいChatGPTチャットへ渡したとき、**現在のPersonalRag高速化開発を同じ状態から再開するための正本引き継ぎ資料**である。

新規チャットでは、まずこの文書と次の2資料を読むこと。

- `PersonalRag_DEV_ENV_BOOTSTRAP_2026-08-19.md`
  - Rust 1.97.1環境の再構築方法
  - GitHub Actions artifactを使ったbootstrap
  - 回帰・release・benchmarkの環境完成ゲート
- `PersonalRag_VNEXT_Q2_ACTIVE_LIST_FASTPATH_2026-08-20.md`
  - 直近waveの実装内容、性能値、oracle、durable byte identity

このZIP内のソースコードが現時点の正本である。

---

# 1. 現在の正本

## 1.1 build tag

```text
PersonalRag_GUI_PortableCore_VNextQ2ActiveListFastPath_2026-08-20
```

このhandoff追加前の配布ZIPのSHA-256:

```text
01c72d9e829f7a7abd4549fc7e821024a241d186eb23ee96a153a8f634ede879
```

このhandoff文書を追加した最終引き継ぎZIPは別のSHA-256になるため、**新しいZIP自身のSHA-256はチャット側で別途提示する値を正とする**。

## 1.2 直前baseline/oracle

```text
PersonalRag_GUI_PortableCore_VNextV6Xxh64FastPath_2026-08-20.zip
SHA-256:
0fdb1a37bb1e3d980d206c9e9ed75a5a108bb1473360a2e3ab5380c358ef7c2d
```

Q2 active-list waveのdurable outputは、このv6 baselineと全6ファイルbyte identicalである。

---

# 2. PersonalRagの現在の位置づけ

PersonalRagはまずWindows上で高速なファイル名・パス検索とテキスト内容検索を実装し、その後LLMを組み込み自然文検索へ拡張するプロジェクト。

現在の高速化対象は主に `search-core` のvNext persistent segment build/query基盤。

重要な方針:

- deterministic/correctnessを性能より優先する。
- 変更前に必ず回帰を通す。
- 実装後は専用test、新規test、全回帰、fmt/clippy、release/self-testを通す。
- 性能採否は単発値ではなくA/B中央値を使う。
- 可能な限りserialized bytes、durable generation、reader互換性をoracleで固定する。
- 負けた高速化案は残さない。
- ソース変更後は日本語1行のコミットコメント案を提示する。
- ユーザー明示なしにgit commitしない。
- Windowsの最終production-switch validationはLinuxコンテナでは代替できないため最後にWindowsで実行する。

---

# 3. 現在までの高速化履歴

以下は大きなwaveだけを時系列でまとめたもの。各詳細はソースルートの `PersonalRag_*.md` を参照。

## 3.1 vNext segment/build基盤

Perf12をproduction/oracleとして残しつつ、PRSEG2系の新segment writer/readerを作成。

基本構成:

- bounded local-ID segment
- 8KiB block
- u16 local IDs
- block-level q1/q2/q3 inverted index
- mmap/exact verification
- durable generation store
- generation manifest / CURRENT
- load/compaction/GC

---

## 3.2 Q1 block bitset fast path

q1 presence生成で全byteごとのowner計算/stampを避け、block単位256-bit bitset化。

代表4KiBで約15%前後改善。

詳細:

```text
PersonalRag_VNEXT_Q1_BLOCK_BITSET_FASTPATH_2026-08-19.md
```

---

## 3.3 Q2 owner-stamp + Q1/Q2 fused scan

flat16 q2で8KiB bitmap clearをやめ、u16 owner-stamp表へ変更。

さらにsingle-CPU/flat16でq1/q2を同一block scanへ融合。

代表4KiBで直前版比約10%改善。

詳細:

```text
PersonalRag_VNEXT_Q2_OWNER_STAMP_Q12_FUSED_FASTPATH_2026-08-19.md
```

---

## 3.4 content checksum fusion + q3 recent cache

q1/q2 scan中にcontent section checksumを同時計算し、後段のcontent再走査を削除。

高重複q3には256-entry recent cacheを既存exact hash setの前段へ追加。

代表4KiBでさらに約10%前後改善。

詳細:

```text
PersonalRag_VNEXT_CHECKSUM_FUSION_Q3_RECENT_CACHE_FASTPATH_2026-08-19.md
```

---

## 3.5 Q1 from Q2

q1 presenceを全byteから作るのではなく、owner-local unique q2の先頭byteから派生し、document末尾byteだけ補完。

`cq12_fused` hot loopが約30%短縮、end-to-endも大幅改善。

詳細:

```text
PersonalRag_VNEXT_Q1_FROM_Q2_FASTPATH_2026-08-19.md
```

---

## 3.6 Periodic q3 exact skip + generation stamp + q1/q2 proof reuse

高反復blockでrecent q3から周期候補を見つけ、**残りsuffix全体をbyte exact比較して周期を証明した場合だけ**q3走査を一括skip。

さらに:

- LocalQ3Set resetを大block時generation-stamp化
- q3が証明したperiodic suffix offsetをq1/q2へ再利用
- 小blockは従来経路を維持するadaptive gate

q3 emitはprofile上70%以上削減したケースあり。

詳細:

```text
PersonalRag_VNEXT_PERIODIC_Q3_REUSE_FASTPATH_2026-08-20.md
```

---

## 3.7 Segment format v5 + small-shard q3 radix8

v4ではcontent sectionについて個別FNVとwhole-file FNVが重複していた。

v5 (`PRSEG2A5`) では:

- content standalone checksumのみ省略
- whole-file 64-bit FNVは維持
- 他13 sectionのstandalone checksumは維持
- readerはv4をstrictに読む

さらに小q3 shardは256 bucket × 2pass stable 8-bit radixを使用。

4KiB 21-pair A/Bで直前版比約27%短縮、21/21勝ち。

詳細:

```text
PersonalRag_VNEXT_V5_CONTENT_CHECKSUM_RADIX8_FASTPATH_2026-08-20.md
```

---

## 3.8 Segment format v6 + XXH64 whole-file checksum

v5後の最大CPU bottleneckはwrite_stream中のwhole-file FNVだった。

v6 (`PRSEG2A6`) では:

- whole-file checksumをFNV-1aからXXH64(seed=0)へ変更
- v4/v5 readerでは従来FNVをそのままstrict verify
- section layout・q1/q2/q3/content/pathは変更なし

4KiB/tmpfs 21-pairで:

```text
106.081 ms -> 57.694 ms
45.61%短縮
21/21勝ち
```

通常ディスク + sync_all()でも約16%短縮、7/7勝ち。

詳細:

```text
PersonalRag_VNEXT_V6_XXH64_WHOLE_FILE_FASTPATH_2026-08-20.md
```

---

## 3.9 現在: Q2 active-list fast path

v6後、high-periodic contentではq2 active keyが少ないにもかかわらず、owner-local unique q2 pair全体をpacked pairへ積み、65,536-bucket radix scatterしていた。

現在版では、flat16 q2かつq3 periodic proof coverageが75%以上の場合のみ:

```text
q2 key -> Vec<u16> owner list
```

へ直接appendする。

active q2 keyだけsortし、owner listを既存sparse q2 formatへencodeする。

fallbackは従来radix関数を文字どおりそのまま呼ぶため、非対象hot loopに追加branchを入れない。

adaptive gate:

```text
proven_owners * 4 >= block_count * 3
```

最終結果:

```text
cargo test --offline : 164 / 164 PASS

cq12_fused median:
7.4275 ms -> 6.3970 ms
13.87%短縮

tmpfs end-to-end:
72.229 ms -> 68.618 ms
5.00%短縮

normal disk + sync_all(), 21 pairs:
148.061 ms -> 144.846 ms
2.17%短縮
pairwise median improvement 3.56%
12 / 21 wins
```

Durable output:

```text
DURABLE_BYTE_IDENTITY=PASS
CURRENT + 4 .prseg2 + manifest
全ファイル名・size・SHA-256一致
```

詳細:

```text
PersonalRag_VNEXT_Q2_ACTIVE_LIST_FASTPATH_2026-08-20.md
PersonalRag_VNEXT_Q2_ACTIVE_LIST_FASTPATH_EVIDENCE_2026-08-20.txt
```

---

# 4. 現在のformat / compatibility

現在の新規write format:

```text
PRSEG2A6
version = 6
footer   = PR2FTR06
whole-file checksum = XXH64(seed=0)
```

Reader compatibility:

```text
v6 -> XXH64 whole-file verify
v5 -> legacy FNV whole-file verify
v4 -> legacy FNV whole-file verify + legacy content section FNV verify
```

v5とv6の検索用serialized sectionsは、v6 metadataをv5へ戻したcompatibility oracleでbyte identicalを確認済み。

Q2 active-list waveではformatを変更していないため、直前v6 baselineとdurable generation全6ファイルがbyte identical。

---

# 5. 現在の品質ゲート

最新正本で確認済み:

```text
cargo fmt -- --check                                  PASS
cargo clippy --offline --all-targets -- -D warnings  PASS
cargo test --offline                                  164 / 164 PASS
cargo build --offline --release                       PASS
pr_portable self-test                                 SELF_TEST_PASS
```

完成ZIPを新規ディレクトリへ展開した状態でも:

```text
164 / 164 PASS
release build PASS
SELF_TEST_PASS
release benchmark PASS
```

完成ZIP再展開時の参考benchmark:

```text
VNEXT_BUILD_PROFILE
 docs=20000
 payload=4096
 segments=4
 elapsed_ms=62.418
 bytes=87495475
```

絶対時間は共有CPU・storage状態で変動するので、過去絶対値だけを合否に使わないこと。

---

# 6. 開発環境

完全手順は:

```text
PersonalRag_DEV_ENV_BOOTSTRAP_2026-08-19.md
```

を正本とする。

要点:

```text
OS     : Debian 13系 x86_64
Rust   : 1.97.1
Cargo  : 1.97.1
Clippy : 0.1.97
rustfmt: 1.9.0-stable
LLVM   : 22.1.6
```

通常shellから外部ネットワークへ出られないため、Rustがない新規コンテナではGitHub Actions artifact経由でbootstrapする。

portable toolchain最終配置:

```text
/mnt/data/rust-toolchains/1.97.1-x86_64-unknown-linux-gnu
```

symlink:

```bash
TOOLCHAIN=/mnt/data/rust-toolchains/1.97.1-x86_64-unknown-linux-gnu
for x in rustc cargo rustdoc rustfmt clippy-driver cargo-clippy; do
  ln -sfn "$TOOLCHAIN/bin/$x" "/usr/local/bin/$x"
done
```

期待version:

```text
rustc 1.97.1 (8bab26f4f 2026-07-14)
cargo 1.97.1 (c980f4866 2026-06-30)
LLVM 22.1.6
```

---

# 7. 新規チャット開始直後にやること

## Step 1: ZIP検証と展開

チャットで提示された最新引き継ぎZIPのSHA-256を確認する。

```bash
sha256sum /mnt/data/<handoff zip>
```

展開後、`search-core`を見つける。

```bash
find /mnt/data -maxdepth 4 -type d -name search-core -print
```

---

## Step 2: Rust環境確認

```bash
rustc --version --verbose
cargo --version
cargo clippy --version
rustfmt --version
```

Rustがない場合は `PersonalRag_DEV_ENV_BOOTSTRAP_2026-08-19.md` に従ってGitHub Actions artifactから1.97.1を復元する。

---

## Step 3: 変更前Gate 0

**ソースを1行も変更する前に必ず実行する。**

```bash
cd <source>/search-core

cargo fmt -- --check
cargo clippy --offline --all-targets -- -D warnings
cargo test --offline
cargo build --offline --release
./target/release/pr_portable self-test
```

期待:

```text
164 / 164 PASS
SELF_TEST_PASS
```

新しい引き継ぎ後にテスト数が増えている場合は、件数ではなく全PASSを正とする。

---

## Step 4: baseline profileを取り直す

CPU最適化判定はtmpfs＋CPU affinityでI/O揺らぎを外す。

```bash
cd <source>/search-core
cargo build --offline --release --example vnext_build_profile_bench

PR_VNEXT_BENCH_ROOT=/dev/shm/pr-vnext-baseline \
PR_PROFILE_BUILD=1 \
taskset -c 0-3 \
./target/release/examples/vnext_build_profile_bench
```

通常benchmark:

```bash
PR_VNEXT_BENCH_ROOT=/dev/shm/pr-vnext-baseline \
taskset -c 0-3 \
./target/release/examples/vnext_build_profile_bench
```

payload変更例:

```bash
PR_VNEXT_BENCH_PAYLOAD=2048 \
PR_VNEXT_BENCH_ROOT=/dev/shm/pr-vnext-2k \
taskset -c 0-3 \
./target/release/examples/vnext_build_profile_bench
```

採否は:

1. warm-up
2. baseline/candidateを交互実行
3. 最低5回、できれば9〜21 pair
4. median
5. pairwise improvement median
6. win count
7. internal profile hot-path

を併用する。

---

# 8. 次に何をするべきか

## 最優先: 最新正本を再profileして新しい支配項を確定する

Q2 active-listでq2 radixが軽くなったため、**過去のprofile順位をそのまま信用しない**。

新チャットの最初の開発作業は、4KiB/tmpfsで次を再度分解すること。

優先観測項目:

```text
content q3 emit / shard / radix / encode
q1/q2 scan本体
q2 active-list encode
path index / path encode
write_stream (XXH64込み)
sync_all()
segment total
```

CPU optimizationは `/dev/shm`、durability確認は通常ディスクで分ける。

---

## 次の候補1: q3 residual CPU

periodic exact skip後もq3が残存CPU支配なら、次を調べる。

- periodic proof成立後も行っている固定処理
- active shardごとのmetadata/rank構築
- scatter/encodeのallocation/copy
- sparse shardでの固定scan

ただし過去に負けた案をそのまま再試行しないこと。

---

## 次の候補2: q1/q2 scan本体

現在のactive-listはradixを削っただけで、owner-local q2 detection自体は残っている。

もし最新profileでscanが最大なら:

- q3 periodic proof metadataを利用してさらにscan範囲を安全に減らせないか
- memory localityを壊さずblock単位で処理量を減らせないか
- q2 keyのactive universeが極端に小さいケースをさらに特化できないか

を検討する。

ただしq3 hot loopへq1/q2処理を混ぜる案は既に悪化済み。

---

## 次の候補3: path index

q3/q2が十分下がった場合はpath index/encodeの割合が相対的に上がる可能性がある。

profileで有意なら初めて触ること。

---

## 次の候補4: durable sync

通常ディスクでは`sync_all()` varianceが非常に大きい。

重要:

- durability semanticsを削って速くしない。
- `sync_all()`単純削除は禁止。
- file/directory publication順序、rename、manifest/CURRENTのfail-safe semanticsを維持する。

もしsyncが支配的なら、個別segmentのdurable completionとgeneration publicationの順序を設計レビューし、**同じfailure guaranteeのままsync回数をcoalesceできるか**を検討する。

これはWindows/filesystem差が大きいため、Linuxだけで最終採用を決めない。

---

# 9. 再実験しない/慎重に扱う失敗案

以下は過去に実装して実測で不採用になった。新しい状況で明確な理由がない限り、そのまま再試行しない。

- segment並列数を4→2/1へ減らす
- streaming initializer worker増加を速度目的に採用
- q3 24-bit大bitmap dedup
- q3 owner-stamp巨大表
- q3 touched-slotだけclearする方式
- q3小hash tableからgrow
- q3 rolling keyだけの単純最適化
- q3 sampling 16→4の単純削減
- q1/q2/q3/checksum全面1-pass融合
- q3 hot loopでq1/q2も副産物生成
- q2側で独立に周期検出し直す
- active-list末尾をlast_owner表の代用にする案
- `set_len()`によるsegment file事前確保
- BufWriter 4MiB
- BufWriter 256KiB
- whole-file FNVをBufWriter flush時に計算
- FNV 8byte手動unroll
- Linux `write_vectored`をproduction採用
- q3 checksum fusionでperiodic memcmpをscalar FNV loopへ置換

失敗理由・数値は各waveレポートに残っている。

---

# 10. 今後の実装waveの標準手順

1. **変更前回帰**
2. detailed profile
3. bottleneck仮説を1個に絞る
4. 最小変更で実装
5. 専用oracle test追加
6. targeted test
7. release A/B
8. 効果が弱い/負けたら即破棄
9. payload matrix
10. durable byte identity またはformat compatibility oracle
11. `cargo fmt --check`
12. `cargo clippy -D warnings`
13. 全回帰
14. release build
15. `pr_portable self-test`
16. 通常ディスク + `sync_all()` A/B
17. source diff review
18. 環境構築手順書更新
19. waveレポート/evidence作成
20. `target/`除外でZIP化
21. ZIP CRC/SHA確認
22. 完成ZIPを新規展開
23. 再展開物から全回帰/release/self-test/benchmark
24. 日本語1行コミットコメント案を提示

**git commitはユーザーから明示指示があるまで行わない。**

---

# 11. Benchmark/validationに関する注意

このChatGPT Linux環境は共有CPU/storageなので外れ値が大きい。

- 1回だけのelapsed_msを根拠にしない。
- baseline/candidateの実行順を交互に反転する。
- CPU optimizationは`taskset` + `/dev/shm`を主判定にする。
- 通常diskはdurable non-regression確認として使う。
- `sync_all()` spikeがあるためmeanだけで判断しない。
- median、pairwise median、wins、internal hot-pathを併記する。

---

# 12. bridge-coreの既知環境制約

この隔離環境では`bridge-core`のoffline build時、crates.io cacheに既存依存の`ignore` crateがないため依存解決前で停止する。

```text
no matching package named `ignore` found
```

これは今回までのsearch-core変更とは無関係。

`search-core`は外部dependencyなしでoffline regression/buildが可能。

---

# 13. Windowsで最後に行うこと

Linux上のsearch-core検証が完了しても、production switchの最終acceptanceはWindowsで実施する。

既存script:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\validate-vnext-production-switch-windows.ps1 -LaunchShadow
```

Windows native scanner関連のreport/scriptもZIP内にある。

---

# 14. 新規チャットに渡す最初の指示文例

以下をそのまま新チャットへ伝えればよい。

```text
このZIPがPersonalRagの最新正本です。
まず PersonalRag_HANDOFF_2026-08-20.md と
PersonalRag_DEV_ENV_BOOTSTRAP_2026-08-19.md を読んでください。

ソースを変更する前にRust 1.97.1環境を確認し、
search-coreでfmt/clippy/全回帰/release/self-testを通してください。
現在の期待回帰は164/164 PASSです。

その後、VNextQ2ActiveListFastPathをbaselineとして
4KiB/tmpfs＋CPU固定で詳細profileを取り直し、
新しく最大になったボトルネックを1つずつ高速化してください。
単発値ではなく交互A/B中央値で採否し、
serialized bytes/durable output/reader compatibilityを壊さないでください。

実装後は専用oracle→全回帰→fmt/clippy→release/self-test→
normal disk sync_all A/B→レポート/環境手順更新→ZIP化→
完成ZIP新規展開から再回帰まで実施してください。
負けた高速化案は残さないでください。
ユーザー明示なしにgit commitしないでください。
変更後は日本語1行のコミットコメント案を提示してください。
```

---

# 15. 現在地点の一言まとめ

PersonalRag vNext buildは、

```text
Q1 bitset
→ Q2 owner stamp
→ Q1/Q2 fused
→ checksum fusion
→ Q3 recent cache
→ Q1-from-Q2
→ exact periodic Q3 skip / proof reuse
→ v5 duplicate content checksum removal / q3 radix8
→ v6 XXH64 whole-file checksum
→ Q2 adaptive active-list
```

まで高速化済み。

**現在の正本は164/164 PASS、v6 durable formatを維持し、Q2 active-list waveまで採用済み。次は最新profileから支配項を再決定するところから再開する。**
