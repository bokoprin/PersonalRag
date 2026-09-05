# Gate 1 実装チェックポイント 01

日時: 2026-09-05 JST。Gate 1 **未完了**、Gate 2 **未着手**。

## 差分セルフレビュー

- Critical: 本チェックポイントの実装差分で未解決の指摘なし。
- High: 同時writerによるsnapshot競合、保存失敗イベント消失、古いqueryのUI上書きを確認し、writer lease、再保存retry、世代確認を実装。
- Medium: 非常に長い改行なしtextの行確保、全件Regex走査の最悪性能、native keyboardの外部入力試験は未検証。正式Gateの残作業として保持。
- Low: GUI列の配置/一致highlightを目視確認し改善。正本のpixel完全一致は不要。

## 実行した検証

- `dotnet build PersonalRag.Astra.sln -c Release`: PASS、警告0、エラー0。
- `dotnet run --project tests/Astra.Tests -c Release --no-build`: PASS、152 checks。
- `dotnet run --project tests/Astra.Gui.Tests -c Release --no-build`: PASS、9 checks。WPF routed event/実コントロール試験。
- `git diff --check`: PASS。
- 正本2ファイルのSHA256はdelivery manifestと一致。

Windowsの実行中DLL lockで全体buildが一度失敗したが、corpus生成完了後の再実行で解消。sourceを変更して解消したものではない。

## 開発ベースライン

1GiB / 1024 files、固定seed20260905、Core Ultra 9 285H、32GB、NVMe、AC、.NET8 Release。

|版|構築|永続容量|問題queryのp95|
|---|---:|---:|---:|
|初版|1.487秒|0.3351%|最大2366.77ms|
|2/3gram・必要literal候補絞込|2.186秒|0.3647%|約8〜11ms|

生データ: baseline-1g.json / baseline-1g-v2.json（各query 3回の開発試験）。
**100GiBの正式性能を示さない。** 現在のheadにはさらにqueryfilter事前計算・filenameキャッシュ等が含まれ、再測定が必要。
コーパス生成: 10GiB=16.68秒、100GiB=494.49秒。これらは生成時間でありindex構築時間ではない。
