# 進捗と再開位置

## 現在状態

Gate 1 / Gate 2 の**source implementationとローカル回帰試験は完了したが、formal acceptanceは未完了**。
現環境ではReleaseソリューションビルド、Astra.Tests 686 checks、Astra.Gui.Tests 9 checks、ASTRA003復旧smoke testがPASSしている。
次の正本作業は official Windows machine で acceptance runner を実行し、FAILした数値だけを最適化すること。

## ASTRA003 ローカル実測（正式判定ではない）

同一の10GiB / 10,240ファイル `bench-data/core-10g` で、現行Release binaryを実測した。

- build: 13.32秒、永続比率0.00888%、unsearchable 0
- content search 100回: 全query完了、binary SHA256をbuild/searchで照合
- update latency 20 samples: create p95 47.9ms、modify 171.7ms、rename 142.6ms、move 120.2ms、delete 49.4ms
- 10,000件churn: create 99.37秒、modify 25.21秒、rename 9.57秒、delete 1.83秒、再起動後10,240ファイル一致、ピークprivate 1.59GiB
- crash-write / abandoned temp recovery: `RECOVERY PASS`
- 実WPF startup: filename 617ms、content 827ms、filename-ready private 333MiB、content-search private 614MiB

上記は現行実装の回帰・容量・挙動確認用であり、100GiB / 1M filesを含むofficial GateのPASS宣言には使わない。

## Astraから引き継いだ実測 checkpoint (ASTRA002)

100GiB / 102,400 files:

- build 332.27s
- persistent ratio 0.3646%
- normal content p95 max 33.35ms
- short content p95 max 16.69ms
- load 4.28s (FAIL)
- 同一process全体peak private 約5.13GiB（Ready値としては計測汚染あり）

Astraはこのload/memory問題に対してASTRA003 block persistenceへ変更中に停止した。

## 今回仕上げた内容

- ASTRA003: 256-file Brotli block、block SHA256、共有raw buffer slice
- formal benchmarkをbuild processとfresh search processに分離
- Ready/search/build memoryを別指標化
- cold first queryを参考値として保存
- 1,000,000-file filename/path専用測定
- create/modify/rename/move/delete各100 samples p95測定
- Gate1 PowerShell runner / summary
- DOCX/XLSX/PPTX extractor
- PdfPig 0.1.16 PDF extractor
- Office/PDF bounded RAM LRU cache
- Gate2 functional fixture tests + location検証
- Gate2 mixed corpus generatorを10GiBで固定
- Gate2 PowerShell runner / summary
- Frozen GUIから外れていた常設「対象フォルダー」buttonを削除。初回だけmodal setup。
- inaccessible directoryは全体index buildを停止しない
- 破損DOCX / binary-control text等の `InvalidDataException` を1ファイル単位で隔離
- Office/PDF cacheの更新中poison防止（抽出前後size/mtime一致時のみcache）
- `formal-startup` を実WPF test runnerへ実装し、10GiB/100GiB/1M filename first batchを測定
- 実WPFのfilename-ready/content-search private memoryを正式Gateへ追加（Gate1 100GiB/1M、Gate2 mixed 10GiB）
- `DataRoot` の実配置物理ディスクがNVMeであることをformal runnerで検証
- 1M-file storeにもload/Ready/Search RAM hard gateを適用
- Gate2 DOCX live update / restart persistence回帰testを追加
- GUIテストの`Key`名前衝突を修正し、現行ソースでWPF 9 checksを再実行
- `dotnet run`配下でもrecovery childを正しく起動し、コミット前停止を再現可能化
- recovery / idle / update / churnレポートの完了状態をformal summaryへ反映

## 次に実機で行うこと

1. `powershell -ExecutionPolicy Bypass -File scripts/Run-Gate1.ps1 -Generate`
2. `GATE1_SUMMARY.json` のFAILを確認
3. FAILがあれば corpus/query/thresholdを変えず実装だけ修正
4. Gate1全PASS後 `powershell -ExecutionPolicy Bypass -File scripts/Run-Gate2.ps1 -Generate`
5. Gate2全PASS後だけ `PERSONALRAG V1 COMPLETE`

この環境の直近電源APIは`ACLineStatus=0` / `BatteryStatus=1`を返しているため、runnerのACチェックを通すには電源状態の再確認が必要。

特に最初に注目する指標:

- ASTRA003 100GiB fresh-process load <= 2s(filename), <=3s(content)
- Ready private <=2GiB
- search peak <=3GiB
- 1M filename/path p95 <=50ms
- mixed document normal p95 <=100ms
- mixed persistent <=5%

未実行項目をPASSと記録しない。
