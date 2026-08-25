# CQ3DIR 4方式 read-only prototype benchmark

## Executive summary

- sourceMethod: `generated-temporary`
- current corpus tuple: `sourceFiles=65404`, `processedFiles=65404`, `indexedFiles=65404`, `bytesRead=2790429937`
- source index: **5,326,281,995 bytes / 4.960487 GiB**
- current CQ3DIR: **998,107,292 bytes / 0.929560 GiB**
- correctness validation: **567,107 keys**。current Prefix10と、`fixed8-packed14`、`blocked-delta-64`、`blocked-delta-256`のfound/miss、encoding、posting count、payload offset、payload byte lengthが一致した。

| Representation | CQ3DIR bytes | CQ3DIR reduction | Estimated whole index | Whole-index reduction |
|---|---:|---:|---:|---:|
| current-prefix10 | 998,107,292 | 0.000% | 5,326,281,995 bytes / 4.960487 GiB | 0.000% |
| fixed8-packed14 | 798,488,712 | 20.000% | 5,126,663,415 bytes / 4.774577 GiB | 3.748% |
| blocked-delta-64 | 312,915,720 | 68.649% | 4,641,090,423 bytes / 4.322352 GiB | 12.864% |
| blocked-delta-256 | 303,561,798 | 69.586% | 4,631,736,501 bytes / 4.313641 GiB | 13.040% |

`blocked-delta-64`は約68.6%のCQ3DIR削減を実際のprototypeでも保持したが、random hitはcurrentの **1.842253倍**、mixedは **1.982539倍** だった。容量だけなら有望だが、interactive lookupの速度を犠牲にする。`fixed8-packed14`はCQ3DIR削減が20%に留まる一方、4 workloadすべてでcurrentより速く、次のRust prototype候補として最も安全なシグナルを示した。

このタスクはprototype benchmarkのみであり、production Rust、PRSEG/CQ3DIR/CQ3POST形式、reader/writer、migration、checksum、publish、fail-closed動作を変更していない。

## Provenance

