# PersonalRag Content Search Architecture Bake-off 設計

## 目的

Plain Text Content Search のproduction方式を決めるため、同一corpus・同一query・同一exact verifier・同一oracleで4方式を比較する。

1. Bounded Parallel Direct Scan
2. Block q-gram Bloom Filter + Exact Scan
3. Block Trigram Inverted Index + Exact Scan
4. SQLite FTS5 Trigram

Bake-offでは4方式すべてをproduction hardeningしない。方式選定後、勝者だけをFilenameCatalog typed change feed、generation、durable delta、compaction、GUIへ統合する。

## 共通contract

```text
IContentSearchBackend
  BuildAsync(ContentCorpus)
  SearchAsync(ContentQuery)
  ApplyChangesAsync(ContentChange[])
  GetDiagnostics()
```

全方式で候補生成後に同一 `ContentExactVerifier` を通し、最終結果は `FP=0 / FN=0` を要求する。

## 共通text layer

- plain-text extension whitelist
- UTF-8 / UTF-8 BOM
- UTF-16 LE / BE BOM
- ASCII
- CP932 / Shift_JIS
- binary heuristic
- block extraction
- overlapによるblock境界跨ぎ保護
- line / column / snippet
- substring case-sensitive
- substring Unicode full casefold
- regex

初期blockは約64K decoded chars、overlap 256 chars。比較時は全方式で固定する。

## A: Bounded Parallel Direct Scan

indexを保持せず、対象fileをbounded worker poolでstream scanする。

```text
Query -> file list -> bounded read/decode -> exact verification
```

役割はbaseline、fallback、small corpus候補。persistent indexは原則0。更新時のindex rebuildは不要。

## B: Block q-gram Bloom Filter

各blockにtrigram存在Bloom Filterを持つ。

```text
Query -> trigram -> Bloom candidate block -> exact verification
```

初期filterは256 bytes/block。Bloom false positiveは許容するがfalse negativeは禁止。1/2文字・regex等はscan fallback。変更fileだけfilterを追加し、旧blockをinactive化する。

## C: Block Trigram Inverted Index

Unicode trigramをpacked `ulong` keyとして、keyからblock postingsを引く。

```text
Query -> trigram -> rarest-first postings intersection -> exact verification
```

base postingsはdelta-varint encoded disk blob。RAMにはkey directoryとsmall deltaを持つ。高頻度gramはbase postingを省略し、安全なfallbackを許可する。1/2文字は初期実装ではscan fallback。

## D: SQLite FTS5 Trigram

ContentBlockをSQLite FTS5 trigram rowとして格納する。

```text
FTS5 candidate -> common exact verification
```

SQLite側はWAL / synchronous=NORMALを固定。ASCII 3文字以上のsubstringをFTS候補生成に使い、full Unicode casefold・short query・regexは安全側へfallbackする。updateは対象file rowのみdelete/insertする。

## Corpus

正式方式比較では最低3種を用いる。

- Source / Config型: 小file多数、5～10GiB
- Log型: 中～大file、20GiB
- Huge-file型: 10～100 files、合計10GiB

## Query class

- rare ASCII
- common ASCII
- zero-hit
- Japanese
- Unicode casefold
- hex ID
- GUID-like
- path-like
- 3-char / 2-char / 1-char
- regex fixed literal
- regex no literal
- deep-position hit
- many-hit

## Benchmark

検索は固定seedで20 rounds、2 warmup、18 measured。

記録:

- initial build time
- peak / Ready private bytes
- persistent bytes
- first-result latency
- full-result p50/p95/p99/max
- bytes read
- candidate / verified blocks
- fallback count
- update convergence
- write amplification
- cancellation latency
- FP/FN

仮目標:

```text
FP=0
FN=0
rare p95 <= 50ms
general substring p95 <= 100ms
first useful result <= 100ms
additional Ready RAM <= 512MiB
cancel <= 100ms
single-file updateでfull corpus rebuild = 0
```

## Production移行

勝者決定後の形:

```text
FilenameCatalog
   -> CatalogChangeBatch
Content Engine
   -> Extractor
   -> Selected Backend
   -> Exact Verifier
   -> IContentCatalog
   -> GUI / future RAG
```

Content側で独自FileSystemWatcherを持たない。FileKeyをidentityにし、Renamed/Movedだけでは原則content reindexしない。Filename `SourceGeneration` と Content `IndexedGeneration` は分離する。

## Bake-off完了条件

- 4方式が同一APIで動作
- 3 corpus typeを実測
- query set固定
- independent oracleでFP/FN=0
- build/search/update/memory/diskを比較
- raw report保存
- WINNER / SECONDARY / REJECTEDを数値根拠付きで決定
