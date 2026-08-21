# PersonalRag 開発・実測環境構築手順

**更新日:** 2026-08-20  
**目的:** 新規ChatGPTチャットへPersonalRag高速化開発を引き継いだ際、現在と同等のRust開発・回帰テスト・release build・vNextベンチマーク環境を、手動ファイル受け渡しなしで再構築する。

---

## 1. この文書で再現する環境

現在確認済みの環境は以下。

```text
OS/ABI     : Linux x86_64 / Debian 13系
Kernel     : Linux 6.18.35
CPU        : Intel Xeon Platinum 8573C
visible CPU: 5 (`nproc` / `os.cpu_count()`)
cgroup CPU : 4 CPU equivalent (`cpu.max = 400000 100000` at this session)
Rust effective parallelism: 4 (`std::thread::available_parallelism()` is cgroup-aware)
glibc      : 2.41
Rust host  : x86_64-unknown-linux-gnu

rustc      : 1.97.1 (8bab26f4f 2026-07-14)
cargo      : 1.97.1 (c980f4866 2026-06-30)
clippy     : 0.1.97 (8bab26f4f6 2026-07-14)
rustfmt    : 1.9.0-stable (8bab26f4f6 2026-07-14)
LLVM       : 22.1.6
```

PersonalRag側は次を要求する。

`rust-toolchain.toml`:

```toml
[toolchain]
channel = "1.97.1"
components = ["clippy", "rustfmt"]
profile = "minimal"
```

`search-core/Cargo.toml`:

```toml
[package]
rust-version = "1.97"
edition = "2024"
```

### 現在のソース正本

2026-08-20時点で、この手順書を同梱する最新ソースbuild tagは次。

```text
PersonalRag_GUI_PortableCore_VNextQ2ActiveListFastPath_2026-08-20.zip
```

ZIP自身のSHA-256を本文へ埋め込むと自己参照になり再現不能になるため、**同梱外の companion `.zip.sha256` を正本とする**。
新規チャットでより新しいZIPが渡された場合は、そのZIPとcompanion SHAを正本に置き換えること。

このbuildの直前基準（A/B oracle）は次。

```text
PersonalRag_GUI_PortableCore_VNextV6Xxh64FastPath_2026-08-20.zip
SHA-256:
0fdb1a37bb1e3d980d206c9e9ed75a5a108bb1473360a2e3ab5380c358ef7c2d
```

---

## 2. 重要: 通常のRustインストールは使えない

ChatGPTの作業コンテナでは、通常シェルから外部ネットワークへ直接出られない場合がある。

実際に2026-08-19の環境では以下が使用不能だった。

- `curl https://sh.rustup.rs`
- `rustup` による `static.rust-lang.org` からの直接取得
- `apt` によるRust導入
- `git clone` によるGitHub直接取得
- `container.download` でRust公式 `.tar.xz` / `.tar.gz` を直接取得

Webブラウザ/Web検索は外へ出られても、バイナリ配布物を作業コンテナへ渡す段階でMIME/ダウンロード制限に当たる場合がある。

**現在成功している正規のbootstrap方法は、GitHub ActionsでRust 1.97.1を生成し、GitHub Actions artifactをGitHubコネクタ経由で作業コンテナへ持ち込む方法。**

手動アップロードは不要。

---

# 3. Rust 1.97.1 bootstrap手順

## 3.1 使用するGitHubリポジトリ

2026-08-19に実際にbootstrap用として使用したリポジトリ:

```text
bokoprin/stack_overflow_chatbot
```

このリポジトリの `main` は変更しない。

一時ブランチのみ使用する。

推奨ブランチ名例:

```text
chatgpt-rust-bootstrap-YYYYMMDD
```

2026-08-19には以下を使用した。

```text
chatgpt-rust-bootstrap-20260819
```

既に同名ブランチがある場合は別名にする。

---

## 3.2 GitHubコネクタで一時ブランチを作る

新規チャットではGitHubコネクタを使用する。

概念的な手順:

