# Content Search Bakeoff Formal Report

## 判定

- Formal series: `content-bakeoff-325a13ec1de1`
- Measured source commit: `325a13ec1de12282a7c1ce9de3f3581689faa061`
- Report payload commit: `03eefedd452a843f135ff9ee4cad5e3125fef78b`
- Working branch: `codex/content-search-bakeoff-measure`
- Initial remote `origin/codex/content-search-bakeoff`: `ec8e099f520be54ca24fc83e16f5d539b143f9b6`
- Formal lock: `reports/content-search-bakeoff/FORMAL_BAKEOFF_LOCK.json`
- Aggregate `evaluation_complete`: `true`（12 raw reports、corpus、update/cancellation aggregateを生成済み）
- Aggregate `pass`: `false`
- Winner: なし
- Secondary: なし
- Rejected: `scan`, `bloom`, `trigram`, `sqlite-fts5`
- Classification: **TUNING_REQUIRED**

全方式が300秒のsearch invocation timeoutに到達したため、条件を緩和してwinnerを作成していません。raw timeoutは正式な性能結果として保持しています。`evaluation_complete=true` は全ケースのterminal raw reportとaggregateが揃ったことを示しますが、§59の全query FP/FN確認、update、cancellation、winner決定を満たさないため、正式受け入れ完了宣言は出していません。

## 固定条件

- Block size: 65,536 chars
- Overlap: 256 chars
- Benchmark: 20 rounds、最初の2 rounds warmup、18 rounds measured
- Query shuffle seed: `123456`
- Search timeout: 300 seconds / invocation
- Backend build timeout: 3 hours
- Formal child timeout: 6 hours
- Corpus generator/oracle/query/threshold/source hashesは `FORMAL_BAKEOFF_LOCK.json` に固定
- formal lock後のsource、runner、query、oracle、corpus、threshold変更なし
- ACは受け入れ条件ではなく記録項目のみ

## Environment

- OS: Windows 11 Home (`10.0.26200`)
- CPU: Intel(R) Core(TM) Ultra 9 285H、16 logical processors
- RAM: 33,682,821,120 bytes
- .NET SDK: `8.0.425`
- ACLineStatus: `2`
- BatteryStatus: `2`, estimated charge `100%`
- Power scheme: バランス (`381b4222-f694-41f0-9685-ff5bb260df2e`)
- Thermal throttling: Win32から直接取得できず、ソフトウェア上の兆候は記録されず

## Corpus

実filesystem上のcorpusを使用し、仮想metadataでは代替していません。

| corpus | entries | searchable bytes | logical source bytes |
|---|---:|---:|---:|
| SOURCE_CONFIG | 500,000 | 5,502,926,848 | — |
| LOG | 20,000 | 21,609,054,208 | — |
| HUGE | 20 | 10,871,635,968 | — |
| **total actual searchable** | **520,026** | **37,983,617,201** | **107,374,182,400 (100 GiB)** |

固定fixtureを含むmanifestのSHA-256は `8440245b3671d7458074d177a8074c1be50cb8e006a175635b264e00846e3647` です。

## Gate 0 / regression

現source commit `325a13e` で再実行し、すべてexit code 0でした。stdout/stderrは `logs/content-bakeoff-gate0-formal-325a13e-current/` に保存しています。

| command | exit | elapsed |
|---|---:|---:|
| `dotnet --info` | 0 | 466.45 ms |
| `dotnet restore ContentSearch.Bakeoff.sln` | 0 | 2,363.04 ms |
| `dotnet build ContentSearch.Bakeoff.sln -c Release --no-restore` | 0 | 1,734.74 ms |
| `powershell -NoProfile -ExecutionPolicy Bypass -File scripts/Run-ContentSearchBakeoff.ps1` | 0 | 4,359.22 ms |

Smokeは4 backendすべてでupdate correctnessを通過しました。formalの検索timeoutとは別のGate 0結果です。

## Formal raw results

`Build` はcorpus enumeration/discovery、metadata収集、backend build、persistence publishを含みます。各行のtimeoutは最初に到達したqueryで、後続のlatency sampleは任意に除外していません。

