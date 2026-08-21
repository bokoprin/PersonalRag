# PersonalRag GUI + Portable Search Core — Safe Ingestion / Performance Pass 3

## Safe Ingestion / Performance Pass 3（今回の追加）

- 画像・実行ファイル・archive等は**filename/pathのみindex**し、raw binary bodyは読まない
- hydrationをfile countだけでなく**aggregate bytesでもbounded**化（GUI policy: 32–128 MiB）
- text normalizeをin-place化し、full-size temporary bufferを削減
- Windows小ファイルtreeはaverage sizeに応じてhydration workerを最大8まで利用
- venv presetに`.venv2`等、build presetに`bin/obj/.vs/debug/release/cmake-build-*`等を追加
- 長いcurrent pathはellipsis表示し、layout横伸びを防止（全文はtooltip）
- hydration中のprepared bytesを既存GUI progressへ接続

Synthetic mixed corpusでは旧版がbinary contentをindexしていたケースに対し、build **8.96 s -> 28.9 ms**、peak RSS **881 MiB -> 10.4 MiB**。text-only 10k corpusでも **107.7 ms -> 95.5 ms**で、生成indexはbyte-identicalです。

詳細は `SAFE-INGEST-PERF3-REPORT.md` を参照してください。

このパッケージは、既存PersonalRag GUIとPortable Search Coreの完全接続版を基準に、**大規模rootで残っていたbridge側の重複I/O・同期・検索後処理コストを削減した高速化版**です。

GUIの見た目、Portable Search Coreのpersistent index format、Q2/POS sidecar format、query semanticsは変更していません。

## Progress Fix（今回の追加）

Performance Pass 1を基準に、index rebuild中の進捗が長時間更新されず最後に突然100%になる問題を修正しています。

- file hydration中も **128 files または 100 ms** ごとにbackend progressを更新
- parallel hydrationはworkerから **64 files単位** で結果を返し、progress更新のためにsegment/batchを小さくしない
- hydrate中に`processed_files` / `bytes_read` / `current_path`が動く
- cancel flagをhydration worker内でも確認し、長い読込中のcancel応答を改善
- 全ファイル処理後もQ2/POS/verify/publish中はactive jobを **99%** に留め、完了前に100%表示しない
- terminal state (`completed` / `cancelled` / `failed`) をphase表示の正本にし、staleな`publishing`表示を防止
- progress付きparallel hydrationを共通化し、単一/複数workerの処理を局所リファクタリング

Portable index format、segment size、Q2/POS format、query semanticsは変更していません。


## 今回の高速化

### 1. Scanner hot path
- custom relative path除外を使わない通常ケースでは、filter_entryごとのrelative-path文字列生成を省略
- parallel scanの「現在path」更新を全entryのMutex lockからprogress sampling時だけに変更
- worker-local 4096件batch mergeは維持
- scan時に取得したsize / modifiedを後段へ引き継ぐ

100k-file synthetic tree（80k採用 + 20k除外）parallel scanの中央値:

- 変更前: **50.250 ms**
- 変更後: **46.615 ms**
- **1.078x / 約7.2%短縮**

### 2. Scan → Portable build zero-rework handoff
GUI scannerがすでに取得済みの

- path
- portable display path
- file size

を`DiskPathInput`としてPortable Coreへ渡します。

これによりbuild hydration時の**再metadata取得 + fileごとのcanonicalize**を省きます。read error等でskipされた場合でも、`source_indices`でGUI catalog metadataを正しいdocument IDへ再整列します。

20k files / 4 segments base build中央値:

- 旧path API: **58.554 ms**
- fast metadata-aware API: **41.965 ms**
- **1.395x / 約28.3%短縮**

旧経路と新経路の生成index directoryはA/B各runで**byte-identical**です。

### 3. GUI search metadata cache + allocation reduction
- catalogにscan時の`size_bytes` / `modified_ns`を保存
- search結果生成やsize/modified sortで候補ごとの`fs::metadata()`を省略
- Match Case OFFのplain / Whole Words判定で、candidateごとのASCII lowercase文字列コピーを廃止し、byte比較へ変更
- 旧catalogはmetadata arraysが無い場合、自動的に従来のfilesystem metadata fallbackを使います

20k-hit query / size降順 / limit=100 の中央値:

- 従来metadata再取得: **25.446 ms**
- catalog metadata: **7.177 ms**
- **3.545x / 約71.8%短縮**

## 既存GUIで接続済み