1. GitHub Appのinstallationを確認
2. `bokoprin/stack_overflow_chatbot` へpush権限があることを確認
3. `main` から一時ブランチを作成
4. 一時ブランチにbootstrap workflowを1ファイルだけ追加
5. 一時ブランチ → `main` のdraft PRを作成
6. `pull_request` イベントでActionsを起動

**mainへ直接workflowを追加しないこと。PRは絶対にmergeしないこと。**

---

## 3.3 bootstrap用GitHub Actions workflow

一時ブランチに以下を作成する。

推奨パス:

```text
.github/workflows/chatgpt-rust-bootstrap.yml
```

内容:

```yaml
name: Temporary Rust 1.97.1 bootstrap

on:
  pull_request:
    branches:
      - main

permissions:
  contents: read

jobs:
  build-toolchain:
    runs-on: ubuntu-latest

    steps:
      - name: Install exact Rust toolchain
        shell: bash
        run: |
          set -euo pipefail
          rustup toolchain install 1.97.1 \
            --profile minimal \
            --component clippy \
            --component rustfmt

          rustup run 1.97.1 rustc --version --verbose
          rustup run 1.97.1 cargo --version
          rustup run 1.97.1 cargo clippy --version
          rustup run 1.97.1 rustfmt --version

      - name: Pack portable toolchain
        shell: bash
        run: |
          set -euo pipefail

          WORK="$RUNNER_TEMP/chatgpt-rust-bootstrap"
          TOOLCHAIN="$HOME/.rustup/toolchains/1.97.1-x86_64-unknown-linux-gnu"
          ARCHIVE="$WORK/rust-1.97.1-x86_64-unknown-linux-gnu.tar.gz"

          mkdir -p "$WORK"
          test -x "$TOOLCHAIN/bin/rustc"
          test -x "$TOOLCHAIN/bin/cargo"
          test -x "$TOOLCHAIN/bin/clippy-driver"
          test -x "$TOOLCHAIN/bin/rustfmt"

          tar -C "$HOME/.rustup/toolchains" \
            -czf "$ARCHIVE" \
            1.97.1-x86_64-unknown-linux-gnu

          sha256sum "$ARCHIVE" > "$ARCHIVE.sha256"

      - name: Upload portable toolchain
        uses: actions/upload-artifact@v4
        with:
          name: chatgpt-rust-1.97.1-linux-x64
          path: |
            ${{ runner.temp }}/chatgpt-rust-bootstrap/rust-1.97.1-x86_64-unknown-linux-gnu.tar.gz
            ${{ runner.temp }}/chatgpt-rust-bootstrap/rust-1.97.1-x86_64-unknown-linux-gnu.tar.gz.sha256
          if-no-files-found: error
          retention-days: 1
```

### 注意

Actions runnerには通常Rust/rustupが導入済みなので、そこで外部ネットワークを使って1.97.1を取得する。

この処理はPersonalRagコードを一切触らない。

---

## 3.4 draft PRを作ってActionsを起動

一時ブランチから `main` へdraft PRを作る。

例:

```text
Title:
[temporary] Rust 1.97.1 bootstrap artifact

Body:
Temporary infrastructure-only PR created to package Rust 1.97.1
for an isolated development environment.
It does not modify application code and will be closed after the artifact is retrieved.
```

2026-08-19には実際にPR #5を使用した。

PR作成後、workflow runを取得する。

成功時は少なくとも以下がすべて `success` になること。

```text
Install exact Rust toolchain  success
Pack portable toolchain       success
Upload portable toolchain     success
```

---

## 3.5 artifactをGitHubコネクタから取得

workflow runからartifact一覧を取得する。

artifact名:

```text
chatgpt-rust-1.97.1-linux-x64
```

2026-08-19に生成したartifactは次だった。

```text
artifact id   : 9354717466
artifact size : 214,917,894 bytes
ZIP SHA-256   : 9ebd768d2cfe46755041c5c24a32dee859b6fb556c126d6126efff325a976134
```

このID・SHAは再生成するたびに変わるので、将来は**そのrunが返した値を正とする**。

GitHubコネクタの `download_workflow_artifact` を使うと、artifact ZIPが作業コンテナにmaterializeされる。

