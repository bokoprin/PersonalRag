# 構造と意味論

ゼロから実装し、旧PersonalRagのコード・index形式との互換性は要求しない。

- `Astra.Core`: deterministic search / indexing / persistence / runtime。GUIや将来のLLM層に依存しない。
- `Astra.Cli`: index/search/hits/ratio/inspectの外部境界。
- `Astra.Gui`: Frozen GUIに対応するWPF UI。検索はworker実行、query世代とcancelで古い応答を棄却。
- `Astra.Bench`: 再現可能corpus、build、fresh-process load/search、update latency、churn、recovery、idle計測。

## 抽出境界

`ITextExtractor -> IEnumerable<TextUnit(Location, Text)>` を唯一の文書抽出境界とする。

- Plain text: `Line N`
- DOCX: `Paragraph N`（header/footer/footnote/endnoteも対象）
- XLSX: `SheetName!Cell`
- PPTX: `Slide N`
- PDF: `Page N`

Open XML (DOCX/XLSX/PPTX) はZIP/XMLから直接抽出。PDFはPdfPigのtext layer抽出を使用する。画像のみPDFのOCRはv1対象外。
検索/indexing層は文書形式を知らないため、将来の形式追加で検索engineを変更しない。

Office/PDFはsource解析コストがplain textより高いため、SearchEngine側の`TextExtractor`だけが384MiB上限のLRU cacheを持つ。
cacheはRAMのみで永続5%の分子には影響せず、size/mtimeが変われば無効化する。抽出前後でsize/mtimeが変化した文書はcacheへ投入しない。32MiB超の単一文書はcache materializeしない。
破損文書やbinary textなど個別抽出失敗は `Unsearchable` として隔離し、他ファイルのindex/searchを継続する。

## 検索

ファイル名/パス:

- Ordinal / OrdinalIgnoreCase
- 空白区切りAND
- `*` / `?` wildcard
- Filename / FullPath scope

内容:

- Literal / Regex / Wildcard
- logical unit境界内で照合
- 2/3文字signatureは候補除外専用
- signatureで候補になった後、元ファイルの抽出textで必ず最終照合
- 候補filterはfalse positiveを許すがfalse negativeを作らない

1ファイル1行。初回rowではHit総数を未確定の下限として返せる。選択後に`GetHits`で32件単位に列挙し、必要時だけ全件countする。

## 永続形式 ASTRA003

- 256 files / block
- blockごとにBrotli圧縮
- header SHA256 + 各compressed block SHA256
- load時はblockを並列展開
- `FileEntry.Signature` は展開blockの `ReadOnlyMemory<byte>` sliceを参照し、signature配列をfileごとに複製しない
- tempへwrite/flushしてからcommit
- writer.lockで複数writerを排他
- crashで残った正規形式tempは次回正常save後に削除

正式メモリ測定ではbuildとload/searchを**別プロセス**で実行する。build後のGC heap reservationをReady RAMへ混ぜない。

## 継続更新

`FileSystemWatcher`でcreate/change/delete/renameをdirty pathとして集約する。overflow、directory change、restart時はreconcileへfallback。
reconcileはsize/mtimeが不変なら既存signatureを再利用する。reparse pointは再帰しない。アクセス不能directoryは全体indexingを停止させない。

検索はsnapshot世代単位。元ファイルが消えた/変化した陽性候補は最終照合で古い結果を返さない。新内容がsnapshotへ公開されるまでの遅延はupdate-latency acceptanceで測る。

## 将来自然文検索

自然文/LLM/embeddingはv1に実装しない。
将来層は `IDeterministicSearch` を呼び出すquery planner/retrieverとして追加する。
既存filename/content index、deterministic semantics、Frozen GUIの基本検索を作り直さず追加できることを境界条件とする。
