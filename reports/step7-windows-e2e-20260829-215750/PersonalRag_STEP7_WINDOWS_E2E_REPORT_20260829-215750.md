# PersonalRag V2 Step 7 Windows実機E2E再検証レポート

## 結論

判定は STEP7_NOT_COMPLETE です。指定された8件の再判定は次のとおりです。

| Issue ID | 判定 | 要約 |
|---|---|---|
| S7-BUILD-001 | FAIL | clean checkoutでもsource manifestの.gitignore SHA-256が一致しない |
| S7-BUILD-002 | PASS | Windows clippyが-D warningsで成功 |
| S7-GUI-001 | PASS | 実Win32 GUIのファイル名、内容、AND、regex、wildcard、Unicode検索が期待結果 |
| S7-INCREMENTAL-001 | PASS | explicit update後にcreate/modify/rename/move/deleteとネストpathを正しく反映 |
| S7-INIT-001 | PASS | release indexerの製品initが新規root/storeで成功 |
| S7-USN-001 | FAIL | watchがアクセス拒否で起動せず、publish未確認 |
| S7-DOC-001 | BLOCKED | pdftotextとzstdが利用できず、required helper provisioningを完了できない |
| S7-DOC-002 | BLOCKED | document initとdocument extraction testがhelper不足/path解決エラーで実行不能 |

8件の集計は PASS=4 / FAIL=2 / BLOCKED=2 / SKIP=0 です。未実施項目をPASSにはしていません。

残存する主要な不具合・阻害要因は、source manifest不一致、通常権限でのUSN watch起動失敗、文書helper不足/Windows path解決、完全な永続storeの容量SLO超過です。したがってStep 7の完了判定はできません。

## 実施範囲と不変条件

- 実施日時: 2026-08-29（日本時間）
- 実施環境: ユーザーの実Windows上のnative PowerShell、release executable、実Win32 GUI
- C:\Program Files\は読み取り専用で扱った
- index、store、corpus、raw logはsource checkout外の使い捨てパスに置いた
- 製品ソース、テスト、仕様書、Cargoファイル、SOURCE_MANIFEST.sha256は変更していない
- FAIL/BLOCKEDを検出しても修正していない
- APIキーは値を出力せず存在有無だけ確認し、OPENAI_API_KEY=false、CODEX_API_KEY=false
- 検証中のsource checkoutでのgit status --porcelainは空だった

証跡ルートは C:\Users\bokop\AppData\Local\PersonalRag\Step7-20260829-215750、使い捨てcorpus/storeルートは C:\Users\bokop\AppData\Local\Temp\PersonalRag-Step7-20260829-215750 です。

## Git、指示書、環境

- origin: https://github.com/bokoprin/PersonalRag.git
- fetch後のorigin/main: 5ecb941010d2822befc7dd1ccace89b8be040171
- 検証HEAD: 5ecb941010d2822befc7dd1ccace89b8be040171
- 検証ツリー: d0e0dcf49f0b079e72b63720a5f2ea79e0fb23c4
- 結果branch: reports/step7-windows-e2e-20260829-215750
- AGENTS.md: 41行、2280 bytes、SHA-256 CCB7123BC36D04A4CFDA63BF3578873F9EDD939401276A329796A12A5CB715A4
- STEP7_WINDOWS_RETEST_CODEX_2026-08-29.md: 379行、11843 bytes、SHA-256 1A6533DC904950818C37DEEDACACBB7F872DD4985FFEB36C92144114D56BD65D
- Windows: Windows 10 Home / Version 2009 / OS build 26200 / x64
- Rust: rustc 1.97.1 (8bab26f4f 2026-07-14)
- Cargo: cargo 1.97.1 (c980f4866 2026-06-30)
- Git: git version 2.52.0.windows.1
- C: はNTFS
- 実行tokenはMedium Mandatory Level、PowerShellは非昇格

## Preflightとbuild gate