- branch: `perf/autopilot-5round-20260823-192255`
- source/profile HEAD: `3e9a33882fecfc90d99e89634bcfe92b62961f6f`
- selected index: `C:\Users\bokop\AppData\Local\PersonalRag\cq3dir-prototype\cq3dir-prototype-20260825-214751\profile\warm-measured`
- task root: `C:\Users\bokop\AppData\Local\PersonalRag\cq3dir-prototype\cq3dir-prototype-20260825-214751`
- source root: `C:\Program Files\`（read-only）
- independent analyzer artifact: `...\cq3dir-prototype-20260825-214751\cq3dir-analysis.json`
- benchmark JSON: `reports/cq3dir-prototype-benchmark-20260825-215140.json`
- source format: `PRSEG005`, directory kind: `Prefix10`, segments: 14
- Warm generation: `warm-prime` 1回 + `warm-measured` 1回のみ
- free space before generation: **604,156,289,024 bytes / 562.664 GiB**（要求20 GiB以上）
- warm-prime tree SHA-256: `4dd07e85ef8dac6ea34fb416a0d5e9385f6b08574204981e91bc08691bb17469`
- warm-measured tree SHA-256: `4dd07e85ef8dac6ea34fb416a0d5e9385f6b08574204981e91bc08691bb17469`

### Frozen benchmark config

```json
{
  "accelerationProfile": "balanced",
  "buildWorkers": 4,
  "hydrationBatchBytes": 134217728,
  "hydrationWorkers": 4,
  "maxFileBytes": 33554432,
  "scannerMode": "auto",
  "segmentDocs": 5000
}
```

profile schema v2、frozen config完全一致、warm-prime/measuredのenvironment tuple一致、両tree完全一致、両verify成功、14 PRSEG、temporary fileなしを確認した。前回のCQ3DIR監査とsourceFiles、processedFiles、indexedFiles、bytesReadが一致するため、`corpusChangedSincePriorAudit=false` とした。

| 項目 | 値 |
|---|---:|
| discoveredEntries | 73,657 |
| discoveredFileEntries | 65,489 |
| discoveredDirectoryEntries | 8,168 |
| selectedFiles | 65,404 |
| selectedBytes | 15,324,549,070 |
| selectedContentFiles | 40,035 |
| selectedContentBytes / bytesRead | 2,790,429,937 |
| warm-measured verifyWallMs | 6,977.1814 |

## Correctness

各segmentでcurrent Prefix10をreferenceとして、候補をtiming前に比較した。検証対象のmetadataは次の5つである。

- found / miss result
- encoding
- posting count
- payload offset
- payload byte length

validation key classは次のとおりである。

- 全14 segmentの、空でないprefixごとのfirst / middle / last key
- deterministic random existing keys（segmentごとに最大20,000 keys）
- deterministic random missing keys（segmentごとに20,000 keys）

| segment | entries | validation keys |
|---|---:|---:|
| seg-00000.prseg | 10,545,770 | 40,668 |
| seg-00001.prseg | 10,344,150 | 40,663 |
| seg-00002.prseg | 300,208 | 40,004 |
| seg-00003.prseg | 6,955,094 | 40,665 |
| seg-00004.prseg | 12,152,137 | 40,680 |
| seg-00005.prseg | 1,498,136 | 40,544 |
| seg-00006.prseg | 11,854,352 | 40,672 |
| seg-00007.prseg | 2,593,961 | 40,605 |
| seg-00008.prseg | 11,575,254 | 40,668 |
| seg-00009.prseg | 10,208,494 | 40,676 |
| seg-00010.prseg | 144,183 | 39,286 |
| seg-00011.prseg | 9,404,640 | 40,680 |
| seg-00012.prseg | 7,303,600 | 40,659 |
| seg-00013.prseg | 4,929,311 | 40,637 |
| **total** | **99,809,290** | **567,107** |

結果は `current-prefix10`、`fixed8-packed14`、`blocked-delta-64`、`blocked-delta-256` の全候補でPASS。correctness mismatchはなく、誤ったcandidateのtimingは採用していない。各workloadのcandidate checksumもcurrentと一致した。

## Size cross-check

独立に実行した `scripts/analyze-cq3dir-readonly.ps1` のJSONとprototypeのmanaged-memory encoded byte countを比較した。

| Representation | Analyzer bytes | Prototype bytes | Exact match | CQ3DIR reduction | Whole-index reduction |
|---|---:|---:|---|---:|---:|
| current-prefix10 | 998,107,292 | 998,107,292 | true | 0.000% | 0.000% |
| fixed8-packed14 | 798,488,712 | 798,488,712 | true | 20.000% | 3.748% |
| blocked-delta-64 | 312,915,720 | 312,915,720 | true | 68.649% | 12.864% |
| blocked-delta-256 | 303,561,798 | 303,561,798 | true | 69.586% | 13.040% |

全representationについて、次の式も一致した。

`EstimatedWholeIndexBytes = IndexBytes - CurrentCq3DirBytes + DirectoryBytes`

## Lookup benchmark

条件は `QueriesPerWorkload=16384`、`Repeats=5`、`BatchSize=256`、`Seed=20260825`。表示するratioは同じworkloadのcurrent medianに対する比率で、1.000未満がcurrentより速い。

### hit-random

| Representation | median ns/op | ratio vs current | batch p50 | batch p95 | batch p99 | M lookups/s |
|---|---:|---:|---:|---:|---:|---:|
| current-prefix10 | 900.027 | 1.000000 | 885.547 | 1,491.055 | 2,061.039 | 1.111077 |
| fixed8-packed14 | 828.439 | 0.920460 | 833.398 | 1,411.328 | 1,877.113 | 1.207089 |
| blocked-delta-64 | 1,658.078 | 1.842253 | 1,652.734 | 2,261.738 | 2,739.859 | 0.603108 |
| blocked-delta-256 | 4,784.955 | 5.316454 | 4,802.539 | 6,174.863 | 7,221.027 | 0.208988 |

### miss-random

| Representation | median ns/op | ratio vs current | batch p50 | batch p95 | batch p99 | M lookups/s |
|---|---:|---:|---:|---:|---:|---:|
| current-prefix10 | 563.647 | 1.000000 | 565.625 | 975.879 | 1,308.859 | 1.774159 |
| fixed8-packed14 | 545.349 | 0.967536 | 532.422 | 946.504 | 1,289.328 | 1.833688 |
| blocked-delta-64 | 1,302.652 | 2.311111 | 1,263.281 | 1,685.996 | 2,178.617 | 0.767665 |
| blocked-delta-256 | 3,269.174 | 5.800034 | 3,361.719 | 4,644.941 | 5,383.152 | 0.305888 |

### mixed-random-50

| Representation | median ns/op | ratio vs current | batch p50 | batch p95 | batch p99 | M lookups/s |
|---|---:|---:|---:|---:|---:|---:|
| current-prefix10 | 766.412 | 1.000000 | 753.711 | 1,271.484 | 1,588.918 | 1.304781 |
| fixed8-packed14 | 743.683 | 0.970343 | 714.844 | 1,260.547 | 1,702.977 | 1.344659 |
| blocked-delta-64 | 1,519.443 | 1.982539 | 1,470.117 | 1,994.199 | 2,511.938 | 0.658136 |
| blocked-delta-256 | 4,175.693 | 5.448363 | 4,133.203 | 5,268.828 | 6,182.539 | 0.239481 |

### hit-sorted-locality

| Representation | median ns/op | ratio vs current | batch p50 | batch p95 | batch p99 | M lookups/s |
|---|---:|---:|---:|---:|---:|---:|
| current-prefix10 | 709.854 | 1.000000 | 689.063 | 1,180.508 | 1,431.473 | 1.408740 |
| fixed8-packed14 | 678.238 | 0.955461 | 670.898 | 1,136.387 | 1,358.180 | 1.474409 |
| blocked-delta-64 | 1,438.870 | 2.026994 | 1,451.953 | 1,830.098 | 2,239.309 | 0.694990 |
| blocked-delta-256 | 4,403.641 | 6.203585 | 4,528.516 | 5,569.531 | 6,611.449 | 0.227085 |

## Prototype encode cost

これはcurrent CQ3DIRをmanaged memory上で候補表現へ変換する時間であり、production builderへ統合したbuild costではない。

| Representation | prototype encode ms |
|---|---:|
| fixed8-packed14 | 3,894.623 |
| blocked-delta-64 | 7,411.087 |
| blocked-delta-256 | 7,083.801 |

`blocked-delta-256`はsizeがわずかに小さいが、今回のprototype変換時間はblocked64より短かった。これはmanaged C# prototypeの変換処理の結果であり、Rust builderの並列化・allocation・cache挙動を直接表さない。

## Trade-off matrix

| Representation | capacity saving | random-hit ratio | miss ratio | mixed ratio | locality ratio | complexity | migration / format risk |
|---|---:|---:|---:|---:|---:|---|---|
| current-prefix10 | 0.000% | 1.000 | 1.000 | 1.000 | 1.000 | 現行 | なし（baseline） |
| fixed8-packed14 | CQ3DIR 20.000% / whole 3.748% | 0.920 | 0.968 | 0.970 | 0.955 | 低〜中 | 中。14-bit count上限、reader/writer、format versionの協調が必要 |
| blocked-delta-64 | CQ3DIR 68.649% / whole 12.864% | 1.842 | 2.311 | 1.983 | 2.027 | 中〜高 | 高。checkpoint、varint decode、malformed/truncated fail-closed、format migrationが必要 |
| blocked-delta-256 | CQ3DIR 69.586% / whole 13.040% | 5.316 | 5.800 | 5.448 | 6.204 | 中〜高 | 高。decode windowが大きく、interactive latencyへの影響が明確 |

## Recommendation for next experiment

**`RUST_PROTOTYPE_FIXED8`**

- `fixed8-packed14`は独立analyzerとprototypeのサイズが完全一致し、CQ3DIRを20.000%削減した。
- 567,107 validation keysで、found/missと5つのmetadata項目が全候補で一致した。
- hit、miss、mixed、sorted-localityの全workloadでmedian ratioがそれぞれ0.920460、0.967536、0.970343、0.955461となり、currentより遅くならなかった。
- blocked64はCQ3DIR削減が68.649%と大きいが、random hit 1.842253倍、mixed 1.982539倍であり、先にRust側でlookup costを検証する価値はあるものの、interactive desktop searchの第一候補にはしなかった。
- blocked256はblocked64よりCQ3DIRが0.937ポイントしか小さくならない一方、random hit 5.316454倍、sorted locality 6.203585倍だった。
- 次のRust prototypeでは、fixed8のbyte layout、既存checksum/publish/fail-closed/determinism/corruption gateとの接続を検証する。production formatへの採用やmigrationはこのタスクでは行わない。

## Execution notes and repairs

- 指定の外側PowerShell wrapperで最初に起動したときは、`Add-Type`の一時コンパイルが構文エラーになり、benchmark本体へ到達しなかった。index metadataは不変で、JSONは生成されなかった。
- 同じ3つのC#ソースをPowerShell 7の現在プロセスで再コンパイルしてPASSを確認し、同じ指定パラメータでbenchmark scriptを直接実行した。
- prototype C#、benchmark script、production Rustにはcompile/runtime repairを適用していない。実験設計・workload・seed・repeat数は変更していない。
- 直接実行後のprototypeは`CQ3DIR_PROTOTYPE_BENCHMARK_COMPLETE`を出力し、correctness後に全16 timing結果を生成した。外側のPowerShellでは`$LASTEXITCODE`が未設定だったため後処理の終了コード表示だけがnullになったが、JSON、sentinel、入力metadata検査を正とした。

## Limitations

- これはmanaged C# prototype microbenchmarkであり、Rust/mmap production lookup latencyの最終証拠ではない。
- source PRSEGは`FileAccess.Read`で開き、候補表現はmanaged memoryだけに構築した。PRSEG/CQ3DIR/CQ3POSTのbyte列は変更していない。
- JIT、GC、Windows scheduler、Defender、cache、CPU frequencyの揺らぎは残る。warmup、5 repeats、representation order rotation、repeat間GCで影響を抑えた。
- prototype encode msはcurrent directoryからの変換時間であり、production build concurrencyやdurable write時間を含まない。
- サイズ差とlookup比のtrade-offを示す実験であり、production formatの永続採用を決めるものではない。

## Temporary-index cleanup

- generated source task root: `C:\Users\bokop\AppData\Local\PersonalRag\cq3dir-prototype\cq3dir-prototype-20260825-214751`
- generated bodies: `profile\warm-prime`、`profile\warm-measured`
- benchmark JSONとこのMarkdown、task-owned analyzer JSON、profile summary/tree/log metadataを保存・検証した後、上記2つのindex bodyだけを削除した。
- 既存index、archive、`C:\Program Files\`、repository sourceは削除しない。
- cleanup結果: `temporaryIndexesDeleted=true`。task root、profile summary、tree JSON、raw log、analyzer JSONは保持した。