| corpus/backend | status | build (s) | ready private (MiB) | persistent (GiB) | timeout |
|---|---|---:|---:|---:|---|
| SOURCE_CONFIG / scan | TIMEOUT | 14.94 | 260.8 | 0.00 | search / `CONTENT_(RARE\|COMMON)_KAPPA` |
| SOURCE_CONFIG / bloom | TIMEOUT | 283.91 | 808.0 | 5.24 | search / `CONTENT_(RARE\|COMMON)_KAPPA` |
| SOURCE_CONFIG / trigram | TIMEOUT | 445.26 | 641.7 | 5.13 | search / `CONTENT_(RARE\|COMMON)_KAPPA` |
| SOURCE_CONFIG / sqlite-fts5 | TIMEOUT | 792.93 | 439.0 | 11.11 | search / `CONTENT_(RARE\|COMMON)_KAPPA` |
| LOG / scan | TIMEOUT | 1.24 | 26.0 | 0.00 | search / `ZZ` |
| LOG / bloom | TIMEOUT | 830.06 | 265.4 | 20.28 | search / `ZZ` |
| LOG / trigram | TIMEOUT | 831.72 | 229.9 | 20.20 | search / `ZZ` |
| LOG / sqlite-fts5 | TIMEOUT | 5,497.95 | 47.5 | 41.02 | search / `ZZ` |
| HUGE / scan | TIMEOUT | 0.02 | 9.2 | 0.00 | search / `ZZ` |
| HUGE / bloom | TIMEOUT | 393.27 | 155.8 | 10.20 | search / `ZZ` |
| HUGE / trigram | TIMEOUT | 405.37 | 123.4 | 10.16 | search / `ZZ` |
| HUGE / sqlite-fts5 | TIMEOUT | 1,533.73 | 29.4 | 20.63 | search / `ZZ` |

全12 raw reportの`seriesId`は `content-bakeoff-325a13ec1de1`、`sourceCommitSha`は同一の `325a13ec1de12282a7c1ce9de3f3581689faa061` です。

### Correctness / latency

timeout reportではsearch sampleが完了していないため、FP/FNはraw report上で未確定値、p50/p95/p99/maxおよびworst-class percentileは算出対象なしです。timeoutを成功やzero-latencyとして扱っていません。集計のbackend correctness欄の0は、完了レポートの誤差集計が0であることを示す値ではなく、timeoutで検証できなかった方式をeligibleにしないための集約値です。

### Update / cancellation

各backend/corpusがquery benchmark前にtimeoutしたため、backend-specific `UPDATE_*.json` は `updates:null`、`CANCELLATION.json` は測定対象なしです。update correctness、cancellation p95、post-update stateは未完了であり、受け入れPASSには数えていません。

## Series invalidation

現series作成時に、前series `content-bakeoff-78d8c6c77b7c` を、terminal TIMEOUTの集計完了扱いとtimeout build evidence保持の修正により無効化しました。さらに今回のsource commitではinitial build計測へfilesystem discovery/metadataを含めるrunner修正を固定しました。変更前のraw結果は現seriesへ流用していません。詳細は `FORMAL_BAKEOFF_INVALIDATIONS.json` とgit historyにあります。

## 結論と次の扱い

- `evaluation_complete=true`、`pass=false`。
- 4方式とも正式thresholdを満たすeligible winnerにならなかった。
- timeout回避のためcorpus、query、round、threshold、timeoutを縮小・変更していない。
- performance missは結果として保持し、AC未接続を免除理由にしていない。
- 次に性能改善を行う場合は、現series終了後の別tuning roundとして新source commit・新FORMAL_BAKEOFF_LOCKを作り、4方式を同一条件で最初から再測定する。現seriesのraw結果は再利用しない。
- 今回の範囲ではContent Search bakeoffのproduction統合、Filename/GUI phase、RAG/LLM/embedding/vector searchへの移行は行っていない。

`PERSONALRAG_CONTENT_SEARCH_BAKEOFF_COMPLETE` は、§59の全条件（全query FP/FN、update、cancellation、winner/secondary決定、commit、push、remote HEAD確認）が揃っていないため宣言していません。

\n

## Remote update check

Formal lock後のpush前fetchで確認したSHAは次のとおりです。

- `origin/codex/content-search-bakeoff-measure`: `73999fb4c05afcc46958dc98d5337abd274ef75f`
- `origin/codex/content-search-bakeoff`: `ec8e099f520be54ca24fc83e16f5d539b143f9b6`

remote側にlocal HEADより先行するcommitはなく、formal sourceへ取り込むremote差分はありません。lock後にpull、merge、rebase、force pushは行っていません。report payload commitは `03eefedd452a843f135ff9ee4cad5e3125fef78b`、report metadata commitは `25d7b2df45f8ec255320559dc3b91788caa842ce` です。
\n