2026-08-19には以下へ配置された。

```text
/mnt/data/chatgpt-rust-1.97.1-linux-x64.zip
```

将来はパスが変わる可能性があるため、コネクタが返した実パスを使用すること。

---

# 4. artifact検証とローカルインストール

## 4.1 ZIP SHA-256確認

GitHub artifact metadataの `digest` とローカルZIPを比較する。

```bash
sha256sum /mnt/data/chatgpt-rust-1.97.1-linux-x64.zip
```

GitHub側が例えば

```text
sha256:<HASH>
```

を返したなら、完全一致すること。

**不一致なら展開しない。**

---

## 4.2 artifact ZIP展開

```bash
mkdir -p /mnt/data/rust-bootstrap-1.97.1
unzip -o \
  /mnt/data/chatgpt-rust-1.97.1-linux-x64.zip \
  -d /mnt/data/rust-bootstrap-1.97.1
```

期待ファイル:

```text
/mnt/data/rust-bootstrap-1.97.1/
├── rust-1.97.1-x86_64-unknown-linux-gnu.tar.gz
└── rust-1.97.1-x86_64-unknown-linux-gnu.tar.gz.sha256
```

2026-08-19に生成した内部tar.gzのSHA-256:

```text
6a5ff618a0a5a9c05ecbec90aa2519af3aa1049c107efdffaec12bc38a6d80e6
```

これは同じworkflow内容なら再現する可能性はあるが、GitHub runner/tar metadata等で変わり得るため、将来は**同梱checksumのハッシュ値とローカルtar.gzの実測値を比較**する。

checksumファイルにはGitHub runner上の絶対パスが入っているため、そのまま

```bash
sha256sum -c ...sha256
```

するとパス不一致になる場合がある。

その場合は値だけ比較する。

例:

```bash
EXPECTED=$(awk '{print $1}' \
  /mnt/data/rust-bootstrap-1.97.1/rust-1.97.1-x86_64-unknown-linux-gnu.tar.gz.sha256)

ACTUAL=$(sha256sum \
  /mnt/data/rust-bootstrap-1.97.1/rust-1.97.1-x86_64-unknown-linux-gnu.tar.gz \
  | awk '{print $1}')

printf 'expected=%s\nactual=%s\n' "$EXPECTED" "$ACTUAL"
test "$EXPECTED" = "$ACTUAL"
```

---

## 4.3 portable toolchainを展開

配置先は現在と同じにする。

```bash
mkdir -p /mnt/data/rust-toolchains

tar -C /mnt/data/rust-toolchains \
  -xzf /mnt/data/rust-bootstrap-1.97.1/rust-1.97.1-x86_64-unknown-linux-gnu.tar.gz
```

最終配置:

```text
/mnt/data/rust-toolchains/1.97.1-x86_64-unknown-linux-gnu
```

2026-08-19時点では約624MB。

確認:

```bash
ls -l /mnt/data/rust-toolchains/1.97.1-x86_64-unknown-linux-gnu/bin
```

---

# 5. /usr/local/bin へsymlink作成

以後のチャットでPATH設定を毎回行わず、普通に `cargo` / `rustc` を使えるようにする。

```bash
TOOLCHAIN=/mnt/data/rust-toolchains/1.97.1-x86_64-unknown-linux-gnu

for x in rustc cargo rustdoc rustfmt clippy-driver cargo-clippy; do
  ln -sfn "$TOOLCHAIN/bin/$x" "/usr/local/bin/$x"
done
```

確認:

```bash
for x in rustc cargo rustdoc rustfmt clippy-driver cargo-clippy; do
  echo "$x -> $(readlink -f /usr/local/bin/$x)"
done
```

期待先:

```text
/mnt/data/rust-toolchains/1.97.1-x86_64-unknown-linux-gnu/bin/<binary>
```

バージョン確認:

```bash
rustc --version --verbose
cargo --version
cargo clippy --version
rustfmt --version
```

期待値:

```text
rustc 1.97.1 (8bab26f4f 2026-07-14)
cargo 1.97.1 (c980f4866 2026-06-30)
clippy 0.1.97 (8bab26f4f6 2026-07-14)
rustfmt 1.9.0-stable (8bab26f4f6 2026-07-14)
host: x86_64-unknown-linux-gnu
LLVM version: 22.1.6
```

---

# 6. PersonalRagソース展開

添付された正本ZIPのSHA-256を最初に確認する。

例:

```bash
sha256sum /mnt/data/PersonalRag_GUI_PortableCore_VNextBuildFastPath_2026-08-19.zip
```

2026-08-19版の期待値:

```text
ee803a65225a827070e82bd3f6cfc55ebb33c1c984dc6d4d5063137fee86016d
```

展開例:

```bash
cd /mnt/data
unzip -q PersonalRag_GUI_PortableCore_VNextBuildFastPath_2026-08-19.zip
```

2026-08-19版の `search-core`:

```text
/mnt/data/PersonalRag_GUI_PortableCore_VNextBuildFastPath_2026-08-19/
  PersonalRag_GUI_PortableCore_VNextBuildFastPath_2026-08-19/
    search-core/
```

新しいZIPではディレクトリ名が変わる可能性がある。

必ず実体を検索する。

```bash
find /mnt/data -maxdepth 4 -type d -name search-core -print
```

以後:

```bash
cd <正本のsearch-core>
```

---

# 7. 環境構築完了ゲート

**高速化実装を始める前に必ず以下をすべて通す。**

順序も維持する。

## Gate A: rustfmt

```bash
cargo fmt -- --check
```

期待:

```text
PASS
```

---

## Gate B: Clippy

```bash
cargo clippy --offline --all-targets -- -D warnings
```

期待:

```text
PASS / warning 0
```

2026-08-19環境での初回実測参考値:

```text
約5.25秒
peak RSS 約313MiB
```

性能評価値ではなく、環境確認参考値。

---

## Gate C: 変更前回帰テスト

```bash
cargo test --offline
```

2026-08-20版正本では:

```text
164 passed
0 failed
```

実測参考:

```text
wall    約26.84 sec
max RSS 約449 MiB
```

**新しいソースではテスト件数が増減し得る。重要なのは全PASSであること。**

実装前に回帰テストが失敗している場合、高速化実装を始めない。

---

## Gate D: release build

```bash
cargo build --offline --release
```

期待成果物例:

```text
target/release/pr_portable
```

実行ラッパー側のtimeoutで途中終了した場合、Rustコンパイルエラーと混同しないこと。

`target/release/deps` 等の中間成果物が生成されているなら、同じコマンドを再度実行して完了させる。

---

## Gate E: vNext baseline benchmark

```bash
cargo run --offline --release --example vnext_build_profile_bench
```

デフォルト条件:

```text
docs         = 20,000
payload      = 4,096 bytes
segment docs = 5,000
segments     = 4
```

環境変数:

```text
PR_VNEXT_BENCH_DOCS
PR_VNEXT_BENCH_PAYLOAD
PR_VNEXT_BENCH_SEGMENT_DOCS
PR_VNEXT_BENCH_ROOT
PR_VNEXT_BENCH_STREAM_WORKERS
PR_VNEXT_BENCH_KEEP
PR_PROFILE_BUILD
```

2026-08-19の最初の実測:

```text
VNEXT_BUILD_PROFILE docs=20000 payload=4096 segments=4 elapsed_ms=398.987 bytes=87495475
```

この環境で過去レポートの約0.41秒帯を再現できている。

---

# 8. ベンチマークの取り方

このChatGPT実行環境は共有5-vCPU環境であり、特に並列worker使用時に外れ値が出る。

**単発値で高速化採否を判断しない。**

原則:

1. release buildを済ませる
2. warm-upを最低1回行う
3. 同一条件で最低5回測る
4. 中央値を主指標にする
5. min/maxも残す
6. output bytes / correctnessも比較する
7. 変更前後を同一チャット・同一環境で測る

### normal

```bash
unset PR_VNEXT_BENCH_STREAM_WORKERS
cargo run --offline --release --example vnext_build_profile_bench
```

