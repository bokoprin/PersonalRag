# PersonalRag

PersonalRag は Windows 向けローカル検索アプリです。

## 現在の正本

Filename/Path Search の production 正本は次です。

- Solution: `PersonalRag.sln`
- App: `src/FilenameSearch.Gui`
- Catalog: `src/FilenameSearch`
- Search core: `src/FilenameSearch.Core`
- Champion engine: `src/FilenameSearch.RouteC`

`PersonalRag.Astra.sln` / `src/Astra.*` は過去の内容検索研究実装として残していますが、
**Filename/Path production path ではありません**。次の Content Search フェーズでも、まず
`docs/CONTENT_ENGINE_BOUNDARY.md` の境界から追加し、Filename Engine を置き換えないこと。

## Filename Search architecture

Route A/B/C の bake-off で選ばれた Route C を immutable base として維持し、その上に:

- exact filesystem path identity
- `FileKey = VolumeId + native file id`
- indexed mutable delta
- tombstone
- background compaction
- generation based persistence
- checksum
- writer lease
- dirty-shutdown marker
- typed catalog change feed
- fixed local volume federation

を追加しています。

1件の更新後に1M件の全scanへ落ちたり、1変更ごとに全indexを永続化し直したりしないことが
production invariant です。

## 検索 semantics

- Filename / FullPath
- substring
- 空白区切り AND
- `*` / `?` wildcard
- wildcard は **substring pattern**
- case-sensitive ON/OFF
- Unicode NFC search key
- case-insensitive は Unicode full case folding + NFC

ファイルシステムから得た `Name` / `FullPath` は検索用に正規化して保存しません。
NFC/NFD が異なる実パスを同じパスへ書き換えないこと。

## 起動

通常起動では固定ローカルドライブを自動検出して統合検索します。

特定rootだけで開発/テストする場合:

```powershell
dotnet run --project src/FilenameSearch.Gui -c Release -- C:\SomeRoot
```

Canonical build:

```powershell
dotnet restore PersonalRag.sln
dotnet build PersonalRag.sln -c Release --no-restore
dotnet run --project tests/FilenameSearch.Tests -c Release --no-build
dotnet run --project tests/FilenameSearch.Gui.Tests -c Release --no-build
```

## Actual process startup probe

WPF test process内の `new MainWindow()` ではなく、実際の `.exe` process 起動から Ready まで測るため:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/Run-FilenameActualGuiProbe.ps1 `
  -GuiExe <FilenameSearch.Gui.exe> `
  -Root <root> `
  -Store <index.manifest> `
  -Report reports/actual-gui-startup.json `
  -Query fixture_
```

## 正式再測定

hardening 後の Filename phase は旧レポートをそのまま PASS とみなしません。

`docs/CODEX_FILENAME_FORMAL_REMEASURE.md`

を Codex に渡し、1M GUI / post-update / sustained churn / all-volume / restart /
actual process startup を再測定し、全HARD条件を満たしてから再び

`PERSONALRAG_FILENAME_PHASE_COMPLETE`

を宣言します。

## 将来の Content Search

Content Engine は `IFilenameCatalog.GetSnapshot()` で初期状態を取得し、`FileKey` と typed `CatalogChangeBatch`（`SourceId` / `SourceGeneration` 付き）を購読します。
Content Engine が独自の FileSystemWatcher を持たないこと。

詳細:

- `docs/FILENAME_ENGINE_ARCHITECTURE.md`
- `docs/CONTENT_ENGINE_BOUNDARY.md`
- `docs/NTFS_USN_DECISION.md`
