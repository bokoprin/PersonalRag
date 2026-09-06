# Codex 指示書 — Filename Hardening 正式再測定と完走

## 0. 最上位指示

この文書を正本として、`codex/filename-hardening` の Filename/Path Search hardening を
**正式再測定まで完走**すること。

途中経過を最終回答にしてはいけない。

この文書のHARD条件を1つでも満たさない場合:

```text
原因分析
-> 最小修正
-> focused test
-> full regression
-> 再測定
```

を自律的に繰り返すこと。

`PERSONALRAG_FILENAME_PHASE_COMPLETE`

を再宣言できるのは、全HARD条件を同一source commitでPASSした場合だけ。

内容検索へ進んではならない。

---

# 1. 正本

Canonical solution:

`PersonalRag.sln`

Canonical production path:

```text
src/FilenameSearch.Core
src/FilenameSearch.RouteC
src/FilenameSearch
src/FilenameSearch.Gui
```

Architecture:

`docs/FILENAME_ENGINE_ARCHITECTURE.md`

Content future boundary:

`docs/CONTENT_ENGINE_BOUNDARY.md`

USN decision:

`docs/NTFS_USN_DECISION.md`

旧:

```text
src/Astra.*
PersonalRag.Astra.sln
```

は今回のproduction filename測定対象にしない。

---

## 1.1 Correctness測定境界

`src/FilenameSearch.RouteC/RouteCEngine` は production では conservative candidate generator。
その `Search()` 戻り値を直接oracleと比較してはいけない。

正式correctness/performance測定の検索経路は必ず:

```text
PersonalRag.FilenameSearch.FilenameSearchEngine
または
IFilenameCatalog / FileSystemCatalog / MultiVolumeCatalog
```

を通す。ここでexact filesystem metadataに対する最終verificationが行われる。
旧 `bench/runner` のRoute C direct benchmarkはarchitecture bake-off履歴であり、hardening後production acceptanceの正本ではない。

# 2. 今回必ず検証する設計invariant

## 2.1 exact path identity

- `FilenameRecord.Name`
- `FilenameRecord.FullPath`

はfilesystem exact spelling。

NFC/NFDの2pathを同一文字列へ変換しない。

検索keyだけNFC/casefoldする。

HARD:

- NFC/NFD相当名を2つ作成可能なfixtureで両方検索できる
- 戻ったFullPathがdistinct
- 両FullPathを実際にopen/stat可能

## 2.2 Wildcard

wildcardはwhole-string globではなくsubstring semantics。

HARD vectors:

```text
target: my_report_2026.xlsx_backup
query : report_*.xlsx
=> MATCH

target: abcXYZdef
query : X?Z
=> MATCH

target: alpha
query : *
=> MATCH
```

Filename / FullPath両方でoracle一致。

## 2.3 Unicode full case fold

最低限:

- ß / SS
- ẞ / ss
- ﬃ / ffi
- final sigma
- Kelvin sign
- dotted I vectors

を固定testへ追加。

oracleはproduction implementationを呼ばず独立実装/fixture expectedを使う。

## 2.4 FileKey

Windows NTFS:

```text
VolumeId + NativeId
```

rename/move前後で同一fileのFileKeyが不変。

hard link fixtureでは同一FileKeyを複数pathが共有可能であること。

FileKey duplicateをinvalid扱いしてはいけない。

## 2.5 Snapshot + typed change feed

初期 `IFilenameCatalog.GetSnapshot()` と、その後の Added / Updated / Removed / Renamed / Moved を検証。

HARD:

- snapshot Recordsがcatalog全件と一致
- single-volume snapshotの `SourceGenerations[VolumeId]` がsource generationと一致
- batch `SourceId` / `SourceGeneration` が正しい
- data change batchごとにsource generationが単調増加
- data変更0件のreconcileでも `Reconciled=true` 通知を受け取れる
- snapshot取得とfeed購読の間にgapがあればsource generation差で検出可能

Content Engineが別watcherを必要としない情報量があること。

## 2.6 Post-update search architecture

1M baseをload。

rare/common/wildcard queryを測定。

その後1ファイルだけupdate。

再度同queryを測定。

HARD:

- unrelated single updateを理由に全base scanへ落ちない
- rare query `UsedScan=false` を維持
- p95 <= 50ms
- p99 <= 100ms

overlayが1 / 100 / 1000 / compaction直前の各状態でも測定。

## 2.7 Write amplification

1M baseで1ファイルupdateを100回。

計測:

- manifest bytes written
- delta bytes appended
- full base rewrite count
- total storage write bytes where measurable

HARD:

```text
full base rewrite count == 0
```

compaction threshold未満の通常updateでbase generationが変わらないこと。

## 2.8 Compaction

delta threshold到達時:

同一ファイルだけを4096回以上更新するfixtureも含め、distinct overlayが小さい場合でも
journal change-count/bytes thresholdでcompactionが起動すること。