| チェック | 判定 | 実測 |
|---|---|---|
| clean checkout | PASS | git status --porcelainが空 |
| latest main取得 | PASS | origin/mainと検証HEADが一致 |
| source manifest | FAIL | .gitignore expected=77bd56e8931d992f68bd54ed63af38a1c6876005a91c40344615f3d5e3d1a05a、actual=ef201bddb5a37dc649a3f2eca9b13f4bfc2e1a968981e93d99a207eac152eaec |
| cargo fmt --all -- --check | PASS | exit code 0 |
| cargo clippy --offline --locked --all-targets -- -D warnings | PASS | exit code 0、約3.26秒 |
| cargo test --offline --locked | FAIL | exit code 101。unit 24 passed、document_extraction 9 failed |
| cargo build --offline --locked --release | PASS | exit code 0、約26.05秒 |

cargo testの24 unit testは成功しました。9件の失敗は文書helperのprogram not foundと、それに続く文書verification前提の失敗です。完全な出力はZIP内のraw/cargo-test-rerun.txtに保存しています。

release binary:

- personalrag-v2-gui.exe: 910,848 bytes、SHA-256 48E194B3AAF91FF2C73D6D74AE85F888D80286767E71F335F53090BA2F97E4F3
- personalrag-v2-indexer.exe: 978,944 bytes、SHA-256 5E85D6CDE8B1BCB7F6FE1A932051DB2AB7146B0776F599C9ED2BB7CD2FA813EA

## Corpusとfixture

ライフサイクル検証には C:\Users\bokop\AppData\Local\Temp\PersonalRag-Step7-20260829-215750\corpus を使用しました。初期状態は132 searchable files、1,196,967 bytesで、最終状態は132 files、1,197,572 bytesです。最終状態の差分はcreate 1件とdelete 1件が相殺されず、+605 bytesとなっています。

document fixtureはcorpusとは分離した document-corpus\docs に置きました。

- fixture.pdf: PDF 1.4 bytesを手動生成し、xrefを含む。headerは37 80 68 70 45 49 46 52
- fixture.docx: System.IO.Compression.ZipArchiveでOOXML packageを生成
- fixture.xlsx: OOXML workbook、sharedStrings、worksheetを含む
- fixture.pptx: OOXML presentation、slide、relationshipを含む

DOCX/XLSX/PPTXはunzip -tでNo errors detectedまで確認しました。いずれも単なる拡張子変更ではなく、各検索対象contentに固有markerを入れています。fixture構造検証はPASSですが、製品経路での抽出・GUI検索はhelper阻害によりBLOCKEDです。

## 初回indexとstatus（S7-INIT-001）

corpusを小さくした最初の診断用initでは、persistent index capacity 936.080% exceeds 10% hard SLOでexit code 2になりました。この試行は容量不足の診断であり、受入判定用corpusには使っていません。

padding後に同じrelease executableで実行した製品経路は次のとおりです。

    INIT_OK bundle=1 metadata=139 searchable=132 store_bytes=147884 usn_available=false journal_id=0 next_usn=0
    STATUS_OK bundle=1 content=1 metadata=1 delta=1 state=1 metadata_records=139 delta_changes=0 journal_id=0 next_usn=0

新規root/storeからの初回indexとstatusはPASS、S7-INIT-001はPASSです。初回出力にusn_available=false、journal id 0が記録されています。

## 実Win32 GUI検索・操作性

タイトル PersonalRag V2 — Local Universal Grep のGUIをnative UI automationで起動し、以下を確認しました。

