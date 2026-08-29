# PersonalRag V2 Step 7 Windows実機最終E2E再検証

実施日: 2026-08-30（Asia/Tokyo）
実行結果: **`STEP7_NOT_COMPLETE`**

## 1. 結論

fresh cloneしたGitHub `main`上で、release build、初回index、native Win32 GUI、PDF/DOCX/XLSX/PPTX、通常権限watch、create/modify/rename/move/delete、explicit update、再起動、fail-closed recovery、容量測定を実行した。

製品の実行経路は確認できたが、`cargo test --offline --locked` と `--test document_extraction` の2テストが、fixture生成処理の `Command::new("zip")`（`tests/document_extraction.rs:99`）で `program not found` となった。ユーザー指定どおり、未許可のzip導入やソース修正は行っていない。この未解決FAILのため、Step 7全体は完了判定しない。

| ID | 判定 | 要約 |
|---|---|---|
| `S7-BUILD-001` | PASS | fresh checkout、`SOURCE_MANIFEST` 98/98、変更なし |
| `S7-BUILD-002` | PASS | Rust 1.97.1、fmt / clippy / release build成功、両exe存在 |
| `S7-GUI-001` | PASS | native GUI検索・Preview・More・Open・Explorer・resizeを確認 |
| `S7-INCREMENTAL-001` | PASS | explicit updateとGUI Reloadで新規markerを検索可能 |
| `S7-INIT-001` | PASS | 製品exeのinit/statusとGUI loadが成功 |
| `S7-USN-001` | PASS | 通常権限で`mode=directory-notify`、5操作すべてWATCH_UPDATE後に検索可能 |
| `S7-DOC-001` | PASS | 許可されたhelper導入後、製品4形式init/statusが成功 |
| `S7-DOC-002` | FAIL | 製品GUIの4形式検索は成功したが、document_extraction 2/9がzip不足でFAIL |
| `S7-CAPACITY-001` | PASS | complete store ratio: 4 MiB 5.087256%、96 MiB 2.659954%、256 MiB 2.637484% |

判定件数: **PASS 8 / FAIL 1 / BLOCKED 0 / SKIP 0**。未実施項目をPASSにはしていない。

## 2. 実行場所・source integrity

- Remote: `https://github.com/bokoprin/PersonalRag.git`
- fresh clone: `C:\Users\bokop\AppData\Local\PersonalRag\Step7-Final-Fresh-20260830-060120`
- 既存の`C:\shinsuke\app\PersonalRag` cloneは再利用していない。
- 起点branch: `main`
- HEAD: `a33a32a81a344cdbdeee14431fe71a159afe2471`
- HEAD tree: `490b98c77dd3d57c29052124a440aa3a8d6f9e76`
- `AGENTS.md`: 42行、2387 bytes、SHA-256 `B9F39A89DA00ED13CA83FAC6D673D0E590DACE233EB39F2EBBB765BE6E927470`
- `STEP7_WINDOWS_FINAL_RETEST_CODEX_2026-08-30.md`: 328行、9069 bytes、SHA-256 `E8A322B24E4A86217C685D9E4CE6F0941A657D37079FE5884FBED2B899337CFC`
- `SOURCE_MANIFEST.sha256`: `SOURCE_MANIFEST: 98/98 PASS normalized_legacy_crlf=0`
- 事前・最終pre-publishの`git status --porcelain`: 空。
- 製品ソース、tests、仕様書、Cargoファイル、`SOURCE_MANIFEST.sha256`は変更していない。
- `git diff --check`: exit 0。
- `OPENAI_API_KEY` / `CODEX_API_KEY`: 存在なし（値は取得・記録していない）。

## 3. Windows / build gate

実機はWindows 10 Home、build `26200`、64-bit。`git 2.52.0.windows.1`、`rustc 1.97.1`、`cargo 1.97.1`。

- `cargo fmt --all -- --check`: PASS
- `cargo clippy --offline --locked --all-targets -- -D warnings`: PASS
- `cargo build --offline --locked --release`: PASS
- `target\release\personalrag-v2-gui.exe`: exists
- `target\release\personalrag-v2-indexer.exe`: exists

