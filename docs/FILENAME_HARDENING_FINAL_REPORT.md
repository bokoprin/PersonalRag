# PersonalRag Filename/Path Search hardening 正式受入れ報告

## 受入れ対象とseries固定

- 対象branch: `codex/filename-hardening`
- measured source commit: `27ee9072bf65e9411f48b4c41b7465f10c460d93`
- formal series: `632feb127d27423e8119e68850379b7a`
- initial remote branch SHA: `930bc4d38276b0d371a3c3df7f36107d3c601172`
- report payload commit: `45223b13603eebf31e1f435ec28d2f7cf2792ec8`
- report metadata finalization commit: payload pointerを確定する後続commit（branch HEADで確認）
- production scope: `PersonalRag.sln`、`src/FilenameSearch.Core`、`src/FilenameSearch.RouteC`、`src/FilenameSearch`、`src/FilenameSearch.Gui`
- 旧Astra、Content Search、LLM、embedding、vector search、rerankingは対象外。

正式seriesは、production source、formal runner、PowerShell測定script、corpus、query set、独立oracle、acceptance rulesを固定した同一lockで実行した。lockは [`FORMAL_SERIES_LOCK.json`](../reports/filename-hardening/FORMAL_SERIES_LOCK.json) に保存している。roundは20、warmupは2、集計は18、seedは`123456`であり、遅いsampleの除外や条件変更は行っていない。

## 電源・実行環境