- searchを継続可能
- new base atomically publish
- restartで同じ結果
- tombstone反映
- old generation cleanup
- compact前後FP/FN=0

## 2.9 Persistence integrity

HARD:

- writer lease: 同一storeへ2 writer目を拒否
- base index 1byte corruptionを検出
- base metadata 1byte corruptionを検出
- delta payload 1byte corruptionを検出
- manifest corruptionを検出してfail-safe rebuild
- dirty.marker残存後のrestart reconcile
- committed generationをpartial tempより優先
- root identity mismatch storeを再利用しない
- normalizer version mismatch storeを再利用しない

## 2.10 Directory correctness

HARD:

- directory create
- directory rename
- directory move
- recursive directory delete
- 10k child tree delete

子孫eventが全部届くことへ依存しない。

最終結果FP/FN=0。

## 2.10.1 Multi-volume store self-exclusion

C:\ が検索rootで、multi-volume store rootもC:\配下にある構成を必ず試す。

HARD:

- C: catalogが全volume分のshared store rootを検索対象に含めない
- D:/E:側index更新がC: catalogのgeneration更新を誘発しない
- persistenceが自己増殖しない

## 2.11 Queue boundedness

watcher event storm中のqueue/memoryを観測。

100k change stormを発生。

HARD:

- processがunbounded memory growthしない
- overflow時reconcileへ収束
-最終catalog == filesystem oracle

## 2.12 GUI live refresh

query入力中にmatching fileをcreate/delete/rename。

HARD:

GUI result listが手動再入力なしで1000ms以内に更新。

core catalogだけ更新されGUIがstaleのままはFAIL。

---

# 3. Official machine

正式性能PASSは以下だけで宣言。

```text
OS: Windows 11
CPU: Intel Core Ultra 9 285H
RAM: 32 GB
Storage: 内蔵NVMe
Power: AC
Build: Release
.NET SDK: 8.0.424
GPU: 未使用
Network search: 未使用
```

OS/CPU/RAM/AC/DataRoot physical NVMe/.NET SDK versionをreportへ記録。

---

# 4. Build / regression Gate 0

source変更前後とも:

```powershell
dotnet restore PersonalRag.sln
dotnet build PersonalRag.sln -c Release --no-restore
dotnet run --project tests/FilenameSearch.Tests -c Release --no-build
dotnet run --project tests/FilenameSearch.Gui.Tests -c Release --no-build
```

さらに既存のfilename E2E / idleを実行。

旧Astra Gate1/Gate2は今回のfilename completion判定には使わない。

全コマンドのexit codeを保存。

---

# 5. 1M official production corpus

## 5.1 件数

最低1,000,000 searchable filesystem entries。

architecture bake-offのlogical corpusだけではなく、
**production FileSystemCatalog / persistence / GUIを通る形**で測る。

測定rootは内蔵NVMe。

## 5.2 Fixture

含める:

- 日本語
- NFC/NFD
- mixed case
- duplicate names
- deep paths
- wildcard vectors
- hard link where supported
- 0 hit
- common query
- rare query

## 5.3 Build

記録:

- initial discovery
- Route C base build
- persistence publish
- total first-build
- peak private memory
- persistent total bytes

---

# 6. 1M core HARD gates

同一commitで:

```text
FP = 0
FN = 0

p50 <= 20ms
p95 <= 50ms
p99 <= 100ms
worst query class p95 <= 50ms

Ready core private <= 1.00GiB
Persistent filename data <= 1.00GiB
Initial 1M build <= 60s
Existing base load <= 1.5s
```

ただし旧runnerのように数queryごとに強制GCしてuser-visible pauseを測定外へ追い出さない。

2種類記録:

1. algorithm benchmark
2. sustained no-forced-GC benchmark

sustained:

最低10,000 mixed queries。

記録:

- p50/p95/p99/max
- GC count
- private memory before/after

HARD:

```text
sustained p95 <= 50ms
sustained p99 <= 100ms
```

---

# 7. Post-update 1M HARD gate

base load直後:

- rare
- common
- 1-char
- 2-char
- wildcard
- full path

をbaseline測定。

次に1file upsert。

同じsetを20 rounds。

HARD:

```text
rare query must not become full scan
global p95 <= 50ms
global p99 <= 100ms
```

100 / 1000 updates後も同じ。

compaction後にも同じ。

report:

`reports/filename-hardening/post-update-1m.json`

---

# 8. Sustained churn

最低:

```text
10,000 create
10,000 modify
10,000 rename
10,000 move
10,000 delete
```

に加えて30分以上のmixed churnを実施。

観測:

- search p95/p99 during churn
- delta size
- compaction count
- base rewrite count
- process private memory
- queue saturation/reconcile count
- final oracle

HARD:

- final FP/FN=0
- full-base rewriteはcompaction時のみ
- private memory <= 1.50GiB
- create/delete/rename/move convergence p95 <= 1s
- churn後search p95 <= 50ms / p99 <= 100ms

