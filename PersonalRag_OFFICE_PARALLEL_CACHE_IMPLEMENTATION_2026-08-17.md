# PersonalRag Office Parallel Extraction + Persistent Cache Implementation

Date: 2026-08-17

## 1. 結論

TXT / DOCX / XLSX / PPTX benchmarkで、Officeの主ボトルネックがvNext index生成ではなくZIP/DEFLATE/XML抽出とファイル/part単位の固定費であることを確認したため、Office専用indexを作らず、Bridge側の抽出パイプラインを **bounded parallel extraction + persistent extracted-text cache** へ置き換えた。

実装後は、旧production経路（serial extract -> spool write -> spool reread -> vNext index）に対して、最終benchmarkの全Office条件でcold cacheが同等以上、warm cacheは全条件で高速化した。

特に 1000 files x 4KiB / multipart では:

| Format | Old production | Cold cache | Warm cache | Cold speedup | Warm speedup |
|---|---:|---:|---:|---:|---:|
| DOCX | 183.165 ms | 129.557 ms | 87.702 ms | 1.414x | 2.088x |
| XLSX | 197.040 ms | 159.231 ms | 118.540 ms | 1.237x | 1.662x |
| PPTX | 248.259 ms | 183.558 ms | 89.598 ms | 1.352x | 2.771x |

vNext `.prseg2`のformat/semanticsは変更していない。

## 2. 実装フェーズ A-H

### Phase A - OfficeExtractionService 境界

新規:

- `bridge-core/src/office_cache.rs`
- `OfficeExtractionService`
- `OfficeExtractionConfig`
- `OfficeExtractionRequest`
- `OfficePreparedContent`
- `OfficeExtractionBatchReport`
- `OfficeCacheGcReport`

Office抽出/cacheはBridge側で完結し、Search Coreへは通常テキストと同じprepared content pathを渡す。Perf12/vNextの双方で同一の抽出結果を使用する。

### Phase B - bounded parallel extraction

Office file間を並列化した。

production default:

- `max_workers = min(logical_cpus, 4)`
- extraction memory budget = `clamp(build_memory_budget / 8, 64 MiB, 512 MiB)`
- worker数だけでなく、同時処理するsource file size合計がmemory budgetを超えないよう制御
- completion順ではなく`source_index`順へ戻してdeterminismを維持

1/2/4/8 workerを比較し、8 workerは一部cold workloadで速い一方、小～中規模Officeで揺れ/競合が増えたため、production defaultは4 workerに固定した。

### Phase C - persistent extracted-text cache

cacheはindex generationの外へ配置する。

production layout概念:

```text
app-data/
├ portable-index/
├ office-extraction-cache/
│  ├ objects/
│  │  └ ab/
│  │     ├ <key>.txt
│  │     └ <key>.meta
│  ├ tmp/
│  └ LIVE
└ ...
```

`portable-index`と`portable-index-build-*`は同じ`office-extraction-cache`を共有する。任意の独立index pathでは`<index>.office-cache`を使用し、別index同士を混在させない。

cacheにはSearch Core normalize前のcanonical extracted textを保存する。vNext/Perf12固有の形式は保存しない。

### Phase D - searchable XML fingerprint

cache identityは単純なsize/mtimeではなく、Office ZIP内の **検索対象XML partだけ** から作る。

fingerprint要素:

- `INGESTION_VERSION`
- Office kind
- ZIP entry name
- compression method / flags
- CRC32
- compressed/uncompressed size
- compressed searchable payloadの2x64-bit hash

対象entry:

- DOCX: document / footnotes / endnotes / comments / header / footer
- XLSX: sharedStrings / worksheets
- PPTX: slides / notesSlides

fingerprint pathはZIP central directoryを読み、対象XML payloadだけseek/readする。cache hit判定のためにOffice container全体を`fs::read()`しない。

そのため、画像/動画などmedia-only変更では検索対象XMLが同じならcache keyを維持できる。検索対象XMLが変われば必ずcache miss/re-extractになる。

### Phase E - spool bypass

cacheが利用可能なOfficeでは:

```text
cache .txt
  -> DiskPathInput.content_path
  -> Search Core
```

とし、従来の:

```text
extract
 -> temporary spool write
 -> spool reread
 -> Search Core
```

を省いた。

cache directory/writeが利用できない場合は、正確性を優先して従来のin-memory extraction/spool fallbackへ戻る。

### Phase F - full build / incremental / USN統合

Full build:

1. scan/order確定
2. Office jobsをbatch準備
3. fingerprint/cache lookup
4. missのみbounded parallel extraction
5. cache object publish
6. source orderでDiskPathInput生成
7. Perf12/vNext build
8. build/verify成功後にLIVE publish
9. full build完了時だけcache GC