### streaming 1 worker

```bash
PR_VNEXT_BENCH_STREAM_WORKERS=1 \
cargo run --offline --release --example vnext_build_profile_bench
```

### streaming 2 workers

```bash
PR_VNEXT_BENCH_STREAM_WORKERS=2 \
cargo run --offline --release --example vnext_build_profile_bench
```

### streaming 4 workers

```bash
PR_VNEXT_BENCH_STREAM_WORKERS=4 \
cargo run --offline --release --example vnext_build_profile_bench
```

2026-08-19に5回ずつ取得した参考値:

| Mode | Median | Min | Max |
|---|---:|---:|---:|
| normal | **420.756 ms** | 399.198 ms | 449.215 ms |
| streaming 1 | 736.296 ms | 636.163 ms | 896.841 ms |
| streaming 2 | 565.445 ms | 480.967 ms | 1177.358 ms |
| streaming 4 | 456.846 ms | 391.170 ms | 1539.610 ms |

全ケース:

```text
bytes=87495475
```

が一致した。

worker 2/4は外れ値が非常に大きいので、並列化評価では特に中央値を使うこと。

---

# 9. 内部build profiling

vNext内部のボトルネックを見る場合:

```bash
PR_PROFILE_BUILD=1 \
cargo run --offline --release --example vnext_build_profile_bench
```

2026-08-19 normalの一例:

```text
total elapsed        約443 ms
segment total        約362-363 ms
index_group          約178-189 ms
checksum             約26-27 ms
write total          約145-157 ms
write_stream         約38-41 ms
write_sync           約104-119 ms
```

20k × 4KiB / 5k docs per segment条件では:

```text
logical_occurrences = 20,475,000
Q2 layout            = flat16
```

が選択された。

今後の高速化では、少なくとも次を分離して見る。

- index構築
- Q2/Q3生成
- checksum
- encode
- stream write
- fsync/write_sync
- segment worker並列化
- peak RSS

---

# 10. `/usr/bin/time -v` による実測

壁時計時間とpeak RSSも必要なら:

```bash
/usr/bin/time -v cargo test --offline
```

またはrelease benchmarkバイナリを直接使う。

Cargo起動/コンパイルノイズを避けるには、release build後に生成されたexample binaryを直接実行する方がよい。

```bash
find target/release/examples -maxdepth 1 \
  -type f -name 'vnext_build_profile_bench-*' \
  -executable -print
```

同一binaryでbefore/afterを比較すること。

---

# 11. 高速化開発時の必須手順

環境構築完了後の変更は必ず以下で行う。

1. 変更前の既存回帰テストを実行しPASSを確認
2. 現状baselineを複数回実測
3. bottleneck/profileを取得
4. 変更内容と仮説を整理
5. 設計
6. 実装
7. 実装部分に対応するテスト追加/修正
8. 実装部分テスト
9. 同一条件でA/Bベンチ
10. correctness / output bytes / query semantics確認
11. 全既存回帰テスト
12. `cargo fmt -- --check`
13. `cargo clippy --offline --all-targets -- -D warnings`
14. release build
15. コードレビュー
16. 問題があれば設計→実装修正→テスト→回帰→再レビュー（最大3ループ）

高速化では「速くなったように見える」だけで採用しない。

採用条件は原則:

- correctness維持
- 回帰テスト全PASS
- output semantics維持
- 同一条件の中央値で改善
- peak RAM/index size等に重大な悪化なし
- code complexity増加に見合う効果がある

---

# 12. bootstrap用GitHub資源の後片付け

artifactを取得し、ローカルtoolchainが動いたら必ず片付ける。

## 12.1 draft PRをclose

**mergeしない。**

2026-08-19のPR #5も未mergeのままcloseした。

## 12.2 一時ブランチをmainの元SHAへ戻す

GitHubコネクタにbranch delete操作がない場合は、一時ブランチrefをPR作成前の`main` SHAへforce resetする。

これによりbootstrap workflow commitをブランチ先端から外す。

その後、一時ブランチ上で

```text
.github/workflows/chatgpt-rust-bootstrap.yml
```