テストの実測は以下のとおり。

- full `cargo test --offline --locked`: exit 101。24 unit tests PASS、document_extractionは7 PASS / 2 FAIL。実行された非zero test caseは33件中31 PASS / 2 FAIL。
- `cargo test --offline --locked --test document_extraction -- --nocapture`: 7 PASS / 2 FAIL。
- `cargo test --offline --locked --test windows_document_helper -- --nocapture`: 1 PASS / 0 FAIL。
- FAIL箇所はいずれも`tests/document_extraction.rs:99`のfixture生成用`zip`起動で、`Error { kind: NotFound, message: "program not found" }`。
- `where.exe zip`: exit 1。未許可の別ソフトウェアは導入していない。

## 4. helper

初回確認では`pdftotext`と`zstd`が不足し、native `C:\Windows\System32\tar.exe`は存在した。ユーザーが許可した唯一の導入操作として、リポジトリ内の次だけを実行した。

```powershell
powershell -ExecutionPolicy Bypass -File .\tools\setup_windows_helpers.ps1 -Install
```

導入後のhelperは次のとおり。

- `pdftotext`: Poppler 25.07.0
- ZIP reader: `C:\Windows\System32\tar.exe` / bsdtar 3.8.8
- `zstd`: Zstandard CLI 1.5.7

製品の実行ではPATHのシェル更新に依存せず、上記の明示パスを`PERSONALRAG_PDFTOTEXT`、`PERSONALRAG_UNZIP`、`PERSONALRAG_ZSTD`へ渡した。Git/MSYSの`unzip.exe`は選択していない。

## 5. disposable corpusと初回index

全データはfresh clone外のC:上に置いた。

- lifecycle root: `...\Step7-Final-Evidence-20260830-060120\step7-data\corpus`
- lifecycle store: `...\step7-data\store`
- document root: `...\step7-data\document-corpus`
- document store: `...\step7-data\document-store`
- 初回lifecycle corpus: 135 files / 3,007,632 bytes（>100 wildcard対象を含む）
- fixture: 実体のあるPDF、DOCX、XLSX、PPTX。renamed plain textではない。

小さい4形式fixtureだけでの最初のdocument initは、製品の容量hard SLOにより`persistent index capacity 1029.781% exceeds 10% hard SLO`で拒否された。このログを保存した後、fixture rootへ検証専用4 MiB paddingを追加し、再実行した。

最終的な製品init/status:

```text
INIT_OK bundle=1 metadata=145 searchable=135 store_bytes=162033 usn_available=false journal_id=0 next_usn=0
STATUS_OK bundle=1 content=1 metadata=1 delta=1 state=1 metadata_records=145 delta_changes=0
```

document rootは次で成功した。

```text
INIT_OK bundle=1 metadata=7 searchable=5 store_bytes=118285 usn_available=false journal_id=0 next_usn=0
STATUS_OK bundle=1 content=1 metadata=1 delta=1 state=1 metadata_records=7 delta_changes=0
```

## 6. native Win32 GUI

GUIはrelease `personalrag-v2-gui.exe`を実起動し、アクセシビリティツリーとnative screenshotを確認した。代表的なGUI表示時間は製品画面のsearch status値であり、手動計測からp50/p95等は推定していない。

- filename `alpha` → `alpha.txt`, `text\alpha.txt`
- Full path ONのroot相対`text\alpha.txt` → 正しい1件。絶対filesystem pathは0件だった。
- Case sensitive OFFの`MIXEDCASENAME.TXT` → `MixedCaseName.TXT`
- Case sensitive ONの同じwrong-case → 0件
- literal `PR_STEP7_FINAL_ALPHA_7F41` → alpha、`Line 1 · byte 0`
- file=`alpha` AND content=`PR_STEP7_FINAL_日本語_9A73` → alpha、`Line 4 · byte 0`
- regex `ERROR_[0-9]{4}` → alpha / bravoの2件
- wildcard `PR_STEP7_FINAL_BULK_*` → 125件
- Unicode `PR_STEP7_FINAL_日本語_9A73` → alpha / `日本語ファイル.txt`の2件
- More: shared markerが127件へ展開され、limitが145へ増加
- Preview: alpha選択後、`PR_STEP7_FINAL_ALPHA_7F41`を含む内容を確認
- Open: Sakura Editorの`alpha.txt` windowが開いた
- Show in Explorer: `text - エクスプローラー` windowが開いた
- maximize/restore: ボタン表示が`最大化`から`元のサイズに戻す`へ変わり、GUIは応答した
- typing: native keyboardのHome → Shift+End → type → Returnで応答した