Incremental / USN:

- Office upsertごとにfingerprintを再計算
- USNが変更を報告した場合、size/mtimeが同じでもfingerprintで検索対象XMLを確認
- media-only変更はcache reuse可能
- searchable XML変更はnew cache key + re-extract
- generation publish/verify成功後だけLIVEを更新
- small incremental hot pathではcache全object GCを実行しない

### Phase G - LIVE + safe cache GC

Default policy:

- soft limit: 2 GiB
- target: 1.6 GiB
- grace: 7 days

GCはLIVEから参照されているcache objectを削除しない。LIVE更新に失敗した場合はGCを実行しない。

cacheはcorrectness sourceではなくacceleratorなので、generation storeと同じpower-loss durabilityは要求しない。cache objectはtempへ完全write後にrenameするが、fileごとの`sync_data()`は行わない。

この判断は実測による。初期実装でcache objectごとに`sync_data()`したところ、200 Office fileのcold buildが旧経路より2～3倍遅くなった。cacheは破損/消失しても再抽出可能なため、過剰なfsyncを削除した。

cache text/metaはread時にsize/checksum/fingerprintを検証し、corruptionはcache missとして再抽出・修復する。

### Phase H - benchmark / hard gates

追加:

- `search-core/examples/office_cache_pipeline_bench.rs`
- `search-core/tests/office_extraction_cache.rs`
- `scripts/generate-office-index-benchmark-corpus.py --media-bytes`

Windows validation harnessにもOffice cache integration testを追加した。

## 3. Correctness / safety hard gates

Search CoreからBridgeの実extractor/cache sourceを直接includeして、offlineでも実コードをcompile/testしている。

新Office cache testsには以下を含む:

- DOCX/XLSX/PPTX legacy extraction
- malformed Office fail-closed
- DEFLATE fixed/dynamic
- searchable XML cache reuse
- media-only change reuse
- searchable XML change -> new key
- parallel output source-order determinism
- LIVE round-trip
- cache text corruption -> miss/re-extract/repair
- cache directory failure -> in-memory extraction fallback
- GC never deletes LIVE object
- production build temp/final cache root sharing
- arbitrary index cache isolation
- legacy extraction vs cached extraction exact text equality
- legacy Office path vs cached Office path generated vNext `.prseg2` byte-identical

Final Search Core regression:

```text
unit                       5 / 5
production oracle          35 / 35
production shadow           1 / 1
Office cache/extractor     14 / 14
durable compaction          6 / 6
durable GC                  5 / 5
durable generation         12 / 12
vNext generation           11 / 11
persistent                  5 / 5
vNext query                 9 / 9
vNext segment              17 / 17
---------------------------------
TOTAL                     120 / 120 PASS
```

Also:

- Search Core `cargo fmt --check`: PASS
- Bridge Rust 2021 `rustfmt --check`: PASS
- Search Core Clippy all-targets `-D warnings`: PASS
- doc tests: PASS
- release examples: PASS
- release `pr_portable`: PASS
- `SELF_TEST_PASS`: PASS
- Python benchmark generator syntax: PASS

## 4. Final benchmark

All timings include Office preparation plus vNext segment build. `old_production` is the previous real path: serial extract -> spool write -> spool reread -> index. `cold_cache` deletes cache before each run. `warm_cache` primes cache, then measures fingerprint/cache-hit/direct-index.

Workers=4.

### 4.1 200 files x 24KiB

Single-part:

| Format | Old production | Cold cache | Warm cache | Cold | Warm |
|---|---:|---:|---:|---:|---:|
| DOCX | 126.901 | 116.649 | 88.983 | 1.088x | 1.426x |
| XLSX | 119.767 | 114.395 | 88.353 | 1.047x | 1.356x |
| PPTX | 119.582 | 111.816 | 91.493 | 1.069x | 1.307x |

Multipart:

| Format | Old production | Cold cache | Warm cache | Cold | Warm |
|---|---:|---:|---:|---:|---:|
| DOCX | 118.779 | 101.282 | 79.926 | 1.173x | 1.486x |
| XLSX | 126.256 | 97.016 | 82.070 | 1.301x | 1.538x |
| PPTX | 141.760 | 107.919 | 82.874 | 1.314x | 1.711x |

### 4.2 1000 files x 4KiB

Single-part:

| Format | Old production | Cold cache | Warm cache | Cold | Warm |
|---|---:|---:|---:|---:|---:|
| DOCX | 134.208 | 124.508 | 70.402 | 1.078x | 1.906x |
| XLSX | 140.255 | 134.304 | 81.751 | 1.044x | 1.716x |
| PPTX | 124.897 | 117.700 | 94.285 | 1.061x | 1.325x |

