# PersonalRag Step 7 Windows targeted closure report

- 実施日: 2026-08-30
- 実行ID: 20260830-084705
- fresh clone: C:/Users/bokop/AppData/Local/PersonalRag/Step7-Closure-Fresh-20260830-084705
- 対象branch: reports/step7-windows-closure-20260830-084705
- 検証対象: GitHub bokoprin/PersonalRag の最新 main

## 最終判定

STEP7_COMPLETE

今回のtargeted closureは全項目を実施し、失敗・BLOCKEDはありませんでした。前回の実機E2Eで確定済みのproduct-path結果は、指示書記載の report commit `2ee23681f9f1a09864c421fa9e974fb003ea84af` から引き継ぎました。今回のclosureでは全面GUI/watch E2Eは再実行していません。

Step 7 IDの判定は PASS 9 / FAIL 0 / BLOCKED 0 / SKIP 0 です。内訳は、前回から継承した8件（S7-BUILD-001、S7-BUILD-002、S7-GUI-001、S7-INCREMENTAL-001、S7-INIT-001、S7-USN-001、S7-DOC-001、S7-CAPACITY-001）と、今回確認したS7-DOC-002です。PowerShell 5.1 capacity-tool anomalyはS7 IDとは別の検証ツール事象として CLOSED です。

## Fresh cloneとsource seal

- HEAD: `a668f73058f2966b2688909cfa6dc673e37d52a1`
- HEAD tree: `5aacff6bc19e12e2b769c5e819a1a280ced4206b`
- origin/main: `a668f73058f2966b2688909cfa6dc673e37d52a1`
- `git switch main`: PASS
- `git pull --ff-only origin main`: PASS（Already up to date）
- 初期 `git status --porcelain`: 空
- `SOURCE_MANIFEST.sha256`: `SOURCE_MANIFEST: 91/91 PASS normalized_legacy_crlf=68`
- manifest検証で表示されたlegacy CRLF警告は、検証スクリプトが許容する既存worktree表現であり、manifest判定はPASSです。

## Product-source diff guard

基準コミット `a33a32a81a344cdbdeee14431fe71a159afe2471` からHEADまでの `git diff --name-status` は以下でした。

- M AGENTS.md
- M README.md
- M SOURCE_MANIFEST.sha256
- A STEP7_LAST_TWO_FIX_CANONICAL_2026-08-30.md
- A STEP7_WINDOWS_TARGETED_CLOSURE_CODEX_2026-08-30.md
- A evidence/step7-last-two/final-summary.txt
- M tests/document_extraction.rs
- M tools/measure_product_capacity.ps1

`src/`変更、Cargo.toml/Cargo.lock/Cargo.*変更、build.rs変更は0件でした。期待されたdocument fixture / capacity-tool / verification-documentation範囲内であり、diff guardは PASS です。`TARGETED_RETEST_INVALID` には該当しません。

## Toolchainとhelper

- rustc: `rustc 1.97.1 (8bab26f4f 2026-07-14)`
- cargo: `cargo 1.97.1 (c980f4866 2026-06-30)`
- git: `git version 2.52.0.windows.1`
- `personalrag-v2-indexer.exe helpers`: pdftotext / native tar.exe zip_reader / zstd がすべてavailable=true
- 初期状態でpdftotextとzstdが未検出だったため、指示書で許可された `tools/setup_windows_helpers.ps1 -Install` のみ実行しました。Poppler由来pdftotext/zstdとWindows標準tar.exeが利用可能になりました。
- separate `zip.exe` は導入していません。

## Buildとtest gate

以下をfresh cloneで実行しました。検証中に製品source、tests、specification、Cargo files、SOURCE_MANIFEST.sha256は変更していません。

| command | result |
| --- | --- |
| cargo fmt --all -- --check | PASS |
| cargo clippy --offline --locked --all-targets -- -D warnings | PASS |
| cargo build --offline --locked --release | PASS |
| cargo test --offline --locked | PASS: 18 result lines, 89 passed, 0 failed |
| cargo test --offline --locked --test document_extraction -- --nocapture | PASS: 10/10 |
| cargo test --offline --locked --test windows_document_helper -- --nocapture | PASS: 1/1 |
| where.exe zip | exit 1, not found; expected/acceptable |
| personalrag-v2-indexer.exe helpers | PASS |