代表的なGUI status表示は、初回alpha 0.0 ms、create後10.5 ms、modify後6.6 ms、rename後0.4 ms、move後0.5 ms、delete後4.3 msだった。これは各Reload後の検索statusである。

## 7. PDF / DOCX / XLSX / PPTX

製品のdocument rootで4形式をinitし、native GUIで一意markerを検索した。現在のGUI契約どおりlocationはgeneric `Unit 1 · byte 0`である。

| 形式 | marker | 結果 | GUI status |
|---|---|---|---|
| PDF | `PR_STEP7_FINAL_PDF_ONLY_64C4` | `fixture.pdf` / `docs\fixture.pdf` / Unit 1 | 1 files · 2092.1 ms |
| DOCX | `PR_STEP7_FINAL_DOCX_ONLY_71D1` | `fixture.docx` / `docs\fixture.docx` / Unit 1 | 1 files · 778.1 ms |
| XLSX | `PR_STEP7_FINAL_XLSX_ONLY_82E2` | `fixture.xlsx` / `docs\fixture.xlsx` / Unit 1 | 1 files · 1102.3 ms |
| PPTX | `PR_STEP7_FINAL_PPTX_ONLY_93F3` | `fixture.pptx` / `docs\fixture.pptx` / Unit 1 | 1 files · 759.1 ms |

製品exe経路の4形式検索、file name、logical-unit locationはPASSである。一方、repository testのdocument fixture生成だけが`zip.exe`不足でFAILしたため、`S7-DOC-002`はFAILとした。失敗を製品PASSへ読み替えていない。

## 8. normal-user live watch

watchは管理者昇格なしのPowerShell（`isAdministrator=false`）で実行した。

```text
WATCH_READY mode=directory-notify journal_id=0 next_usn=0 interval_ms=250 fallback_reason=Some("I/O error: アクセスが拒否されました。 (os error 5)")
```

USN raw-volumeアクセスが拒否されたためfallbackが選択されたが、指示書で許可された正式モードであり、管理者昇格は要求していない。watchを稼働したまま、各操作で`WATCH_UPDATE`を待ち、explicit updateを使わずGUI Reloadしてから検索した。

| 操作 | WATCH_UPDATE検知 | 検索確認 |
|---|---:|---|
| create | 283.2 ms | `live-created.txt` / `PR_STEP7_FINAL_CREATE_NEW_7E55` |
| modify | 275.7 ms | 新marker `PR_STEP7_FINAL_MODIFY_NEW_5D44`あり、旧markerなし |
| rename | 279.1 ms | `rename\after-rename.txt`あり、旧pathなし |
| move | 557.1 ms | `text\after-move.txt`あり、旧pathなし |
| delete | 295.5 ms | 旧filename / 旧markerともに0件 |

全5操作でwatch publish、GUI Reload、新状態検索、stale state消失を確認した。`S7-USN-001`はdirectory-notify modeでPASSとした。

## 9. explicit update / restart / recovery

watch停止後、検証専用`explicit-update.txt`を追加して次を実行した。

```text
UPDATE_OK committed=true compacted=true bundle=7 metadata=146 delta_changes=0
```

GUI Reload後に`PR_STEP7_FINAL_EXPLICIT_UPDATE_A41B`が検索でき、`S7-INCREMENTAL-001`はPASSとした。

GUIを閉じ、indexer status、GUI再起動、Reload、alpha marker検索を実行した。statusは次のとおりで、再起動watchも`mode=directory-notify`で`WATCH_READY`となった。