Multipart:

| Format | Old production | Cold cache | Warm cache | Cold | Warm |
|---|---:|---:|---:|---:|---:|
| DOCX | 183.165 | 129.557 | 87.702 | 1.414x | 2.088x |
| XLSX | 197.040 | 159.231 | 118.540 | 1.237x | 1.662x |
| PPTX | 248.259 | 183.558 | 89.598 | 1.352x | 2.771x |

### 4.3 64 files x 64KiB

Single-part:

| Format | Old production | Cold cache | Warm cache | Cold | Warm |
|---|---:|---:|---:|---:|---:|
| DOCX | 92.093 | 83.306 | 72.241 | 1.105x | 1.275x |
| XLSX | 98.195 | 88.067 | 76.485 | 1.115x | 1.284x |
| PPTX | 89.348 | 78.935 | 70.368 | 1.132x | 1.270x |

Multipart:

| Format | Old production | Cold cache | Warm cache | Cold | Warm |
|---|---:|---:|---:|---:|---:|
| DOCX | 103.140 | 83.573 | 66.967 | 1.234x | 1.540x |
| XLSX | 102.160 | 86.894 | 72.021 | 1.176x | 1.418x |
| PPTX | 111.057 | 86.971 | 73.096 | 1.277x | 1.519x |

### 4.4 media-heavy synthetic

8 files per format, each Office file has ~24KiB searchable text + 2MiB ignored media.

Representative multipart:

| Format | Old production | Cold cache | Warm cache | Cold | Warm |
|---|---:|---:|---:|---:|---:|
| DOCX | 10.929 | 8.978 | 8.318 | 1.217x | 1.314x |
| XLSX | 10.354 | 9.910 | 8.678 | 1.045x | 1.193x |
| PPTX | 13.040 | 11.975 | 7.673 | 1.089x | 1.699x |

このsyntheticはOS page cacheの影響が大きく絶対差は小さいが、media-only bytesをfingerprint/cache reuse対象から外す設計のcorrectnessを確認できた。

## 5. Production behavior

### Cold build

cache missでもOffice file間がbounded parallel化され、成功すればcache objectを直接index inputとして使う。最終benchmarkでは全Office条件で旧production経路と同等以上。

### Warm full rebuild

Office XMLの再DEFLATE/XML extractionを避け、fingerprint + cached extracted text read + indexだけになる。今回のsyntheticでは約1.27x～2.77x高速。

### Incremental / USN

変更Officeのみfingerprintを再評価する。検索対象XMLが同じならcache hit、変われば再抽出。cache GCはsmall delta同期経路に入らない。

### Cache failure/corruption

cacheはacceleratorでありcorrectness sourceではない。cache failure/corruptionはOffice extraction fallback/cache missとして処理し、既存検索を止めない。

## 6. Bridge / Windows validation status

現在のLinux offline環境ではBridgeのCargo dependency `ignore` が保存vendorに存在せず、`cargo test --locked --offline --lib`はsource compile前に以下でBLOCKED:

```text
error: no matching package named `ignore` found
```

したがってBridge全crate compileをLinux PASSとはしていない。

一方:

- Bridge changed Rust source: Rust 2021 rustfmt/parse PASS
- Office extractor/cache source: Search Core integration testとして実compile/Clippy PASS
- Windows validation harnessに以下を追加:
  - `office_cache_reuses_media_only_change_and_refreshes_searchable_xml`
  - existing async shadow / Gate5 / first-N gates

Windows native acceptanceでBridge/Tauri全compile/testを行う。

## 7. Modified source files

- `bridge-core/src/engine.rs`
- `bridge-core/src/extractor.rs`
- `bridge-core/src/lib.rs`
- `bridge-core/src/office_cache.rs` (new)
- `bridge-core/tests/integration.rs`
- `search-core/tests/office_extraction_cache.rs` (new)
- `search-core/examples/office_cache_pipeline_bench.rs` (new)
- `scripts/generate-office-index-benchmark-corpus.py`
- `scripts/validate-vnext-production-switch-windows.ps1`

## 8. Phase status

```text
A OfficeExtractionService boundary      DONE
B bounded parallel extraction          DONE
C persistent extracted-text cache      DONE
D searchable XML fingerprint           DONE
E spool bypass                         DONE
F full/incremental/USN integration     DONE
G LIVE + safe cache GC                 DONE
H cold/warm/media benchmark            DONE
```

Office parallel/cache implementation phase: **100% complete on Linux Search Core + source-level integration**.

Remaining project-wide hard gate is the same as before: Windows native Bridge/Tauri acceptance and production shadow burn-in before vNext default promotion.
