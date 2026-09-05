# PersonalRag Astra Autonomous Build Specification v1

## 0. この文書の役割

この文書は、Astra が **既存実装を引き継がず、PersonalRag をゼロから完成まで自走して実装するための正本仕様**である。

Astra は、使用言語、GUIフレームワーク、データ構造、インデックス方式、永続形式、並列化方式、検索アルゴリズム、ライブラリ選定、ビルドシステム、内部API設計を自由に決めてよい。

ただし、**この仕様で定義された外部挙動・正確性・性能・容量・安定性・GUIをすべて満たすことが完成条件**である。

Astra は仕様を満たすために、自律的に次を繰り返すこと。

1. 要求を分析する
2. 設計する
3. 実装する
4. テストする
5. ベンチマークする
6. ボトルネックを特定する
7. 修正する
8. 回帰テストする
9. 全完了条件が PASS するまで継続する

Astra 自身が「十分速い」「完成した」と判断しただけでは完成とはみなさない。

---

# 1. 製品目的

PersonalRag は Windows 上で動作するローカル検索アプリケーションである。

最初に完成させるのは deterministic search であり、以下を実現する。

- Everything のように軽快なファイル名 / パス検索
- 実用的に即応するファイル内容検索
- ファイルシステム変更への継続追従
- 再起動後も再利用可能な永続インデックス
- 低い永続ストレージ使用量
- 常用可能なGUI
- 将来、自然文検索 / LLM検索を追加しやすい構造

自然文検索、embedding、vector search、LLM reranking、チャットUIは **この v1 の実装対象外**。

ただし、将来それらを追加する際に deterministic search core を作り直す必要がない設計であること。

---

# 2. 開発方針

## 2.1 既存実装

既存の PersonalRag 実装、過去のインデックス形式、過去の内部API、過去のアーキテクチャとの互換性は要求しない。

Astra はゼロから再設計してよい。

## 2.2 実装自由

以下は Astra に完全に委ねる。

- プログラミング言語
- GUI framework
- 検索エンジン構造
- persistent format
- indexing algorithm
- compression
- cache
- mmap 使用有無
- threading / async model
- incremental update mechanism
- parser / extractor library
- project layout

仕様を満たす限り制約しない。

## 2.3 禁止事項

Astra は以下を行ってはならない。

- 完了条件を勝手に緩和する
- benchmark corpus を都合よく変更する
- performance test から難しい query を除外する
- persistent data の一部を「cache」と呼んで 5% 計算から除外する
- GUI freeze を独断で変更する
- Gate 1 未完成のまま Gate 2 を優先する
- v1 のために LLM / embedding / vector DB を導入する
- benchmark 数値を推定値だけで PASS にする

---

# 3. GUI 正本

GUI 正本は同梱ファイル:

`PersonalRag_GUI_FROZEN_v1.html`

とする。

Astra が使用する GUI framework は自由だが、完成アプリの主操作・情報構造・挙動はこのモックを再現すること。

ピクセル完全一致は要求しない。

## 3.1 GUI の基本原則

通常のファイル名検索は Everything の操作感を踏襲する。

メイン画面には以下を持つ。

- ファイル名 / パス検索欄
- 対象切替
  - ファイル名
  - フルパス
- 大文字 / 小文字区別
- 内容検索欄
- 内容検索 mode
  - Literal
  - Regex
  - Wildcard
- 検索結果一覧
- 内容検索時の Hits 列
- 選択ファイル内の一致内容ペイン
- 状態表示

## 3.2 結果一覧

結果は **1ファイル1行**。

基本列:

- Name
- Path
- Size
- Modified
- Hits（内容検索時）

内容検索で1ファイルに1,000件ヒットしても、検索結果一覧には1行だけ表示する。

## 3.3 キーボード操作

最低限、以下を実装する。

### 検索結果一覧にフォーカス中

- `↑` : 前のファイル
- `↓` : 次のファイル
- `Enter` : 選択ファイルを開く

### 内容一致ペインにフォーカス中

- `↑` : 同じファイル内の前のヒット
- `↓` : 同じファイル内の次のヒット
- `Home` : 最初のヒット
- `End` : 最後のヒット

内容一致ペインで `↑ / ↓` を押したときに、別ファイルへ移動してはならない。

## 3.4 検索中のGUI