### 検索
- ファイル名 / パス
- 「パスを含めて検索」
- ファイル内容
- Match Case
- Whole Words
- Regex
- 拡張子 filter
- Scope
- Sort: 名前 / パス / サイズ / 更新日時 / 種類
- 昇順 / 降順
- Search v1 = eager `PersistentIndex`
- Search v2 = lazy `LazyPersistentIndex`
- ヒット周辺表示
- 新しい検索入力時の旧検索キャンセル

### Index
- 対象root / 最大bytes
- Scanner: Auto / WalkDir / Windows Native
- cache / venv / node_modules / build / VCS 除外
- `.gitignore` 尊重
- カスタムglob
- 再index / キャンセル
- 「今すぐ同期」はgeneration互換時に差分更新を優先し、USN fast pathが利用できる場合は変更pathだけを処理
- 「再構築」は明示的なPortable Core full rebuild

## 100万ファイル級の経路

1. filesystemを1回walkし、その場で除外
2. scan中はpath + metadataのみ保持（全本文は保持しない）
3. parallel scannerはworker-local batchで共有listへmerge
4. stable path orderへsort
5. scan metadata付き`DiskPathInput`をPortable Coreへhandoff
6. 本文はbounded batchでhydrate
7. immutable segmentをpipeline build
8. Q2 / POS1 / POS2+3 / verify
9. GUI catalogをstream write
10. atomic publish

ファイル件数が増えても、全ファイル本文を同時にRAMへ載せません。

## Windows対応

Rust 1.97.1 `x86_64-pc-windows-gnu` で次を確認済みです。

- search-core `cargo check --all-targets`: PASS
- search-core clippy `-D warnings`: PASS
- search-core Windows test executables link: PASS (PE32+ x86-64)
- bridge-core `cargo check --all-targets`: PASS
- bridge-core clippy `-D warnings`: PASS
- bridge-core Windows test executables link: PASS (PE32+ x86-64)

Tauri GUIは従来版がWindows実機で起動・index作成・日本語/本文/filename検索まで確認済みです。今回追加したsession cache / USN fast pathは`x86_64-pc-windows-gnu`でTauri `check` / `clippy -D warnings`とBridge Windows test executableのlinkまで確認しています。USN fast pathの実NTFS journal読取はWindows実機での最終acceptance対象です。

## Windowsでの起動

ルートの`Build-And-Run.cmd`を実行してください。

1. frontend `npm ci` / test / build
2. Portable Search Core fmt / clippy / test
3. GUI bridge fmt / clippy / test (`--locked`)
4. Tauri fmt / clippy / check
5. Windows release build
6. GUI起動

成功時:

`WINDOWS_GUI_OPTIMIZED_BUILD_PASS`

## Background indexing / Windows USN fast path

「今すぐ同期」は次の順に高速経路を試します。

1. NTFS上で有効な`change-tracker-v1.json`があり、USN checkpoint / generation / scopeが一致していればUSN Change Journalを読む
2. 変更pathだけをhydrateし、既存generationへincremental updateする
3. USNを利用できない、journal reset/wrap、directory namespace変更、hardlink/reparse、追跡map不整合などではfull metadata scanへ安全にfallbackする
4. full metadata scan後も、catalog差分が小さければ本文は変更ファイルだけを再indexする

Windows USN fast pathはNTFS volumeのChange Journal APIを使います。アプリ側でUAC昇格は行わないため、APIを開けない実行権限では自動的に従来のmetadata scanへfallbackします。常駐watcher / `ReadDirectoryChangesW` はまだ追加していません。

full scan時にはDirectory File ID -> relative path mapとUSN checkpointを保存し、次回syncでfile create/modify/delete/renameをpathへ復元します。USN eventがあるupsertはsize/mtimeが同一でも再indexするため、metadata-only差分より強い変更検出になります。

## App Contract v1 / 疎結合境界

GUIとPortable Search Coreの変更影響を分離するため、接続境界を明示的に固定しました。

```text
frontend
   ↓ App Contract v1 (JSON/DTO)
src-tauri adapter
   ↓ SearchEngine / IndexEngine facade
bridge-core
   ↓ private Portable adapter
search-core
```

- `src-tauri` は `personalrag-portable-search` に直接依存しません。
- Tauriはbridge-core所有の `SearchEngine` / `IndexEngine` だけを利用します。
- wire DTOの正本は `app-contract/v1/contract.json` です。
- Rust/TypeScript双方にcompatibility testがあり、未告知のrequest fieldはRust側で拒否します。
- frontendは起動時に `contract_info` を確認し、contract name/version不一致時は通常初期化を止めます。
- 検索コア内部を変更してもFacadeが維持されればGUI変更は不要です。
- GUIの表示・レイアウトを変更してもApp Contract v1が維持されれば検索コア変更は不要です。

