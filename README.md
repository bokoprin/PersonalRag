# PersonalRag Astra

Windows向けローカル deterministic search。正本は [仕様書](specification/PersonalRag_ASTRA_SPEC_v1.md)。
現在はGate 1開発中。正式な性能・容量・GUI受け入れが全PASSした完成版ではありません。

## ビルド・検証

Windows 11、.NET SDK 8.0.424（Windows Desktop targeting packを含む）。外部NuGet依存なし。

```powershell
dotnet build PersonalRag.Astra.sln -c Release
dotnet run --project tests/Astra.Tests -c Release --no-build
dotnet run --project src/Astra.Gui -c Release --no-build
```

GUI起動後「対象フォルダー」で検索対象を選ぶ。保存先は `%LOCALAPPDATA%\PersonalRagAstra\index`。
保存先は検索対象フォルダーの外である必要がある。元ファイルは変更しない。
名前入力で即時検索、内容入力でHits列と一致ペインを表示。「さらに表示」で次の100ファイル。
一覧の上下キーでファイル、Enterで既定アプリから開く。一致ペインの上下/Home/Endは同じファイル内を移動。

CLI:

```powershell
dotnet run --project src/Astra.Cli -c Release -- index C:\Corpus C:\AstraIndex
dotnet run --project src/Astra.Cli -c Release -- search C:\AstraIndex '*.txt' '日本語'
dotnet run --project src/Astra.Cli -c Release -- inspect C:\AstraIndex
dotnet run --project src/Astra.Cli -c Release -- ratio C:\AstraIndex
```

`index`は保存後にchecksumを確認して再読込する。`ratio`は指定store配下の全ファイルbytesを分子に含む。
CLIはsnapshotを読み、GUIは継続監視する。corrupt snapshotは明示エラーとなる。
暗黙のネットワーク検索、自然文/LLM検索は行わない。

[受け入れ台帳](docs/ACCEPTANCE.md) / [構造と将来拡張境界](docs/ARCHITECTURE.md)
