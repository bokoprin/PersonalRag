# 構造と意味論

ゼロから実装する。既存PersonalRagのコードやindexとの互換性は要求しない。

- Astra.Core: SearchRequest → 非同期のSearchPage、選択ファイル内HitPage。GUIや将来のplannerに依存しない。
- Astra.Cli: index/search/verify/benchmarkの再現可能な境界。
- Astra.Gui: Windows GUI。検索はworker実行、query世代とcancelで古い応答を棄却。
- 抽出器: ITextExtractorがlogical locationとtextを列挙。Gate 1はtextのみ。Gate 2を後から追加。
- 保存: version付きスナップショット、payload SHA256、tempへの書込み/flush後にatomic replacement。失敗は旧indexを残す。検索用の元テキストコピーは永続保持しない。
- 更新: FileSystemWatcherでdirty pathを集約。overflow時は再照合。再起動時は既存snapshotを先に提供し、裏で差分照合。
  SearchEngineはある世代のsnapshotを検索する。変更検知と新しいsnapshot公開の間は新内容が候補に入らない場合があり、仕様の変更反映時間で測る。陽性候補は常に元ファイルで照合する。

名前はUnicodeのOrdinalIgnoreCase（case ONではOrdinal）。空白区切りAND、wildcard tokenは名前/フルパス全体にanchor。通常tokenはsubstring。
内容はlogical lineごとに照合し、改行を越えない（GUI正本と同じlocation境界）。Literalは重ならない一致、Regexは.NETのCultureInvariant、Wildcardは `*`→`.*`, `?`→`.`。空の内容条件は名前検索のみ。Regex構文エラーは明示する。
Hitsは一致箇所数。初回pageでは途中の下限を表示でき、完了と区別する。1ファイル1行。
読み取れない/不正encoding/binaryは検索不能状態と理由を保持。これを検索成功と扱わない。

将来の自然文層はSearchRequestを生成して既存コアを呼ぶ。embedding等は別の上位retrieverとして追加し、deterministic semanticsやindex形式を変更しない。
