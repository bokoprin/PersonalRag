# PersonalRag Office Extraction + vNext Index Benchmark

Date: 2026-08-17

## 結論

OfficeファイルはTXTより明確に重い。ただし、今回の分離計測では **vNext index生成そのものはTXT / DOCX / XLSX / PPTXでほぼ同程度**であり、差の主因はOfficeOpenXmlExtractorのZIP/DEFLATE/XML処理と、現行full-buildでOffice抽出結果を一度spoolへ書いて再読込する処理だった。

設計上の最重要ポイントは以下。

1. vNext indexerをOffice専用に最適化する優先度は低い。
2. Office extractionはファイル単位で独立しており、現状のserial prepare loopをbounded parallel化する価値が高い。
3. 多数の小Officeファイル、特に内部partの多いPPTX/XLSXで固定費が大きい。
4. 現行spool方式は多数小ファイルで追加コストが目立つため、抽出cache / producer-consumer / spool再利用を設計候補にする。
5. このbenchmarkはテキスト主体のsynthetic OOXMLでwarm-cache計測。画像・動画を多く含む実Officeは現在 `fs::read(path)` でcontainer全体を読むため、実データではさらに悪化する可能性がある。

## Methodology

最新 `FurtherAccelerated` sourceに再現可能な2ツールを追加した。

- `scripts/generate-office-index-benchmark-corpus.py`
- `search-core/examples/office_extraction_index_bench.rs`

Office containerはPython `zipfile` の DEFLATE level 6で生成。現在の `OfficeOpenXmlExtractor` をそのままbenchmark exampleへ取り込み、実装中のmanual ZIP parser / raw DEFLATE / XML text extractorを測定した。

### Corpus profiles

- `single`
  - DOCX: document.xml 1 part
  - XLSX: worksheet 1 part
  - PPTX: slide 1 part
- `multipart`
  - DOCX: document/header/footer/comments/footnotes/endnotes = 6 parts
  - XLSX: 8 worksheets
  - PPTX: 10 slides + 10 notes = 20 parts

各形式は同じsemantic text量を持つ。Office XMLは各text lineをOOXML風タグで包み、ZIPはDEFLATE圧縮した。

### Size scenarios

- many-small: 1000 files × 4 KiB = 約3.9 MiB semantic text / format
- medium: 200 files × 24 KiB = 約4.7 MiB / format
- few-large: 64 files × 64 KiB = 4.0 MiB / format

release build、warm-up後5回のp50。Linux containerは5 vCPU (`AMD EPYC 9V74`)。

### Metrics

- `extract_ms`: TXTはraw read + ASCII normalize、Officeは現在のOfficeOpenXmlExtractor + normalize。
- `index_ms`: 抽出済みnormalized contentから `.prseg2` を生成。形式固有の抽出処理は含めない。
- `production_like_ms`: 現行full-buildに近づけ、Officeは extract → spool write → spool read → vNext index、TXTはsource read → vNext index。

## Representative result: 200 files × 24 KiB

### single part

| Format | Extract | Extract vs TXT | vNext index | Index vs TXT | Production-like | Total vs TXT |
|---|---:|---:|---:|---:|---:|---:|
| TXT | 1.49 ms | 1.0x | 73.42 ms | 1.00x | 75.22 ms | 1.00x |
| DOCX | 24.45 ms | 16.5x | 79.20 ms | 1.08x | 113.17 ms | 1.50x |
| XLSX | 33.77 ms | 22.7x | 78.94 ms | 1.08x | 124.03 ms | 1.65x |
| PPTX | 26.60 ms | 17.9x | 82.49 ms | 1.12x | 118.01 ms | 1.57x |

### multipart

| Format | Extract | Extract vs TXT | vNext index | Index vs TXT | Production-like | Total vs TXT |
|---|---:|---:|---:|---:|---:|---:|
| TXT | 1.54 ms | 1.0x | 74.96 ms | 1.00x | 81.09 ms | 1.00x |
| DOCX | 32.00 ms | 20.8x | 74.20 ms | 0.99x | 113.90 ms | 1.40x |
| XLSX | 43.62 ms | 28.4x | 80.75 ms | 1.08x | 131.84 ms | 1.63x |
| PPTX | 51.84 ms | 33.7x | 83.85 ms | 1.12x | 134.21 ms | 1.66x |