| 操作 | 判定 | 実測 |
|---|---|---|
| filename alpha | PASS | alpha.txt、text\alpha.txt、0.1 ms |
| relative path text/alpha.txt | PASS | text\alpha.txtに正規化して1件 |
| filename case-insensitive | PASS | MIXEDCASENAME.TXTでMixedCaseName.TXT |
| filename Case sensitive negative | PASS | wrong caseで0件 |
| Literal shared content | PASS | alpha、bravoの2件、3.1 ms |
| filename AND content | PASS | alpha 1件 |
| Regex ERROR_[0-9]{4} | PASS | alpha、bravoの2件、30.4 ms |
| Wildcard + More | PASS | 100件からMoreで125件、243.0 ms |
| 日本語content | PASS | alpha 1件、Line 5、previewにmarker |
| content Case sensitive negative | PASS | pr_step7_日本語検索_9a73で0件 |
| preview | PASS | 選択行の名前・相対path・検索markerを表示 |
| Open | PASS | Sakura editorがcorpus内のtext\alpha.txtを開いた |
| Show in Explorer | PASS | Explorerがtext folderを開いた |
| Reload index | PASS | update後にReady、bundle 2へ更新 |
| restart後の代表検索 | PASS | Ready bundle 2、共有token 2件、4.4 ms |
| リサイズ/input応答 | PASS | resize gesture後もhang/errorなし |
| 通常DPI表示 | PASS | native画面でlabel、表、previewの切れを観測せず |

Full path checkboxの検証は、製品の相対path契約に合わせてtext/alpha.txtを使いました。絶対root文字列は検索対象pathではないため0件となり、製品不具合とは判定していません。

## PDF / DOCX / XLSX / PPTX（S7-DOC-001、S7-DOC-002）

helper discoveryの実測:

    pdftotext available=false path=pdftotext.exe error="program not found"
    unzip available=true path=C:\Program Files\Git/usr/bin/unzip.exe
    zstd available=false path=zstd.exe error="program not found"

setup scriptもinstallなしで実行し、HELPER pdftotext=MISSING、HELPER zstd=MISSING、unzip=PASS、exit code 2でした。-Installは使っていません。外部helperを勝手に導入して結果を変えないためです。

指定どおり実行した結果:

- cargo test --offline --locked --test document_extraction -- --nocapture: exit code 101、9/9 failed
- product init --root document-corpus\docs --store document-store: exit code 2
- initの実エラー: helper C:\Program Files\Git/usr/bin/unzip.exe failed with status 9、cannot find or open \?C:Users...fixture.docx

従ってhelper availabilityはBLOCKED、文書製品経路の初期化・unique markerのGUI検索はBLOCKEDです。未実施の文書検索をPASSにしていません。

## NTFS create / modify / rename / move / delete、watch、explicit update

C:はNTFSで、fsutil usn queryjournal C:ではUSN Journal IDが存在しました。しかし、指定コマンド:

    .\target\release\personalrag-v2-indexer.exe watch --root "$Root" --store "$Store" --interval-ms 250

はexit code 2、stdoutは空、stderrは次のとおりでした。

    ERROR: I/O error: アクセスが拒否されました。 (os error 5)

WATCH_READY、WATCH_UPDATEは観測していません。再起動後にも同じroot/storeでwatchを再起動し、exit code 2と同じアクセス拒否を再現しました。ジャーナル自体は存在するため、通常の非昇格実行でnative executableがlive journalを読み、publishするというS7の条件を満たしておらず、S7-USN-001はFAILとしました。管理者昇格での再試験はUACを発生させるため実施していません。

watchとは独立して、使い捨てcorpusで次の変更を実施しました。

1. text\created-live.txtをcreate
2. modify\modify-me.txtをPR_STEP7_MODIFY_OLD_5C33からPR_STEP7_MODIFY_NEW_5D44にmodify
3. rename\before-rename.txtをrename\after-rename.txtにrename
4. move\before-move.txtをtext\after-move.txtにmove
5. delete\delete-me.txtをdelete

その後の指定explicit updateは次のとおりです。

    UPDATE_OK committed=true compacted=true bundle=2 metadata=139 delta_changes=0
    STATUS_OK bundle=2 content=2 metadata=2 delta=2 state=2 metadata_records=139 delta_changes=0 journal_id=0 next_usn=0