|項目|記録|
|---|---|
|OS|Microsoft Windows NT 10.0.26200.0|
|CPU|Intel64 Family 6 Model 197 Stepping 2, GenuineIntel|
|RAM|33,682,821,120 bytes|
|.NET SDK|8.0.424|
|物理disk|NVMe `MTFDKBA1T0QGN-1BN1AABGA`、1,024,203,640,320 bytes|
|ready fixed local volume|`C:\`（実機2台目なし）|
|ACLineStatus|`1`|
|battery status|`2`、充電中`false`|
|power scheme|`電源設定の GUID: 381b4222-f694-41f0-9685-ff5bb260df2e (バランス)`|
|thermal throttling|取得値なし|

AC overrideを適用し、AC必須という旧記述より今回の計画を優先した。overrideは電源条件だけに適用し、latency、memory、build、load、GUI、idle、correctnessその他のHARD gateは緩和していない。AC状態をFAIL免除理由には使っていない。

## Gate 0 / regression

[`REGRESSION.json`](../reports/filename-hardening/REGRESSION.json) の7操作はすべてexit code `0`だった。

|操作|elapsed|
|---|---:|
|`dotnet --info`|0.384088 s|
|`dotnet restore PersonalRag.sln`|1.399672 s|
|`dotnet build PersonalRag.sln -c Release --no-restore`|4.170563 s|
|FilenameSearch.Tests|7.928661 s|
|FilenameSearch.Gui.Tests|1.049773 s|
|supplemental Filename E2E|81.429175 s|
|supplemental Idle|13.6900675 s|

build、unit、GUI、E2E、Idle補助回帰にcompile failureは残っていない。各stdout/stderrの実体パスはREGRESSION.jsonに記録した。

## 1M corpus / core

[`CORE_1M.json`](../reports/filename-hardening/CORE_1M.json) の実filesystem corpusは1,000,008 entries、logical bytesは110,592,884,736 bytes（要求1,000,000 entries、logical 100GiB以上）だった。日本語、NFC/NFD、mixed case、duplicate、deep path、wildcard、hard link、0-hit/common/rare/1-char/2-char/full-path queryを含む。hard linkは異なるexact pathで同一FileKey、NFC/NFDはdistinct exact pathのままopen可能だった。

|指標|実測|HARD|
|---|---:|---:|
|FP / FN|0 / 0|0 / 0|
|query p50 / p95 / p99 / max|0.0088 / 1.429 / 3.1409 / 5.0383 ms|20 / 50 / 100 ms|
|worst class p95|common 5.0383 ms|50 ms|
|Ready core private|1,048,977,408 bytes|1,073,741,824 bytes|
|persistent filename data|251,003,750 bytes|1,073,741,824 bytes|
|initial build|29.7522552 s|60 s|
|existing base load|0.6426347 s|1.5 s|
|existing catch-up|2.2292092 s|5 s|

## Sustained search

[`SUSTAINED_SEARCH_1M.json`](../reports/filename-hardening/SUSTAINED_SEARCH_1M.json) はforced GCなしで10,000 mixed queriesを連続実行した。p50 `0.0033 ms`、p95 `1.1427 ms`、p99 `2.6895 ms`、max `11.1489 ms`、worst class common p95 `4.0227 ms`、used scan `0`。Gen0/1/2 GC deltaは`369/1/0`で、privateは1,474,113,536から1,475,686,400 bytesだった。

## Post-update / Route C

[`POST_UPDATE_1M.json`](../reports/filename-hardening/POST_UPDATE_1M.json) はupdates-0、1、100、1000、compaction-during、compaction-after、compaction-after-reconcileの7状態を、各20 rounds（warmup 2、measured 18）で実行した。全状態でFP/FNは0、rareの`UsedScan`はfalse、passはtrueだった。

|状態|p50 ms|p95 ms|p99 ms|max ms|
|---|---:|---:|---:|---:|
|updates-0|0.0093|3.6709|4.8134|7.4081|
|updates-1|0.0092|2.5994|6.0531|13.4585|
|updates-100|0.0076|1.1580|2.3640|2.4981|
|updates-1000|0.0044|1.4478|1.7136|3.0933|
|compaction-during|0.0076|1.9806|3.8687|12.4461|
|compaction-after|0.0087|2.6067|3.5155|3.5428|
|compaction-after-reconcile|0.0030|1.1017|2.2859|3.1948|

全状態の最大p95は`3.6709 ms`、最大p99は`6.0531 ms`。immutable base、indexed delta、tombstone、exact verificationのoverlayで検索結果を維持し、単一更新後に全base scanへ戻っていない。

## Write amplification / compaction

[`WRITE_AMPLIFICATION.json`](../reports/filename-hardening/WRITE_AMPLIFICATION.json) は1M baseにsingle updateを100回適用した。manifestは631 bytesのまま、delta appendは74,385 bytes、full base rewrite deltaは0、base generationは3000で不変だった。threshold到達時のcompactionはpost-update、churnで観測し、atomic publish、restart後の結果、tombstone、old generation cleanupを確認した。

## Churn / restart / directory

- [`CHURN.json`](../reports/filename-hardening/CHURN.json): create、modify、rename、move、delete各10,000、mixed 1,800秒、storm 4,269,328 events。convergence p95 `137.9999 ms`、search p95/p99 `0.1095/0.1983 ms`、private `811,651,072 bytes`、queue max `8192`、saturation `1198`、reconcile `3`、compaction/rewrite `11/11`、最終oracle一致。
- [`RESTART_MODIFY.json`](../reports/filename-hardening/RESTART_MODIFY.json): 1M rootで停止中に同一path/nameのsizeとmtimeを変更し、catch-up `3003.2975 ms`、size/mtimeとも期待値一致。
- [`DIRECTORY_TREE.json`](../reports/filename-hardening/DIRECTORY_TREE.json): 10,000 child treeのcreate、rename、move、recursive deleteを実施。reparse pointを除外し、feedのAdded/Updated/Removed/Renamed/Moved/Reconciled、identity、generation、empty reconciled batchを確認し、最終needle/childrenは0。

## Persistence integrity

[`PERSISTENCE_INTEGRITY.json`](../reports/filename-hardening/PERSISTENCE_INTEGRITY.json) はwriter lease、dirty shutdown/torn delta tail、manifest/base index/base metadata/delta payloadの1-byte corruption、partial temp、root identity mismatch、normalizer version mismatchを個別に実施した。すべてfail-safe recovery/rebuildへ移行し、silent wrong resultを返さないこと、second writer拒否を確認した。

## Multi-volume / inaccessible

[`MULTI_VOLUME.json`](../reports/filename-hardening/MULTI_VOLUME.json) はready fixed local volumeが1台（`C:\`）のため、許可された唯一の`NOT_RUN_NO_SECOND_FIXED_VOLUME`となった。synthetic 2-volume federationではunified result、FileKey collision、store self-exclusion、cross-volume generation loopをすべてpassした。Network/removableを勝手に含めていない。

[`INACCESSIBLE.json`](../reports/filename-hardening/INACCESSIBLE.json) は安全なACL fixtureを作れず、`IdentityNotMappedException: Some or all identity references could not be translated.` を`informational_limitation`として記録した。これは正式NOT_RUNではなく、completion/passを妨げない。既存HARD項目は格下げしていない。

## Actual WPF GUI / idle

- [`GUI_ACTUAL_STARTUP.json`](../reports/filename-hardening/GUI_ACTUAL_STARTUP.json): Release `FilenameSearch.Gui.exe`をfresh child processで10回起動。startup p95 `1484.5426 ms`（HARD 2,000 ms）、max private `548,978,688 bytes`、全exit code 0。
- [`GUI_WARM_INPUT.json`](../reports/filename-hardening/GUI_WARM_INPUT.json): 20 rounds/2 warmup/18 measured。p50/p95/p99/max `5.5172/52.2574/66.6436/70.0077 ms`、private `1,106,882,560 bytes`。
- [`GUI_LIVE_REFRESH.json`](../reports/filename-hardening/GUI_LIVE_REFRESH.json): 実WPF bindingの再入力なし更新をcreate/rename/deleteで`191.9948/189.5218/185.8956 ms`に観測。Added/Renamed/Removed traceとrowsを確認。
- [`IDLE_10M.json`](../reports/filename-hardening/IDLE_10M.json): loaded stateで600秒。平均CPU `0.0001627604%`、private after `1,519,742,976 bytes`、generation `4795 -> 4795`、delta `859599 -> 859599`、store bytes不変。

## 無効化seriesと修正履歴

失敗は条件を緩和せず、raw確認、原因分析、最小修正、focused test、full regression、source commit更新、lock更新、全formal再実行の順で処理した。履歴は [`FORMAL_SERIES_INVALIDATIONS.json`](../reports/filename-hardening/FORMAL_SERIES_INVALIDATIONS.json) に記録している。主な修正は次のとおり。

- `676103e8` / `c305e85f`: metadata parallelism、compact loader、ExactTable構築のピークを抑制。
- `2cd4bfad`: seriesごとのtask-owned fixture/store初期化。
- `d680b9b4`、`60fe9cfc`、`d4be06b1`: update収束、quiet window、compaction測定を安定化。
- `9670c8cd`: delta overlayの割り当てを削減。
- `aac05e57`: live refreshでreparse pointを除外。
- `fdcb8575`: churn後のmemory安定化とdirectory follow-up reconcile。
- `27ee9072`: idle計測前のmemory安定化。

## 完了判定とartifact

[`FINAL_FILENAME_HARDENING.json`](../reports/filename-hardening/FINAL_FILENAME_HARDENING.json) と [`FINAL_FILENAME_HARDENING_ACCEPTANCE.json`](../reports/filename-hardening/FINAL_FILENAME_HARDENING_ACCEPTANCE.json) は同じseries `632feb...`を参照し、`evaluation_complete=true`、`pass=true`。許可されたNOT_RUNは`NOT_RUN_NO_SECOND_FIXED_VOLUME`だけで、inaccessibleはinformational limitationとして分離した。残課題はない。

lockにproduction executable SHA256、runner/script/generator、corpus/query/oracle、normalizer、threshold/acceptance hash、AC状態、power scheme、measurement configurationを保存した。report JSONにはsource SHA、report HEAD、実行コマンド、exit code、elapsed、stdout/stderr path、raw metrics、thresholdを付与している。

正式受入れ後にContent Search等へ作業範囲を拡張していない。