`index_ms` は形式差が小さく、Officeの追加時間はほぼ前段にある。

## Many-small result: 1000 files × 4 KiB

多数小ファイルでは固定費とpart数の影響が顕著。

### multipart production-like

| Format | Extract | vNext index | Production-like | Total vs TXT |
|---|---:|---:|---:|---:|
| TXT | 3.74 ms | 72.94 ms | 73.70 ms | 1.00x |
| DOCX | 62.27 ms | 74.80 ms | 160.65 ms | 2.18x |
| XLSX | 80.86 ms | 77.52 ms | 177.65 ms | 2.41x |
| PPTX | 148.97 ms | 83.31 ms | 245.41 ms | 3.33x |

PPTXは同じ約4 MiBのsearchable textでも、20 internal parts × 1000 filesにするとTXTの約3.3倍になった。

## Few-large result: 64 files × 64 KiB

総semantic text量は約4 MiBで同程度だが、ファイル数を64件へ減らすとproduction-like差は縮小。

multipart:

- DOCX: 85.81 ms vs TXT 71.74 ms = 約1.20x
- XLSX: 100.95 ms = 約1.41x
- PPTX: 98.89 ms = 約1.38x

これは **総text量だけでなく、ファイル数 × internal part数がOffice extractionの重要な説明変数**であることを示す。

## What is actually slow?

### vNext indexer

同じ約4〜5 MiBのextracted contentに対して概ね60〜85 ms。Office形式による差はほぼない。Office用の特殊index設計は不要と判断できる。

### Office extractor

TXT raw readはwarm-cacheで1〜4 ms程度だったのに対し、Officeは20〜149 ms。現在のextractorには以下のコストがある。

- container全体の `fs::read`
- ZIP central-directory parse
- included XML entryごとのmanual raw-DEFLATE
- XML tag scan / entity decode
- output String assembly
- part labelの追加

特に現行 `xml_to_text()` は各XML tagでASCII lowercase Stringを生成するため、cell/run/tag数の多いXLSX/PPTXでは改善余地がある。

### Spool

Office full-buildは extracted textを一旦spool fileへ書き、index phaseで再読込する。200×24KiBでは数〜十数ms程度、多数小ファイルではさらに目立った。抽出そのものほど支配的ではないが、無視できない。

## Design implications for the next phase

今回の結果だけから優先順位を付けるなら次の順が妥当。

1. **bounded parallel Office extraction**
   - 現在 `prepare_full_build_inputs()` はfile loopがserial。
   - ファイルごとのextractは独立しているため最も低リスクで高ROI。
2. **Office extraction cache**
   - key候補: path identity + size + modified_ns + `INGESTION_VERSION`。
   - unchanged Officeをfull rebuildのたびに再DEFLATE/XML parseしない。
3. **spool設計の見直し**
   - first buildではbounded producer-consumer、再buildではcacheをspoolとして再利用する案が有力。
4. **XML hot-loop最適化**
   - per-tag `to_ascii_lowercase()` allocationを除去。
   - OOXML既知tagをbyte/prefix比較する。
5. **DEFLATE高速化**
   - manual bit-by-bit Huffman decodeのtable/lookahead化、またはvendor可能なら実績あるinflate implementationを比較。
6. **container全読込の見直し**
   - 実Officeがmedia-heavyなら、central-directory + included entryだけseek/readする方式を検討。

ただし 4〜6 の詳細設計へ入る前に、実際のユーザーOfficeファイル群または画像を含むsynthetic corpusで一度追加測定する価値がある。今回のtext-only synthetic OOXMLはCPU extraction差の比較には適しているが、media-heavy OfficeのI/Oは過小評価する。

## Validation

Benchmark tooling追加後:

- Search Core regression: 106/106 PASS
- `cargo fmt --check`: PASS
- `cargo clippy --all-targets -- -D warnings`: PASS
- release examples build: PASS
- `SELF_TEST_PASS`: PASS
- Python corpus generator syntax check: PASS

Production search semantics / `.prseg2` formatには変更なし。追加したのはbenchmark toolingのみ。