が見えないことを確認する。

mainには最初から変更を入れていないことも確認する。

artifactは `retention-days: 1` なので自動失効する。

---

# 13. Google Driveについて

2026-08-19には途中でGoogle Driveを中継案として試したが、最終的には使用不要だった。

Google Driveコネクタは既存ファイルのupload/downloadには使えるが、外部URLを直接Driveへ保存する用途には使えなかった。

一時的に作成した:

```text
ChatGPT_Rust_Toolchain_Transfer
```

フォルダは最終的に削除済み。

**現在の推奨経路はGoogle DriveではなくGitHub Actions artifact。**

---

# 14. 環境再構築チェックリスト

新規チャットでは以下を上から順に確認する。

```text
[ ] 添付されたPersonalRag正本ZIPを特定
[ ] ZIP SHA-256を確認
[ ] search-coreの実パスを特定
[ ] rust-toolchain.tomlを確認
[ ] rustc/cargo 1.97.1が既に存在するか確認

存在しなければ:
[ ] GitHub writable repoを確認
[ ] mainから一時bootstrap branch作成
[ ] bootstrap workflowを一時branchへ追加
[ ] draft PR作成（絶対mergeしない）
[ ] ActionsでRust 1.97.1 + clippy + rustfmt生成
[ ] artifact run成功を確認
[ ] artifact ZIPをGitHubコネクタから取得
[ ] artifact digestとZIP SHA-256一致確認
[ ] 内部tar.gz SHA-256確認
[ ] /mnt/data/rust-toolchains/1.97.1-x86_64-unknown-linux-gnuへ展開
[ ] /usr/local/binへ6本symlink
[ ] rustc/cargo/clippy/rustfmt version確認

PersonalRag gate:
[ ] cargo fmt -- --check PASS
[ ] cargo clippy --offline --all-targets -- -D warnings PASS
[ ] cargo test --offline 全PASS
[ ] cargo build --offline --release PASS
[ ] vnext_build_profile_bench実行成功
[ ] baselineをwarm-up後5回以上取得
[ ] median/min/max/output bytesを保存
[ ] PR_PROFILE_BUILD=1が動作

後片付け:
[ ] bootstrap draft PR close
[ ] bootstrap branchを元main SHAへreset/削除
[ ] mainに変更がないことを確認
```

---

# 15. 新規チャットへ渡す短い指示文

このMarkdownとPersonalRag正本ZIPを新規チャットへ添付し、最初に次のように指示すればよい。

> 添付した `PersonalRag_DEV_ENV_BOOTSTRAP_2026-08-19.md` を正として、まず現在の実行環境を確認してください。Rust 1.97.1環境が残っていなければ、Markdown記載のGitHub Actions artifact方式で手動受け渡しなしに再構築してください。環境構築後、fmt / clippy / 変更前回帰 / release build / vNext baseline benchmarkまで実測してから高速化開発を開始してください。ソースは添付ZIPを正本とし、変更前回帰が通るまでコードを変更しないでください。

---

# 16. 現在の確定状態（2026-08-20）

```text
環境構築                      100%
Rust 1.97.1                   OK
Cargo 1.97.1                  OK
Clippy 0.1.97                 OK
rustfmt 1.9.0                 OK
/usr/local/bin symlink        OK
cargo fmt --check             PASS
cargo clippy -D warnings      PASS
search-core回帰               164/164 PASS
release build                 PASS
pr_portable self-test         SELF_TEST_PASS
vNext benchmark               実測可能
PR_PROFILE_BUILD/Q3           実測可能
```

現在の正本build tag:

```text
PersonalRag_GUI_PortableCore_VNextQ2ActiveListFastPath_2026-08-20.zip
```

ZIP自身のSHAはcompanion `.zip.sha256` を正とする。

直前A/B oracle:

```text
PersonalRag_GUI_PortableCore_VNextV6Xxh64FastPath_2026-08-20.zip
SHA-256:
0fdb1a37bb1e3d980d206c9e9ed75a5a108bb1473360a2e3ab5380c358ef7c2d
```