詳細は `APP-CONTRACT-V1.md` を参照してください。

### Facade境界での追加高速化

契約整理と同時に、GUI都合の最適化をsearch-coreへ漏らさずbridge側へ集約しました。

- 非path sortは、candidateがlimitより十分多い場合に全件sortせずTop-K選択後に必要分だけsortします。
  - 50,000 candidate / size降順 / limit=2,000 のA/B中央値: **17.680 ms → 13.794 ms**（約22.0%短縮、1.282x）。
- ヒット周辺表示は最大100回のTauri IPCを `snippets_batch` 1回へ集約し、bridge内で最大4 workerのbounded parallel readを行います。
  - 80ファイルの内部A/B中央値: **83.102 ms → 22.142 ms**（約73.4%短縮、3.753x）。

### Contract v1版 Windows gate

Rust 1.97.1 / `x86_64-pc-windows-gnu` で、最終sourceに対して次を確認しています。

- search-core check / clippy: PASS
- bridge-core check / clippy: PASS
- Tauri check / clippy: PASS
- Tauri Windows release link: PASS
- 生成物: PE32+ GUI executable / x86-64
- frontend Contract v1 testを含む **15/15 tests PASS**
- frontend TypeScript check / Vite production build: PASS

Windows実機では `Build-And-Run.cmd` がboundary check → frontend tests/build → core/bridge regression → Tauri locked build → 起動まで実行します。

## Search Core production rollout (2026-08-17)

Perf12 remains the rollback/correctness backend while vNext is rolled out behind a separate Search Core backend switch:

- `perf12`: stable default. Only Perf12 is required for search/build.
- `shadow`: Perf12 serves results; vNext is dual-built/updated and compared on each search. Mismatch/fallback/common-result telemetry is exposed in the GUI.
- `vnext`: vNext serves results when its published generation matches the GUI catalog; otherwise the engine automatically falls back to Perf12.

The backend can be changed from the GUI or with `PERSONALRAG_SEARCH_CORE_BACKEND=perf12|shadow|vnext`. An environment override is process-local and is not persisted to `settings.json`.

A full rebuild in `shadow`/`vnext` creates the vNext durable shadow from the already verified Perf12 generation, so both backends start from the same normalized bytes. Incremental updates publish Perf12 first and then advance vNext best-effort; if vNext becomes stale, read-side generation checks force a Perf12 fallback instead of serving mismatched data.

Windows native acceptance is the final promotion gate. Run:

```powershell
powershell -ExecutionPolicy Bypass -File scripts\validate-vnext-production-switch-windows.ps1 -LaunchShadow
```

Do not make vNext the persisted default until that script passes on Windows and shadow mode reports zero mismatches during the intended burn-in. Perf12 should remain available as rollback until vNext production telemetry is accepted.

## Three fast paths and build-stage profiling (2026-08-18)

The production search core now includes sort-aware generation first-N, conservative mandatory-literal regex prefiltering with Office extraction-cache verification, adaptive parallel vNext segment publication, and a bounded no-copy retained-hydration path for corpora estimated at 32 MiB or less. Larger roots keep the proven Perf12-snapshot fallback to avoid PRPOS memory/cache pressure.

Set `PR_PROFILE_BUILD=1` when running a build benchmark to emit Perf12 hydration/base/PRPOS and vNext q1/q2/q3/checksum/write stage timings. Add `PR_PROFILE_Q3=1` to opt into the finer vNext q3 build sub-profile (`emit`, radix `prepare/count/prefix/scatter`, `dedup`, `encode`, occurrence count, and unique-pair count). Set `PR_PROFILE_QUERY=1` to emit the vNext query sub-profile on diagnostics-capable content queries and dense generation scans: q3 anchor selection, extra-anchor sampling, posting intersection, exact verification, posting encoding mix, and dense-blob `find_from` call/candidate counts. The normal content-query hot path is separately compiled without detailed profiler timers/counters. See `PersonalRag_SIMD_QUERY_FASTPATH_2026-08-18.md` for the current SIMD query A/B and `PersonalRag_THREE_FASTPATHS_BUILD_PROFILE_2026-08-18.md` for the preceding build-profile data.