update後にGUIをReloadし、createの新marker、modifyの新marker/旧marker消失、renameの新名/旧名消失、moveの新path/旧path消失、deleteのfilename消失を確認しました。explicit updateに限定した実製品ライフサイクルのネストpath mappingはPASS、S7-INCREMENTAL-001はPASSです。

explicit updateは操作完了後に約333.948 msでした。watchが起動しなかったため、live change-to-searchable latencyは測定不能です。

## Restart / Reload / fail-closed recovery

- GUIをnative close buttonで終了し、GUI processが0件: PASS
- 同じroot/storeでrelease GUIを再起動: PASS、PID 7216、Responding true
- 再起動後CLI status: PASS、bundle 2
- 再起動後GUI代表検索: PASS、共有token 2件、4.4 ms
- Reload index: PASS

fail-closed確認では製品storeを直接壊さず、store-successをrecovery-storeへコピーしました。コピー側だけのgen-00000000000000000002.prv2（103322 bytes、SHA-256 7DFEF98F80759487E124ACBC24A49FF07C37E67199B77EDDB1EB1E8A20A03B4C）を12 bytesの不正データへ置換しました。

コピー側に対するstatusは:

    STATUS_OK bundle=1 content=1 metadata=1 delta=1 state=1 metadata_records=139 delta_changes=0 journal_id=0 next_usn=0

有効な旧bundle 1へfallbackし、壊れたbundle 2の内容を未検証のまま返していないため、fail-closed recoveryはPASSです。製品storeは変更していません。

## 性能・容量・操作性

docs/PERFORMANCE_SLO.mdのhard first useful batchは300 ms、完全なpersistent footprintのhard capacityは10%以下です。

| 指標 | 実測 |
|---|---:|
| 初回source | 132 files / 1,196,967 bytes |
| 初回initのstore_bytes | 147,884 bytes |
| update後source | 132 files / 1,197,572 bytes |
| update後store全ファイル | 295,652 bytes / 16 files |
| update後persistent/source | 24.6876% |
| GUI working set | 24,186,880 bytes |
| GUI private memory | 6,074,368 bytes |
| GUI launch-to-usable | 約1.8秒以内の観測 |
| explicit update | 333.948 ms |

GUIの手作業による代表latencyはalpha filename 0.1 ms、shared literal 3.1 ms、regex 30.4 ms、bulk wildcard 243.0 ms、日本語25.7 ms、restart後shared 4.4 msです。これは個別観測値であり、p50/p95/p99ではありません。bulkの初回100件はhard 300 ms内ですがpreferred 100 msを超えています。

完全なstore footprintはupdate後24.6876%で、10% hard capacity SLOを超過しています。これは追加の容量不具合S7-CAPACITY-001として記録します。

## FAIL / BLOCKEDの再現情報

### S7-BUILD-001

ID: S7-BUILD-001
Severity: Blocker
Area: source manifest / clean Windows checkout
Preconditions: latest origin/main、clean checkout、normal Windows checkout
Steps to reproduce: powershell -ExecutionPolicy Bypass -File .\tools\verify_source_manifest.ps1
Expected: manifest verificationが成功し、manual line-ending repair不要
Actual: .gitignore expected=77bd56e8931d992f68bd54ed63af38a1c6876005a91c40344615f3d5e3d1a05a、actual=ef201bddb5a37dc649a3f2eca9b13f4bfc2e1a968981e93d99a207eac152eaec
Evidence/log: raw/verify_source_manifest.txt、raw/instructions-and-revision.json
Reproducible: 同じclean checkoutで再現

### S7-USN-001

