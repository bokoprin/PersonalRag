# Validation status — Astra finish pass

## Source completion status

ASTRA003 / Gate 1 / Gate 2 の source-level 仕上げを実施済み。
Frozen specification と Frozen GUI は変更していない。

今回閉じた主な残件:

- `formal-startup` 実WPF modeをGUI test runnerへ実装
- 10GiB / 100GiB / 1M files / mixed 10GiB の実WPF first-batch startup計測
- 実WPF filename-ready / content-search private memory計測
- Build / Ready / Search memoryを別processで測るformal benchmark
- 1M file filename/path hard gate
- DataRootの実配置物理diskがNVMeであることのformal machine検証
- build/search間のAstra.Core binary SHA256 identity gate
- 破損DOCX / binary-control text等を1ファイル単位で隔離
- 検索中に元ファイルが変化した場合のexact-hit結果破棄
- Office/PDF extraction cacheの更新中poison防止
- Gate 2 DOCX live update + restart persistence回帰test
- Gate 2 mixed corpusをsearchable payload主体に固定しopaque paddingを禁止
- Gate 2 mixed corpusでも実WPF startup/RAMをhard gate化
- Frozen GUIとの差分だった内容clear buttonを `×` に統一

## この編集環境で実施済み

PASS:

- Frozen spec SHA256 verification
- Frozen GUI SHA256 verification
- 全 `.csproj` XML parse
- 全C# sourceのlexical delimiter scan
- PowerShell runnerのlexical delimiter scan
- WPF XAML `x:Name` / test `FindName` consistency
- WPF XAML event handler / code-behind consistency
- source内 unfinished marker / stub scan
- `dotnet build PersonalRag.Astra.sln -c Release`: 警告0 / エラー0
- `dotnet run --project tests/Astra.Tests -c Release --no-build`: `PASS 686 checks`
- `dotnet run --project tests/Astra.Gui.Tests -c Release --no-build`: `PASS GUI 9 checks`
- ASTRA003一時ストアでのcrash-write / abandoned temp回収 smoke test: `RECOVERY PASS`

追加のASTRA003ローカル実測（10GiB / 10,240 files）も完了している。build 13.32秒、永続比率0.00888%、update p95はcreate 47.9ms / modify 171.7ms / rename 142.6ms / move 120.2ms / delete 49.4ms、10,000件churn後の再起動一致とピークprivate 1.59GiB、実WPF first filename/content batch 617ms / 827msだった。これらはofficial GateのPASSとは別の回帰証跡である。

## 正式受け入れで未実行

ローカルの実装テストは通過したが、official benchmark machine上の性能判定はまだ **NOT_RUN**。ローカルPASSを正式PASSとは扱わない。

- ASTRA003 10GiB / 100GiB / 1M正式benchmark
- Gate 1 formal acceptance
- Gate 2 mixed 10GiB formal acceptance
- official Windows WPF実GUI startup measurement
- 10分idle / 10,000-file churn / 100-sample update latencyの正式測定

## Official Windowsでの次の正本操作

Windows 11 / Core Ultra 9 285H / 32GB / AC / DataRoot on NVMe / .NET SDK 8.0.424 で:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/Run-All-Formal.ps1 -Generate
```

既にfixed corpusが存在する場合:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/Run-All-Formal.ps1 -DataRoot D:\path\to\bench-data
```

全HARD gateがPASSした場合のみ `reports/formal/PERSONALRAG_V1_COMPLETE.json` が生成され、`PERSONALRAG V1 COMPLETE` と宣言できる。
