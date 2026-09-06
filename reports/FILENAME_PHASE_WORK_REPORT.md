# PersonalRag Astra ファイル名検索フェーズ 作業報告

## 対象と完了状態

添付計画のSection 30（Filename Phase 最終完了条件）に従い、Windows上の決定的なファイル名・フルパス検索を実装、検証しました。

- ブランチ: `codex/filename-phase-final`
- 計測対象ソースコミット: `42a13c170662c47c26101886cafc86cd80a74ee9`
- 直前の受け入れコミット: `79b33c1ea62516ad791e3a251c118bfaa021b807`
- 最終判定: `PERSONALRAG_FILENAME_PHASE_COMPLETE`
- 内容検索、Office/PDF抽出、LLM、embedding、vector searchはこのフェーズでは開始していません。

## 実装内容

- Route C（adaptive short n-gram + selective compressed postings）をChampionとして製品経路へ接続
- Windowsファイルシステム列挙、FileSystemWatcher、再起動時reconcile、atomic persistenceを実装
- Frozen GUIのファイル名・フルパス検索、case切替、結果一覧、非同期検索、stale response抑止を実装
- 大規模起動時も既存スナップショットを検索に利用しながらcatch-upを進める構成へ調整
- `RecordCount`による定数時間のGUIステータス更新と、保存完了を含むidle測定を実装

## Official 1M core

Official corpusは1,000,000 records、論理107,374,182,400 bytes、query 20 rounds（warmup 2 / timed 18）で固定しました。

| 指標 | 実測値 | Hard limit |
| --- | ---: | ---: |
| False positive / false negative | 0 / 0 | 0 / 0 |
| 検索 p50 | 3.8065 ms | 20 ms |
| 検索 p95 | 23.9066 ms | 50 ms |
| 検索 p99 | 31.7082 ms | 100 ms |
| 最悪query class p95 | 38.6437 ms | 50 ms |
| Persistent bytes | 477,165,162 | 1,073,741,824 |
| Ready core private memory | 518,512,640 bytes | 1,073,741,824 bytes |
| 1M build | 38.8486 sec | 60 sec |
| Fresh load | 1.3820 sec | 1.5 sec |
| Update p95 | 0.008 ms | 10 ms |

詳細は [`bench/FINAL_FILENAME_ACCEPTANCE.json`](../bench/FINAL_FILENAME_ACCEPTANCE.json) と、ignored raw reportの `bench-data/official-20260905/reports/route-c-final-6c3ba2f.json` に保存しています。

## Windows product acceptance

- GUI初回batch: p95 85.7499 ms、hard max 89.5225 ms（10 rows）
- 既存index起動: 979.2621 ms
- GUI全体private memory: 414,441,472 bytes
- idle 600秒: CPU 0%、store不変
- create / delete / rename / move convergence p95: 127.8064 / 100.5528 / 127.0817 / 134.651 ms
- 正常restart catch-up: 2,812.5986 ms
- 強制終了後の再起動復旧: PASS
- 破損storeからの復旧: PASS
- 100,000 real-files E2E: 49 checks PASS、初期検出104,007 entries
- churn: create / modify / rename / move / delete を各10,000件処理し、created_matches 10,000、remaining_matches 0
- reparse point: indexed false を確認

生成済みレポート:

- [`reports/filename-product-acceptance-final.json`](filename-product-acceptance-final.json)
- [`reports/filename-e2e-product.json`](filename-e2e-product.json)
- [`reports/filename-gui-startup-100k.json`](filename-gui-startup-100k.json)
- [`reports/filename-idle-10m.json`](filename-idle-10m.json)

## 検証コマンド

```powershell
dotnet build PersonalRag.Astra.sln -c Release --nologo --no-restore
dotnet run --project tests/FilenameSearch.Tests -c Release --no-build
dotnet run --project tests/FilenameSearch.Gui.Tests -c Release --no-build
dotnet run --project tests/Astra.Tests -c Release --no-build
dotnet run --project tests/Astra.Gui.Tests -c Release --no-build
git diff --cached --check
```

結果は順に、Release build（警告0・エラー0）、FilenameSearch 14 checks、FilenameSearch GUI 3 checks、既存コア686 checks、既存GUI 9 checks、差分検査PASSでした。

## 未実施・補足

- inaccessible directoryのACL変更試験は、別ユーザーアカウントの準備が必要なため `NOT_RUN` です。Section 30のHard Gate項目には含まれていません。
- 100k製品corpusのpersistent ratioは70.9378%でした。この値は小さなmetadata-only実ファイル木に対する参考値で、Section 30のWindows product gateにはpersistent ratioの閾値が定義されていないため、受け入れ判定には使用していません。
- Official raw benchmark reportは再生成による実験条件の変更を避けるためignored領域に保持し、最終JSONへ値とSHA-256を記録しています。

## 起動

Release GUIは次で起動できます。

```powershell
& "src/FilenameSearch.Gui/bin/Release/net8.0-windows/FilenameSearch.Gui.exe" "C:\Users\bokop\Documents"
```

第1引数を省略すると検索対象フォルダーの選択ダイアログが開きます。既存indexは `%LOCALAPPDATA%\PersonalRagAstra\filename-index` に保存されます。