- 検索処理で UI thread を長時間blockしてはならない
- 古いqueryの結果が新しいqueryの結果を上書きしてはならない
- 全件列挙完了を待たず、最初の有用な結果を優先して表示する
- 大量hitでもGUI操作を継続できること

---

# 4. 検索仕様

# 4.1 ファイル名 / パス検索

Everything の全検索言語互換は要求しない。

v1 の必須機能:

- filename substring search
- full path substring search
- 空白区切りの複数語 AND
- `*` wildcard
- `?` wildcard
- case-sensitive ON/OFF
- Unicode filename/path
- 日本語 filename/path
- 0 hit
- 大量hit

GUI・操作感は Everything を踏襲するが、Everything 固有の高度な検索構文の完全互換は v1 の完成条件ではない。

---

# 4.2 ファイル内容検索

v1 では以下を必須とする。

- Literal
- Regex
- Wildcard
- case-sensitive ON/OFF
- 日本語
- Unicode
- filename/path 条件との AND
- 1ファイル内の複数hit
- 1ファイル内の大量hit

検索結果は必ず最終的に正確であること。

---

# 4.3 query 組み合わせ

ファイル名 / パス条件と内容条件を同時指定した場合:

`FileCondition AND ContentCondition`

とする。

どちらか一方を空欄にできる。

---

# 5. Gate 1 — 高速 deterministic search core

Gate 1 は最優先。

Gate 1 が PASS するまでは Gate 2 を完成扱いしてはならない。

## 5.1 Gate 1 内容検索対象

最低限、以下の text を対象とする。

- UTF-8
- UTF-16 LE
- UTF-16 BE
- ASCII

代表的対象:

- `.txt`
- `.log`
- `.md`
- `.csv`
- `.tsv`
- `.json`
- `.xml`
- `.yaml`
- `.yml`
- `.ini`
- `.toml`
- source code
- その他、安全に text と判定・decode可能な通常テキスト

binary を text と誤判定して大量の偽検索結果を生成してはならない。

---

# 6. Gate 2 — Office / PDF 検索

Gate 1 完成後、以下を内容検索対象に追加する。

必須:

- DOCX
- XLSX
- PPTX
- PDF

旧 binary Office format (`.doc`, `.xls`, `.ppt`) は v1 必須ではない。

## 6.1 Gate 2 の基本要件

文書形式固有の parser / extractor と検索coreを疎結合にすること。

後から新しい文書形式を追加するとき、検索engine全体を作り直す必要がないこと。

## 6.2 表示 location

可能な限り、人間が理解できる location を返す。

例:

- text/source: `Line 42`
- XLSX: `Sheet1!B27`
- PPTX: `Slide 18`
- PDF: `Page 31`
- DOCX: paragraph / section / logical location

内部表現は自由。

## 6.3 Gate 2 の correctness

抽出可能な text を対象に、検索結果の false negative を発生させない。

暗号化文書、破損文書、画像のみPDFなど、text extraction が原理的にできないものは明示的に「検索不能」と扱ってよい。

OCR は v1 必須ではない。

---

# 7. 性能要件

性能要件は **official benchmark machine** で測定する。

# 7.1 Official benchmark machine

- OS: Windows 11
- CPU: Intel Core Ultra 9 285H
- RAM: 32 GB
- Storage: 内蔵 NVMe SSD
- Power: AC接続
- Build: production / release build
- Dataset: 内蔵NVMe上
- GPU: deterministic searchには使用しない
- Network: 検索には使用しない

correctness test は他環境でも実行してよいが、性能PASS判定はこのマシンを正本とする。

---

# 7.2 First useful batch

検索 latency は「全件検索完了時間」ではなく、GUIがユーザーに最初の有用な結果を表示可能になるまでを測る。

First useful batch:

- 最大100ファイル
- 内容検索では各ファイルの最初の表示に必要な情報を含む
- 全hitの materialize は不要

---

# 7.3 ファイル名 / パス検索 latency

1,000,000 file corpus で測定する。

Warm search:

- p50 ≤ 20 ms
- p95 ≤ 50 ms
- p99 ≤ 100 ms

上記をすべて PASS すること。

---

# 7.4 ファイル内容検索 latency

100 GiB official content corpus で測定する。

## 通常query