現在の開発方針は、**実測→仮説→実装→A/B→互換性/意味論検証→全回帰→レビューを繰り返し、勝った変更だけ残すこと**。

---

# 17. 2026-08-20 v5高速化buildの環境完成確認

環境再構築後に最低限以下を通す。

```bash
cd search-core
cargo fmt -- --check
cargo clippy --offline --all-targets -- -D warnings
cargo test --offline
cargo build --offline --release
./target/release/pr_portable self-test
cargo build --offline --release --example vnext_build_profile_bench
```

この文書更新時の結果:

```text
fmt                         PASS
clippy -D warnings          PASS
search-core tests           160 / 160 PASS
release build               PASS
pr_portable self-test       SELF_TEST_PASS
```

代表benchmark条件:

```text
PR_VNEXT_BENCH_DOCS=20000
PR_VNEXT_BENCH_PAYLOAD=4096
PR_VNEXT_BENCH_SEGMENT_DOCS=5000
CPU affinity=0-3
primary CPU A/B root=/dev/shm
```

直前 `VNextPeriodicQ3ReuseFastPath` との最終4KiB non-profile交互A/B（21組）:

```text
baseline median              157.815 ms
current median               114.866 ms
separate-median improvement    27.21 %
pairwise improvement median    31.53 %
pair wins                     21 / 21
```

4KiB内部profile（7組 / 各側28 segment samples）:

```text
content q1/q2 fused median   29.659 ->  6.721 ms   77.34 % faster
index group median           52.119 -> 25.907 ms   50.29 % faster
content q3 median            17.216 -> 14.966 ms   13.07 % faster
q3 radix prefix median        0.776 ->  0.005 ms   99.42 % faster
profile end-to-end median   176.235 ->118.711 ms   32.64 % faster
```

payload別non-profile中央値:

```text
512 B: 76.915 -> 52.884 ms    31.24 % faster, 8/9 wins
1 KiB: 70.529 -> 56.611 ms    19.73 % faster, 21/21 wins
2 KiB: 136.545 -> 88.666 ms   35.06 % faster, 8/9 wins
4 KiB: 157.815 ->114.866 ms   27.21 % faster, 21/21 wins
```

通常ディスク + `sync_all()` の4KiB A/B（7組）:

```text
baseline median              323.544 ms
current median               289.761 ms
improvement                    10.44 %
pair wins                       7 / 7
```

v5 formatは `PRSEG2A5` / version 5。content sectionの個別FNV fieldは0とし、既存のwhole-file 64-bit FNVでcontentを含む全segment bytesを検証する。他13 sectionの個別checksumは維持する。readerは旧 `PRSEG2A4` / version 4をstrictに読める。

旧正本writerの実v4 segmentと、新v5 segmentをv4 metadata/checksumだけへ変換したものが **21,869,176 bytes完全一致**することを確認済み。検索用sectionのserialized bytesは変更していない。

詳細は `PersonalRag_VNEXT_V5_CONTENT_CHECKSUM_RADIX8_FASTPATH_2026-08-20.md` と evidence textを参照。

`bridge-core` はこの隔離環境では crates.io cache に `ignore` crate がないため、`cargo check --offline` は依存解決時点で停止する。これはsearch-core変更の失敗ではない。

---

# 18. 2026-08-20 v6 XXH64 whole-file checksum高速化buildの環境完成確認

このbuildの新規writer format:

```text
PRSEG2A6 / version 6
footer PR2FTR06
whole-file checksum: XXH64(seed=0)
```

Reader互換:

```text
v6 -> XXH64 whole-file verify
v5 -> legacy FNV whole-file verify
v4 -> legacy FNV whole-file verify + legacy content section checksum
```

v5+ではcontent sectionのstandalone checksum fieldは0のまま維持し、content以外13 sectionのstandalone FNV checksumも維持する。

環境再構築後に最低限以下を通す。

```bash
cd search-core
cargo fmt -- --check
cargo clippy --offline --all-targets -- -D warnings
cargo test --offline
cargo build --offline --release
./target/release/pr_portable self-test
cargo build --offline --release --example vnext_build_profile_bench
```