ID: S7-USN-001
Severity: High / Blocker
Area: native Windows USN watch producer
Preconditions: C: is NTFS、fsutil usn queryjournal C:でjournal存在、normal non-elevated process
Steps to reproduce: 指定のrelease indexer watch --root ... --store ... --interval-ms 250を起動
Expected: WATCH_READY、journal ID/next USN、変更後WATCH_UPDATEと検索可能なpublish
Actual: exit code 2、stdout空、I/O error: アクセスが拒否されました (os error 5)、publishなし
Evidence/log: raw/watch-launch.json、raw/watch.stderr.txt、raw/watch-after-restart.json、raw/watch-after-restart.stderr.txt、raw/usn-queryjournal.txt、raw/identity-and-privilege.txt
Reproducible: 同じ通常権限条件で再現

### S7-DOC-001

ID: S7-DOC-001
Severity: High
Area: PDF/Office helper provisioning
Preconditions: valid fixture 4種、指定setup script、installなしのhelper discovery
Steps to reproduce: indexer helpers、続けてtools/setup_windows_helpers.ps1を-Installなしで実行
Expected: required helperが解決され、path/versionを記録できる
Actual: pdftotextとzstdがprogram not found、setup exit code 2。unzipはGit版が存在するがproduct path解決も失敗
Evidence/log: raw/helpers-before-setup.txt、raw/document-indexer-init.txt
Reproducible: 同一環境で確認

### S7-DOC-002

ID: S7-DOC-002
Severity: High
Area: document extraction、product init、GUI marker search
Preconditions: valid PDF/DOCX/XLSX/PPTX fixture、外部source変更なし
Steps to reproduce: document rootでproduct init、cargo test --offline --locked --test document_extraction -- --nocaptureを実行
Expected: 4文書をindexし、固有marker、preview/locationを確認
Actual: product init exit code 2（Git unzipに\?C:Users... malformed path）、document test 9/9 failed。GUI検索は前提未充足のため未実施
Evidence/log: raw/document-indexer-init.txt、raw/cargo-test-document-extraction.txt、raw/cargo-test-rerun.txt、raw/fixture-validation.txt
Reproducible: 同一環境で確認

### S7-CAPACITY-001（追加）

ID: S7-CAPACITY-001
Severity: High
Area: complete persistent footprint capacity SLO
Preconditions: normal operation後のstore全ファイルを計上
Steps to reproduce: update後のstore-success全ファイルを合計し、source bytesと比較
Expected: persistent/source <= 10%
Actual: 295,652 / 1,197,572 = 24.6876%
Evidence/log: raw/performance-observations.json、raw/store-inventory-before-recovery.json、corpus-file-manifest-final.json
Reproducible: この使い捨てcorpusで確認

## Measurement methodology

- コード変更なしの最新main release buildを使用
- 初回indexはrelease indexerの製品init、statusは製品status
- GUIは実Win32 windowでnative操作
- Step 7の初回/update/restart観測であり、warm/cold専用benchmarkではない
- search latencyはGUI statusに表示された単発のfirst-result観測値
- p50/p95/p99は生成していない
- corpusは実行中に作成した使い捨てNTFS corpus、storeはsource checkout外
- store全ファイルのsizeを容量比較に使用
- corrupt recoveryはstore copyだけで実施
- source manifest、環境、fixture、store artifactのSHA-256をrawへ保存

## Measurement anomalies / limitations

- S7-BUILD-001: clean checkoutなのに.gitignore manifest mismatch
- cargo test: document helper不足により9 test failures
- S7-DOC-001/002: pdftotext、zstd missing、Git unzip product path error
- S7-USN-001: USN journalは存在するが、初回／再起動後の非昇格watchはいずれもaccess denied
- cache/I/Oの複数回統計は取得していないため、単発GUI値を性能改善とは解釈していない
- live watch change-to-searchable latencyは測定不能
- 完全store footprintは容量SLO超過

## 最終状態

このbranchにはレポート、コマンド一覧、主要ログ集約、結果ZIP、SHA-256一覧だけを保存します。mainは変更していません。検証終了時のsource checkout git status --porcelainは空であり、レポート作成後はこのreport-only directoryだけが意図したGit差分です。