```text
STATUS_OK bundle=7 content=4 metadata=7 delta=7 state=7 metadata_records=146 delta_changes=0
```

fail-closed recoveryではstoreを検証専用copyへ複製し、copy内の最新generation `gen-00000000000000000004.prv2`だけを破損させた。originalのSHA-256は維持された。製品statusはbundle 6へfallbackし、alpha markerは検索可能、bundle 7で追加したexplicit markerは0件だった。破損した最新generationの内容を未検証のまま返していない。

## 10. complete product capacity

指定コマンドをWindows PowerShell 5.1で実行したところ、既存スクリプトの`[Array]::Fill[byte](...)`が5.1でparseできずexit 1となった。この実行結果はPASS扱いにしていない。スクリプト・製品・sourceは変更せず、既に存在したPowerShell 7を使い、同じスクリプトを`-MiB @(4,96,256)`として正しい配列で実行した。

| MiB | changed files | final source bytes | complete store bytes | complete ratio | hard gate |
|---:|---:|---:|---:|---:|---|
| 4 | 1 | 4,194,304 | 213,375 | 5.087256% | PASS |
| 96 | 2 | 100,663,296 | 2,677,597 | 2.659954% | PASS |
| 256 | 6 | 268,435,456 | 7,079,943 | 2.637484% | PASS |

4 MiB以上のcomplete persistent store / sourceはいずれも10%以下である。誤ったPowerShell引数渡しにより一時的に`496256` MiB相当を生成しかけたが、即時停止し、TEMP配下の検証専用root（46,186 files / 48,429,531,136 bytes）を対象パス確認後に全削除した。容量スクリプトによる正しい4/96/256 MiB実行後、一時capacity directoryは残っていない。

## 11. latency / memory / anomalies

- lifecycle初回init: 220.838 ms、status: 20.607 ms
- GUI processのrestart観測: responding=true、private memory 5,427,200 bytes、working set 20,504,576 bytes
- live watch event検知: 275.7–557.1 ms（5操作）。OS cache等を分離するベンチマークではなく、Step 7操作確認の観測値である。
- PDF/Office検索statusは上記表に記録。手作業の代表値のみで、分位点は作っていない。
- anomaly: Windows PowerShell 5.1と容量scriptのgeneric method syntax不一致。
- anomaly: 外部test fixture生成の`zip.exe`不足。Poppler/native tar/zstd導入後もこのtest専用依存は存在しなかった。
- anomaly: USN raw-volume access拒否。正式なdirectory-notify fallbackでlive matrixは成功。
- `INVALID_ENVIRONMENT`、corpus change、未承認API key検出はなし。

## 12. evidence

このreportと同一runのraw command output、stdout/stderr、fixture manifest、helper状態、GUI/document/lifecycle観測、corruption copy情報、capacity結果を`PersonalRag_STEP7_WINDOWS_E2E_RESULTS_20260830-060120.zip`へ格納した。ZIP内の`PACKAGE_SCOPE.txt`に収録範囲を記録した。

主なraw証跡:

- `fresh-preflight.json`
- `verify-source-manifest-final.txt`
- `cargo-test-full-with-helpers-result.json`
- `helpers-after-install-result.json`
- `indexer-init-initial-result.json` / `indexer-status-initial-result.json`
- `indexer-init-documents-with-padding-result.json`
- `gui-document-lifecycle-observations.json`
- `watch-normal-start.json`、`watch-op-*.json`、`watch-normal.stdout.txt`
- `indexer-update-explicit-result.json`
- `corruption-recovery.json`
- `measure-product-capacity-4-96-256-corrected-result.json`

## 13. 最終判定

mainは変更していない。report branchにはこの検証成果物だけを保存する。

残存するFAILは、repositoryのdocument extraction testがfixture生成用の`zip.exe`をハードコードしているため実行できないこと。製品の実helper経路と実4形式GUI検索は成功したが、全テストゲートを満たしていない。

したがって最終結果は次のとおり。

```text
STEP7_NOT_COMPLETE
```
