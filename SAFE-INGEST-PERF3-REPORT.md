# PersonalRag Safe Ingestion / Performance Pass 3 — 2026-08-15

## 目的

大量の画像・実行ファイル・その他binaryが存在するWindows treeで、index build中にraw body hydrationがメモリ/I/Oを圧迫してプロセス終了につながる問題を防止する。同時に、既存App Contract v1 / Engine Facade / persistent index formatを維持したまま、現在観測されているingestion hot pathを小規模に最適化する。

## 実装

### 1. binary/imageはfilename/pathのみindex

GUI scannerが拡張子からbinary/container形式を判定し、`ScannedFile.index_content=false`としてPortable Coreへ渡す。Portable Coreはdocument自体を残すためfilename/path検索は維持するが、bodyを読まずcontent indexを空にする。

主対象: jpg/jpeg/png/gif/bmp/webp/ico/tiff/heic/avif/psd、audio/video、zip/7z/rar/gz/xz/zst、exe/dll/sys/pdb/obj/lib/so、pyc、Office/PDF、sqlite/db、font等。

このpolicyはbridge側に置き、portable index formatには埋め込まない。

### 2. hydrationを件数 + bytesの二重上限に変更

従来は最大`segment_docs * 2` filesを一括hydrateしていたため、file sizeが大きいtreeでは1 batchが巨大化し得た。

`DiskPathBuildConfig.hydration_batch_bytes`を追加し、GUI側では検出されたmemory budgetの1/8を基準に32–128 MiBへclampする。1ファイルがbudgetを超える場合でも最低1件は処理する。通常GUIの最大file size 32 MiBとの組み合わせでは、hydration bufferが無制限に膨らまない。

### 3. ASCII normalizeをin-place化

従来:

`fs::read -> original Vec -> fold_ascii -> second full-size Vec`

変更後:

`fs::read -> Vec::make_ascii_lowercase()`

normalized bytesはbyte-identicalで、full-size temporary allocationを1本削減する。

### 4. Windows小ファイル向けhydration worker tuning

Windowsではscan metadataからcontent対象fileのaverage sizeを計算し、小ファイルtreeではI/O latency hidingのためhydration worker上限を拡大する。

- average <= 64 KiB: max 8 workers
- average <= 1 MiB: max 4 workers
- larger: max 2 workers

非Windowsは従来tuningを維持する。

### 5. 除外preset拡張

- venv: `.venv2`等の`.venv`/`venv` numeric/suffix variant
- build: `bin`, `obj`, `.vs`, `debug`, `release`, `cmake-build-*`, `build-*`, `out-*`

除外checkboxがONのときのみ適用する。

### 6. 長いcurrent pathでGUIが横に伸びる問題を修正

`#index-status`へ`min-width:0`, `overflow:hidden`, `text-overflow:ellipsis`, `white-space:nowrap`を追加。全文は`title`属性に保持する。

### 7. prepared memory表示を実データへ接続

hydration中の`prepared_bytes`をSearch Core -> bridge -> Tauri ->既存GUI progress fieldへ伝播し、準備済みメモリ欄が常時0 Bではなくなるよう修正。

## correctness / regression

- App Contract v1: 変更なし
- `src-tauri -> search-core`直接依存: なし（Facade維持）
- persistent index format: 変更なし
- text-only corpus: baselineとoptimized index directoryが全21 files SHA-256 byte-identical
- binary file: filename search可能、binary raw body markerはcontent searchでhitしない
- hydration byte budget production regression追加
- `.venv2` / `bin` / `obj` preset regression追加

## A/B benchmark

### Mixed corpus

Synthetic: 900 text files + 100 pseudo-PNG files (512 KiB each), 1000 docs total。

| Metric | ContractV1 Perf2 | SafeIngest Perf3 |
|---|---:|---:|
| scan | 3.712 ms | 3.927 ms |
| build | 8,955.825 ms | 28.862 ms |
| bytes hydrated | 56,172,800 | 3,744,000 |
| peak RSS | 881,112 KiB | 10,644 KiB |
| index directory | 375,995,921 B | 487,109 B |

Mixed synthetic build speedup: **310.3x**。Peak RSS: **約98.8%減 / 82.8x小さい**。

この大差は旧版が画像raw bytesをq-gram content index化していたことによる。実画像の圧縮データを本文検索する価値はなく、今回のfilename-only policyが本来の用途に合う。

### Text-only corpus

Synthetic 10,000 text files。

| Metric | ContractV1 Perf2 | SafeIngest Perf3 |
|---|---:|---:|
| build | 107.731 ms | 95.537 ms |
| bytes hydrated | 44,800,000 | 44,800,000 |
| peak RSS | 64,800 KiB | 59,836 KiB |

Text-onlyでも **約11.3%短縮 / 1.128x**、RSS **約7.7%減**。生成indexはbyte-identical。

## Windows gate

Rust 1.97.1 / `x86_64-pc-windows-gnu`:

- search-core check/clippy: PASS
- bridge-core check/clippy: PASS
- Tauri check/clippy: PASS
- Tauri Windows release link: PASS
- generated executable: PE32+ Windows GUI x86-64

Frontend:

- Vitest 15/15 PASS
- TypeScript check PASS
- Vite production build PASS

## 残る実機確認

実Windowsで、以前落ちた画像大量folderを含むrootに対して再indexし、以下を確認する。

1. アプリが終了しない
2. binary/imageはfilename検索で見つかる
3. binary bodyはcontent検索対象にならない
4. prepared memoryがboundedに推移する
5. 長いcurrent pathでもlayoutが横に崩れない
6. `cache/venv/build/VCS` presetで候補数が大きく減る
