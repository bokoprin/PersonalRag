# Content Search Bake-off 実装メモ

## 実装範囲

`CONTENT_SEARCH_BAKEOFF_DESIGN.md` の方式選定フェーズとして、以下4 backend を同一 interface で実装した。

- `scan`: bounded parallel direct scan
- `bloom`: block + 256 byte Bloom filter + exact verification
- `trigram`: packed Unicode trigram + disk-backed delta-varint postings + exact verification
- `sqlite-fts5`: SQLite FTS5 trigram + exact verification

共通部として以下を実装した。

- plain-text extension filter
- UTF-8 / UTF-16 BOM / CP932 detection
- binary heuristic
- overlap付きblock extraction
- Unicode 15.1 full casefoldを利用するsubstring exact verifier
- line / column / snippet
- regex verification
- localized `ApplyChangesAsync`
- backend diagnostics
- smoke corpus
- fixed query set / independent expected-count oracle
- 20 rounds / 2 warmup / 18 measured の bake-off runner

## 位置づけ

これは production Content Engine の完成実装ではなく、4方式を同一条件で実測するための bake-off implementation。

まだ以下は production hardening 対象外。

- FilenameCatalog typed change feed への接続
- generation / durable delta / compaction の正式実装
- crash recovery / writer lease / checksum
- GUI
- PDF / Office
- semantic search / embedding / RAG

## 実行

```powershell
dotnet restore ContentSearch.Bakeoff.sln
dotnet build ContentSearch.Bakeoff.sln -c Release --no-restore
powershell -ExecutionPolicy Bypass -File scripts/Run-ContentSearchBakeoff.ps1
```

実 corpus:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/Run-ContentSearchBakeoff.ps1 `
  -Root D:\content-corpus `
  -Work D:\content-bakeoff-work `
  -Report D:\content-bakeoff-work\summary.json `
  -Backend all
```

## 公平性

4方式共通で `ContentExactVerifier` を通す。候補生成方式が異なっても最終結果の query semantics は共有する。

smoke corpus では `oracle-v1.json` の独立 expected count と Direct Scan の result signature の両方を使って cross-check する。