- p50 ≤ 50 ms
- p95 ≤ 100 ms
- p99 ≤ 200 ms

## 短く高頻度なquery

例:

- `th`
- `in`
- `er`
- `00`
- benchmark corpus 上で意図的に高頻度になる2文字query

条件:

- p95 ≤ 200 ms
- hard maximum ≤ 300 ms

short/common query を benchmark から除外してはならない。

---

# 7.5 大量hitファイル

1ファイル内に少なくとも50,000 hitを持つ test file を用意する。

必須:

- 結果一覧は1ファイル1行
- GUIをfreezeさせない
- ファイル選択から最初のhit表示 p95 ≤ 100 ms
- 次/前のhit navigation が即応する
- 50,000 hitを最初に全てGUI objectへ展開しない
- hit数に比例してメモリが無制限増加しない

---

# 7.6 初回index構築

official benchmark machine 上:

## 10 GiB corpus

- HARD ≤ 2分

## 100 GiB corpus

- HARD ≤ 15分

可能なら build 中も完成済み部分から検索可能にする。

ただし progressive availability は推奨であり、上記時間内に全index完成することが必須。

---

# 7.7 起動 / 再起動

既存indexありの場合:

- GUI起動後、filename/path検索可能 ≤ 2秒
- content検索可能 ≤ 3秒
- 起動時の完全再indexは禁止

起動後の差分catch-upはバックグラウンドで行ってよい。

---

# 7.8 filesystem変更追従

通常サイズのローカルファイルに対して:

### filename/path

create / delete / rename / move:

- p95 ≤ 1秒で検索結果へ反映

### content

content modify:

- p95 ≤ 2秒で新内容へ反映

古い内容が最終検索結果に残ってはならない。

---

# 8. 永続インデックス容量

これは HARD 完了条件。

## 8.1 定義

`PersistentRatio = PersonalRagが通常運用で保持する全永続bytes / 検索対象元ファイルの総bytes`

## 8.2 完了条件

- `PersistentRatio ≤ 5.00%`

5%は「努力目標」ではなく完成条件。

## 8.3 分子に含めるもの

以下をすべて含む。

- filename/path index
- content index
- metadata
- postings
- dictionaries
- manifests
- persistent cache
- change journal
- checkpoints
- recovery data
- rollback generation
- app-owned persistent search data

フォルダを分けても除外不可。

build中だけ存在し、正常完了後に削除される temporary file は除外してよい。

## 8.4 測定corpus

最低限:

### Core corpus

- 10 GiB
- 100 GiB
- 主に text/source/log

### Mixed document corpus

- text
- DOCX
- XLSX
- PPTX
- PDF

Gate 2 完了時は mixed document corpus でも ≤ 5.00% を満たすこと。

個別ファイル単位の5%ではなく corpus 全体で判定する。

---

# 9. メモリ / 常駐負荷

official benchmark machine 上。

## 9.1 Memory

HARD:

- Ready / idle ≤ 2 GiB
- 通常検索中 ≤ 3 GiB
- 初回index peak ≤ 8 GiB
- incremental update peak ≤ 4 GiB

正式測定では PersonalRag process 自身の process-private / committed memory を採用する。

OS file cache は参考値。

## 9.2 Idle CPU

index Ready、filesystem変更なしの状態で:

- 10分平均 CPU ≤ 1%

継続的な不要disk read/writeを発生させない。

---

# 10. Correctness

速度より correctness を優先する。

# 10.1 ファイル名 / パス

official test corpus と brute-force oracle を比較する。

supported query について:

- false negative = 0
- false positive = 0

# 10.2 内容検索

official test corpus を直接scanする oracle と比較する。

supported file / supported query について:

- false negative = 0
- false positive = 0

candidate generation 内部では false positive が存在してもよいが、GUIへ返す最終結果は正確であること。

---

# 11. Restart / Recovery / Churn

## 11.1 Restart

- 正常終了→再起動
- 強制終了→再起動

の両方をtestする。

既存indexを安全に再利用できること。

破損を検知した場合、誤った検索結果を返すよりfail-safeを優先する。

## 11.2 Churn test

最低限、同一corpusに対して:

- 10,000 create
- 10,000 modify
- 10,000 rename/move
- 10,000 delete
- restart

を実施。

その後:

- correctness PASS
- PersistentRatio ≤ 5.00%
- restart PASS
- filesystem state と検索結果が一致
- 不要な古いgenerationが無制限に蓄積しない

---

# 12. 将来自然文検索を追加するための要件

v1 では自然文検索を実装しない。

ただし deterministic search engine は、将来以下を上位層として追加可能であること。

- natural-language query parser
- LLM query planner
- semantic retrieval
- embedding/vector search
- reranker
- hybrid deterministic + semantic search

最低限、将来の query planner が既存 deterministic search を呼び出せる明確な境界を持つこと。

将来自然文検索を追加するために:

- filename index を作り直す
- content index を作り直す
- GUIの基本検索 semantics を破壊する

必要がないこと。

内部API形式はAstraに任せる。

---

# 13. Astra 自走ルール

Astra は各Gateについて以下を自律実行する。

1. acceptance test を先に定義
2. baseline / benchmark を記録
3. 実装
4. focused test
5. full regression
6. performance benchmark
7. failure分析
8. 修正
9. 再計測

性能FAIL時は「ほぼ達成」として終了しない。

全HARD条件を満たすまで継続する。

---

# 14. Gate 完了条件

# Gate 1 COMPLETE

以下がすべて PASS:

- Frozen GUI behavior
- filename/path correctness
- filename/path latency
- text content correctness
- normal content latency
- short/common query latency
- huge-hit-file navigation
- 1M file scale
- 10 GiB initial indexing
- 100 GiB initial indexing
- startup/restart
- filesystem incremental update
- Core corpus PersistentRatio ≤ 5.00%
- Ready memory
- search memory
- indexing memory
- idle CPU
- churn/recovery

1つでも FAIL があれば Gate 1 未完了。

Gate 1 完了後のみ Gate 2 へ進む。

---

# Gate 2 COMPLETE

以下がすべて PASS:

- DOCX content search
- XLSX content search
- PPTX content search
- PDF content search
- document location display
- document correctness
- mixed document corpus search latency requirements
- mixed document corpus PersistentRatio ≤ 5.00%
- restart/update/recovery
- Gate 1 regression all PASS

1つでも FAIL があれば Gate 2 未完了。

---

# 15. PersonalRag v1 最終完成条件

最終状態で以下がすべて PASS したときのみ完成。

```text
GUI FROZEN CONTRACT                    PASS

GATE 1
Filename/path correctness             PASS
Filename/path latency                 PASS
Text content correctness              PASS
Content normal-query latency          PASS
Content short/common-query latency    PASS
Huge-hit-file navigation              PASS
1,000,000 file scale                  PASS
Initial index 10 GiB <= 2 min         PASS
Initial index 100 GiB <= 15 min       PASS
Startup filename <= 2 sec             PASS
Startup content <= 3 sec              PASS
Filename update p95 <= 1 sec          PASS
Content update p95 <= 2 sec           PASS
Core persistent ratio <= 5.00%        PASS
Ready memory <= 2 GiB                 PASS
Search memory <= 3 GiB                PASS
Indexing peak <= 8 GiB                PASS
Idle CPU <= 1%                        PASS
Restart/recovery/churn                 PASS

GATE 2
DOCX                                  PASS
XLSX                                  PASS
PPTX                                  PASS
PDF                                   PASS
Document correctness                  PASS
Mixed-corpus performance              PASS
Mixed persistent ratio <= 5.00%       PASS
Gate 1 regression                     PASS

FUTURE EXTENSIBILITY
Natural-language layer addable
without deterministic core rewrite    PASS
```

すべて PASS の場合のみ:

`PERSONALRAG V1 COMPLETE`

と宣言してよい。

---

# 16. 最終成果物

Astra は完成時に最低限以下を残す。

- source code
- build instructions
- release build
- automated correctness tests
- benchmark generator / corpus procedure
- benchmark runner
- benchmark results
- PersistentRatio measurement tool
- Gate 1 acceptance report
- Gate 2 acceptance report
- architecture overview
- future natural-language extension boundary documentation

性能数値には、実測した machine / build / corpus / date を記録する。

---

# 17. 仕様の優先順位

矛盾がある場合:

1. この仕様書の HARD completion condition
2. Frozen GUI HTML
3. automated acceptance tests derived from this specification
4. Astra内部設計文書

Astra内部実装は仕様より優先されない。