この文書更新時の結果:

```text
fmt                         PASS
clippy -D warnings          PASS
search-core tests           162 / 162 PASS
release build               PASS
pr_portable self-test       SELF_TEST_PASS
```

代表4KiB CPU A/B（tmpfs, CPU 0-3, 21組）:

```text
v5 median                   106.081 ms
v6 median                    57.694 ms
median improvement            45.61 %
pairwise improvement median   43.69 %
v6 wins                       21 / 21
```

payload別11組A/B:

```text
512 B : 50.611 -> 42.525 ms   15.98 % faster, 11/11 wins
1 KiB : 63.571 -> 50.183 ms   21.06 % faster, 11/11 wins
2 KiB : 71.647 -> 43.849 ms   38.80 % faster, 11/11 wins
4 KiB :109.274 -> 61.542 ms   43.68 % faster, 11/11 wins
```

通常ディスク + `sync_all()` 4KiB（7組）:

```text
v5 median                   271.801 ms
v6 median                   228.064 ms
improvement                   16.09 %
v6 wins                        7 / 7
```

旧v5 writerの実segmentと、新v6 segmentをv5 magic/version/footer/FNVへ戻したものは **21,869,176 bytes完全一致**。

```text
actual v5 SHA-256:
48e10f923e00ae1d1e4f62784be8e580848c76002f9f8d1522c2c002be95810e

v6 converted-to-v5 SHA-256:
48e10f923e00ae1d1e4f62784be8e580848c76002f9f8d1522c2c002be95810e
```

XXH64はreference vectorとstreaming chunk境界一致をunit testで固定している。

詳細は:

```text
PersonalRag_VNEXT_V6_XXH64_WHOLE_FILE_FASTPATH_2026-08-20.md
PersonalRag_VNEXT_V6_XXH64_WHOLE_FILE_FASTPATH_EVIDENCE_2026-08-20.txt
```

`bridge-core` はこの隔離環境では既存依存 `ignore` crate がoffline cacheにないため、`cargo check --offline` が依存解決前で停止する。search-coreの失敗ではない。



---

# 19. 2026-08-20 q2 active-list fast path buildの環境完成確認

現在の新規writer formatはv6のまま。

```text
PRSEG2A6 / version 6
whole-file checksum: XXH64(seed=0)
```

今回のbuild-only高速化は、flat16 q2かつq3 periodic proof coverage >= 75%の場合だけq2 active owner-listを使い、packed pair radix scatterを省く。対象外では従来radix関数をそのまま使う。

環境再構築後に最低限以下を通す。

```bash
cd search-core
cargo fmt -- --check
cargo clippy --offline --all-targets -- -D warnings
cargo test --offline
cargo build --offline --release
./target/release/pr_portable self-test
cargo build --offline --release --example vnext_build_profile_bench
```

この文書更新時の結果:

```text
fmt                         PASS
clippy -D warnings          PASS
search-core tests           164 / 164 PASS
release build               PASS
pr_portable self-test       SELF_TEST_PASS
```

直前v6正本との4KiB CPU profile（tmpfs, CPU 0-3, 7 runs）:

```text
cq12_fused median           7.4275 -> 6.3970 ms
hot-path reduction          13.87 %
end-to-end median           72.229 -> 68.618 ms
end-to-end reduction         5.00 %
```

通常ディスク + `sync_all()` 4KiB（交互21組）:

```text
v6 median                   148.061 ms
active-list median          144.846 ms
median reduction              2.17 %
pairwise reduction median     3.56 %
active-list wins             12 / 21
```

同一20k×4KiB durable generationについて、`CURRENT`、4 segment、manifestの全6ファイルSHA-256は直前v6正本と完全一致。

```text
DURABLE_BYTE_IDENTITY=PASS
```

専用oracle:

- `periodic_q2_active_lists_are_byte_identical_to_radix_oracle`
- `periodic_q2_active_list_gate_requires_three_quarters_exact_proof`

詳細は `PersonalRag_VNEXT_Q2_ACTIVE_LIST_FASTPATH_2026-08-20.md` と evidence txt を参照すること。