full cargo testは18個のtest result行を返し、合計89 passed、0 failed、0 ignored、0 measured、0 filteredでした。
`document_extraction` では `windows_fixture_zip_generation_uses_native_tar` がPASSし、`windows_document_helper` では `native_zip_reader_handles_verbatim_windows_document_paths` がPASSしました。Windows側fixture generatorは `tar.exe` を使用します。非Windows向けのcfg分岐に残るzipコマンド参照はWindows実行経路ではありません。

## Windows PowerShell 5.1 capacity closure

実行shellは `powershell.exe` で、versionは `5.1.26100.9278`、PSEditionはDesktopでした。実行したコマンドは次のとおりです。

`powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\tools\measure_product_capacity.ps1 -MiB "4,96,256" -Indexer ".\target\release\personalrag-v2-indexer.exe"`

出力は `CAPACITY_REQUEST mib=4,96,256` となり、3サイズとも `hard_gate=True` でした。

| MiB | init ratio | complete store ratio | hard gate |
| ---: | ---: | ---: | --- |
| 4 | 2.539754% | 5.087256% | True |
| 96 | 1.329789% | 2.659954% | True |
| 256 | 1.318659% | 2.637484% | True |

一時ディレクトリは通常測定の前後とも0件でした。

安全境界の確認として、次を実行しました。

`powershell.exe -NoProfile -ExecutionPolicy Bypass -File .\tools\measure_product_capacity.ps1 -MiB "496256" -Indexer ".\target\release\personalrag-v2-indexer.exe"`

結果は終了コード1で、`MiB must be between 1 and 1024: 496256` と明示的に拒否されました。実行前後の `PersonalRag-Capacity-*` ディレクトリ数は `0 -> 0`、新規作成数は0でした。したがって、PowerShell 5.1 capacity anomalyは CLOSED です。

## Step 7 closure status

- S7-BUILD-001: PASS（前回実機E2E結果を継承）
- S7-BUILD-002: PASS（前回実機E2E結果を継承）
- S7-GUI-001: PASS（前回実機E2E結果を継承）
- S7-INCREMENTAL-001: PASS（前回実機E2E結果を継承）
- S7-INIT-001: PASS（前回実機E2E結果を継承）
- S7-USN-001: PASS（前回実機E2E結果を継承）
- S7-DOC-001: PASS（前回実機E2E結果を継承）
- S7-CAPACITY-001: PASS（前回実機E2E結果を継承）
- S7-DOC-002: PASS（Windows document_extraction 10/10、windows_document_helper 1/1、zip.exe未導入）
- PowerShell 5.1 capacity-tool anomaly: CLOSED

## Final cleanlinessと公開物

テスト完了後、report公開前のsource checkoutで以下を確認しました。

- `git diff --check`: exit 0
- `git status --porcelain`: 0 lines
- HEADとorigin/main: 一致

公開branchにはユーザー指定に従い、レポート、統合ログ、結果ZIP、SHA256SUMSのみを配置します。統合ログには各コマンドとraw outputの結合記録を含め、結果ZIPにはAGENTS.md、targeted closure指示書、source seal、diff guard、toolchain、build/test、capacity、invalid input、cleanlinessのrawログを収録します。mainへのmerge/pushは行っていません。

AUTOPILOTとは異なる今回のtargeted closureでは、性能改善や製品コード変更は行っていません。

## 公開artifactの最終検証

初回公開後のWindows既定checkoutでは、統合ログがCRLFへ変換されるため、作成時のSHA256SUMSとcheckout後のログbytesに差が出ることを検出しました。これは製品検証結果ではなく、公開artifactの改行正規化問題です。ログをLF固定で再生成し、専用branchのcommitを更新しました。

その後、`core.autocrlf=false` の別fresh cloneで、専用branchのファイルがレポート・統合ログ・結果ZIP・SHA256SUMSの4件だけであること、3つの公開ファイルのSHA-256がすべて一致すること、ZIPの26エントリにsource/Cargoファイルが含まれないことを確認しました。remote mainは `a668f73058f2966b2688909cfa6dc673e37d52a1` のままです。

## Evidence

詳細なコマンドとrawログは同じディレクトリの `PersonalRag_STEP7_WINDOWS_CLOSURE_LOGS_20260830-084705.txt` と `PersonalRag_STEP7_WINDOWS_CLOSURE_RESULTS_20260830-084705.zip` に保存しています。各公開ファイルのSHA-256は `SHA256SUMS.txt` を参照してください。

最終結果: **STEP7_COMPLETE**
