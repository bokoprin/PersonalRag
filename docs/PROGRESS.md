# 進捗と再開位置

2026-09-05、ブランチ `feat/astra-gate1`。goalは継続中。Gate 1未完了、Gate 2未着手。

## 確認済み

- 正本仕様/GUIのSHA256は同梱delivery JSONと一致。仕様は変更していない。
- 指定CPU、32GB、NVMe、Windows 11。ユーザーがAC接続を回答し、BatteryStatus=2を確認。
- .NET 8.0.424、外部NuGet依存なし。検索Core / CLI / WPF GUI / Core tests / WPF tests / Benchを新規実装。
- コア150検証にPASS（名前、内容、Unicode/encoding、binary、regex、50,000hit、保存破損、変更追従、再起動差分）。
- WPFテスト9検証にPASS。32件のhit page、上下/Home/End、世代棄却、AND、構文エラーを実コントロールで試験。外部UI入力ではなくWPF routed eventによる検証。native keyboard/Enter実ファイル起動、フォーカス、freeze/正式起動性能は別途必要。
- 1GiBベースライン `reports/baseline-1g.json` は一部queryが0.75〜2.37秒でFAIL。
- 2/3文字signatureと必要literal抽出後 `reports/baseline-1g-v2.json` はquery全体概ね0.1〜11ms。構築2.186秒、容量0.36468%。正式スケールのPASSではない。

## 生成済み/生成中

- `bench-data/baseline-1g`: 1024 files, 1GiB。
- `bench-data/core-10g`: 10240 files, 10GiB。
- `bench-data/core-100g`: 102400 files, 100GiB（生成完了をmanifestとプロセス出力で確認すること）。
- `benchmarks/Astra.Bench/Program.cs` の生成手順はv1として固定。seed20260905、1MiB files、5種類、10% UTF16。内容分布/queryを性能結果に合わせて変えない。
- generated corpus / index / build outputsはignore、reportsはcommitする。

## 次の作業

1. 100GiB生成は494.49秒で完了。Windows DLL lockによるbuild失敗は解消し、全体build+コア152 checks+GUI9 checksにPASS。
2. queryfilter事前計算、filenameキャッシュ、writer排他、temp回収、保存retryを含めた初回checkpointを保存する。
3. 正式測定用はpublish先を一意のartifacts配下に固定し、実行中binaryと開発buildを分離。
4. 10GiB/100GiBの構築・容量・100回/query性能・メモリ・再起動を測定。baseline結果を残したまま改善。
5. 100万実ファイル、独立全件oracle、10,000×4 churn/force-kill recovery、10分idle、GUI正式測定。
6. Gate 1全PASSまでGate 2に着手しない。

## 未解決の設計確認事項

- 非常に長い改行なしテキストはStreamReader.ReadLineで1行分を確保する。メモリ上限を含め境界試験が必要。
- 検索はsnapshot世代を使い、候補の元ファイルを実照合する。更新公開までの時間窓は変更追従条件で測る。
- 所有indexへの複数writer排他、強制終了時temp回収、保存失敗retryの強化が必要。
- 100GiBを小規模結果から推定PASSにしない。全HARD条件はAC/Releaseで実測する。
