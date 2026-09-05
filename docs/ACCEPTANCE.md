# Astra v1 受け入れ台帳

正本: specification/PersonalRag_ASTRA_SPEC_v1.md / PersonalRag_GUI_FROZEN_v1.html。
仕様とモックは編集しない。全条件の実測 PASS までは完成宣言しない。

## 実装前に固定する判定

|検証|規模・条件|合格閾値|状態|
|---|---|---|---|
|GUI|正本の入力、列、選択、別々のフォーカスとキー、非同期・古い応答抑止|全動作一致|未実施|
|正確性|独立した直接走査oracle、Unicode/日本語/全mode/AND/空/0件|FP=FN=0|未実施|
|名前/パス|実ファイル1,000,000、warm|p50≤20/p95≤50/p99≤100ms|未実施|
|通常内容|100GiB、全固定query|p50≤50/p95≤100/p99≤200ms|未実施|
|短い高頻度内容|th/in/er/00/日本|p95≤200ms、max≤300ms|未実施|
|大量hit|同一ファイル50,000箇所、選択と上下/Home/End|初回p95≤100ms、GUI展開有界|未実施|
|初回構築|10GiB / 100GiB|120秒 / 900秒|未実施|
|起動|既存index、正常/強制終了後|名前≤2秒、内容≤3秒|未実施|
|変更|create/delete/rename/move/modify各独立測定|名前p95≤1秒、内容p95≤2秒|未実施|
|容量|10GiB/100GiB、全アプリ永続検索データ|≤5.00%|未実施|
|メモリ|process-private/commit|idle≤2GiB/search≤3GiB/build≤8GiB/update≤4GiB|未実施|
|idle|変更なし10分|平均CPU≤1%、不要な継続I/Oなし|未実施|
|churn|同一corpusに各10,000 create/modify/rename/delete、その後restart|正確性・容量・回復すべてPASS|未実施|
|Gate 2|DOCX/XLSX/PPTX/PDF、location、mixed性能/容量、回復、Gate 1回帰|全PASS|Gate 1完了まで着手しない|

正式性能測定はWindows 11 / Core Ultra 9 285H / 32GB / 内蔵NVMe / AC / Release。
machine、電源、ビルドSHA、corpus manifest/hash、query一覧、日時を結果に記録する。
First useful batchは最大100ファイル、先頭一致表示情報を含む。全件完了時間も別に記録する。
部分成功、タイムアウト、例外、0hit queryを結果から捨てない。性能改善時もcorpus/queryを変えない。

## 固定コーパス手順 v1

ユーザー指定のofficial corpusがある場合はそれを正本とする。
指定がなければseed=20260905の再現可能な生成手順を測定前に固定する。
内容はsource/log/CSV/JSON/日本語文章、ASCII/UTF8/UTF16LE/BEを混合し、数値・識別子・行長を変化させる。
10GiBと100GiBは実際の論理bytesで生成する（sparse fileや同一ファイルへのlinkで代用しない）。
名前スケールはUnicode/日本語/複数階層を含む実ファイル1,000,000個。
小規模開発試験は正式性能PASSの根拠にしない。
生成手順実装・ハッシュ固定後、性能結果を見て分布を変更してはならない。

内容通常query（全件保持）: PersonalRag, configuration, request_id, 日本語, ERROR, absent_ASTRA_9f23c751, regex `request_[0-9]+`, regex `^ERROR.*timeout`, wildcard `config*value`, wildcard `存在しない*終端`。
短いquery: th, in, er, 00, 日本。
名前query: 空, main, .txt, 日本語, *.json, file_000000?, source log, absent_ASTRA_9f23c751。
パスquery: source, 日本語, *.txt, source file, absent_ASTRA_9f23c751。
各queryにcase ON/OFF、warmup後最低100回、nearest-rank分位。検索順は固定seedでshuffle。

## 判定の記録

Gate 1: 未完了。Gate 2: 未着手。性能ベースライン: 実装前のため未測定。
