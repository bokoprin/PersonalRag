# PersonalRag Astra

Windows向けローカル deterministic search。正本は [仕様書](specification/PersonalRag_ASTRA_SPEC_v1.md) と [Frozen GUI](specification/PersonalRag_GUI_FROZEN_v1.html)。

このツリーは Gate 1 と Gate 2 の実装を含むが、**正式完成判定は official Windows benchmark machine 上で acceptance runner が全PASSした場合のみ**行う。
未実行のテストや他マシンの推定値を PASS と扱わない。

## 実装済み範囲

- ファイル名 / フルパス substring・AND・wildcard・case切替
- Literal / Regex / Wildcard 内容検索
- 1ファイル1行、遅延Hit列挙、50,000 hitでも有界GUI展開
- UTF-8 / UTF-16LE / UTF-16BE / ASCII text
- DOCX / XLSX / PPTX / PDF text extraction
- DOCX paragraph、XLSX sheet/cell、PPTX slide、PDF page location
- 2/3文字signatureによる保守的候補絞り込み + 元ファイル最終照合
- ASTRA003 block persistence、SHA256検証、writer排他、crash temp回収
- FileSystemWatcherによる継続更新 + overflow/restart時reconcile
- Office/PDFの有界RAM LRU extraction cache（永続容量には含めない）
- 将来自然文query plannerから呼べる `IDeterministicSearch` 境界

PDF extractionのみ実用的なPDF text layer対応のため `PdfPig 0.1.16` を使用する。OCRはv1対象外。

## ビルド

Official環境は Windows 11 / .NET SDK 8.0.424。

```powershell
dotnet restore PersonalRag.Astra.sln
dotnet build PersonalRag.Astra.sln -c Release --no-restore
dotnet run --project tests/Astra.Tests -c Release --no-build
dotnet run --project tests/Astra.Gui.Tests -c Release --no-build -- artifacts/gui-tests
```

GUIは既存indexがあれば即loadする。初回のみ検索root選択ダイアログを表示し、その後のメイン画面はFrozen GUIに合わせる。
`PERSONALRAG_ROOT` 環境変数を指定すれば初回ダイアログを省略できる。
保存先は `%LOCALAPPDATA%\PersonalRagAstra\index`。

## 正式受け入れ

Gate 1:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/Run-Gate1.ps1 -Generate
```

Gate 1がPASSした後、Gate 2:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/Run-Gate2.ps1 -Generate
```

`-Generate` は 10GiB / 100GiB / 1,000,000 files / mixed 10GiB の固定コーパスを初回だけ生成するため、大きな空き容量が必要。
既存コーパスを再利用する場合は `-DataRoot` を指定して `-Generate` を外す。

正式結果:

- `reports/formal-gate1/GATE1_SUMMARY.json`
- `reports/formal-gate2/GATE2_SUMMARY.json`

どちらも `pass: true` になったときのみ `PERSONALRAG V1 COMPLETE` と宣言できる。
一括実行は次を使う。

```powershell
powershell -ExecutionPolicy Bypass -File scripts/Run-All-Formal.ps1 -Generate
```

formal runner は Windows 11 / Core Ultra 9 285H / 32GB / AC接続に加え、`DataRoot` が実際に載っている物理ディスクの `BusType=NVMe` を確認する。GUI startup は実WPFプロセスで first filename/content batch と private memory を測る。

## CLI

```powershell
dotnet run --project src/Astra.Cli -c Release -- index C:\Corpus C:\AstraIndex
dotnet run --project src/Astra.Cli -c Release -- search C:\AstraIndex '*.txt' '日本語'
dotnet run --project src/Astra.Cli -c Release -- inspect C:\AstraIndex
dotnet run --project src/Astra.Cli -c Release -- ratio C:\AstraIndex
```

[受け入れ台帳](docs/ACCEPTANCE.md) / [構造と将来拡張境界](docs/ARCHITECTURE.md) / [進捗](docs/PROGRESS.md)