---

# 9. stopped same-name modify catch-up

これは必須。

1M rootを正常終了。
停止中に既存fileを:

- path/name不変
- size変更
- modified time変更

する。

再起動。

HARD:

- GUI/SearchResultのSizeが最新
- Modifiedが最新
- restart catch-up <= 5s

4,096件以上rootでも必ず実行。
小root testだけではPASSにしない。

---

# 10. Fixed local drive federation

通常GUIをroot引数なしで起動。

HARD:

- Readyな固定local driveを自動検出
- C:/D:/...にfixtureがある場合1queryで統合結果
- FileKey collisionなし
- store自体が結果へ出ない
- store書込みがwatcher self-loopを起こさない
- removable/network driveを勝手に対象にしない

最低2 fixed volumeが実機にない場合:

- synthetic/integration testで2-volume federation correctnessを実施
- 実機項目は `NOT_RUN_NO_SECOND_FIXED_VOLUME`
- それ以外をPASSに偽装しない

---

# 11. Actual WPF process 1M startup

旧 `FilenameSearch.Gui.Tests formal-startup` のin-process値だけでは正式PASSにしない。

必ずpublish/build済み:

`FilenameSearch.Gui.exe`

をchild processとして起動。

利用可能なprobe:

`scripts/Run-FilenameActualGuiProbe.ps1`

例:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/Run-FilenameActualGuiProbe.ps1 `
  -GuiExe <Release FilenameSearch.Gui.exe> `
  -Root <1M root> `
  -Store <1M manifest> `
  -Report reports/filename-hardening/gui-actual-01.json `
  -Query fixture_
```

最低10 fresh processes。

query classを変える。

記録:

- process start -> first useful batch
- child private memory
- result rows
- exit status

HARD:

```text
p95 process-start-to-first-batch <= 2.0s
full app private <= 1.50GiB
```

さらに同一processで入力変更first batch:

```text
p95 <= 100ms
max <= 200ms
```

1Mで測る。

---

# 12. Idle

1M loaded stateで10分。

HARD:

```text
average CPU <= 1%
store generation unchanged
delta unchanged
不要な継続writeなし
private <= 1.50GiB
```

read I/O / write I/O deltaも記録。

---

# 13. Persistence size accounting

manifestだけをpersistent bytesにしてはいけない。

必ず:

```text
manifest
+
<manifest>.data/**/* regular files
```

を集計。

writer.lock / dirty markerも実fileであれば分子に含めてよい。

旧100k tiny-file corpusのratioだけで5%判定しない。

Filename phase hard limit:

```text
1M / logical100GiB equivalentで <= 1.00GiB
```

Content phaseの容量を残す。

---

# 14. USN decision

まずfallback watcher/reconcileで正式測定。

以下のどちらかFAIL:

```text
actual existing-index startup <= 2s
restart catch-up <= 5s
```

なら `docs/NTFS_USN_DECISION.md` に従い
NTFS USN backendを `IVolumeChangeFeed` boundary後方へ実装。

外部contractを変更しない。

USNを導入した場合:

- journal reset
- wrap
- access denied
- non-NTFS
- hard link
- offline changes

のfallback testを追加。

---

# 15. Required reports

保存:

```text
reports/filename-hardening/
  ENVIRONMENT.json
  REGRESSION.json
  CORE_1M.json
  SUSTAINED_SEARCH_1M.json
  POST_UPDATE_1M.json
  CHURN.json
  WRITE_AMPLIFICATION.json
  RESTART_MODIFY.json
  DIRECTORY_TREE.json
  PERSISTENCE_INTEGRITY.json
  MULTI_VOLUME.json
  GUI_ACTUAL_STARTUP.json
  GUI_LIVE_REFRESH.json
  IDLE_10M.json
  FINAL_FILENAME_HARDENING.json
```

各report:

- commit SHA
- executable SHA256
- source hashes where relevant
- machine
- command
- raw metrics
- threshold
- pass

を含む。

---

# 16. Final decision

`FINAL_FILENAME_HARDENING.json` は全項目のaggregate。

HARD:

```json
{
  "evaluation_complete": true,
  "pass": true
}
```

のときだけ:

`PERSONALRAG_FILENAME_PHASE_COMPLETE`

を再宣言。

1つでも:

- FAIL
- NOT_RUN（明示的例外を除く）
- BLOCKED
- estimated

ならcompleteではない。

---

# 17. Codexの終了条件

Codexは:

- 「残りは測定です」
- 「次は実機で確認してください」
- 「概ね完了です」

で終了してはいけない。

official Windows machine上で実行可能な作業は自分で実行する。

FAILは修正して再実行する。

Human inputが本当に必要な外部条件だけをBLOCKEDとして明示する。

Filename phaseがcompleteしたらContent Searchへは進まず停止する。
