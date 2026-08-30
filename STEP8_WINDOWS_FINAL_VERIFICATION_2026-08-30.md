# PersonalRag Step 8 Windows 最終確認指示書

Date: 2026-08-30  
Target branch: `step8-zero-config-continuous-indexing-20260830`  
Purpose: Step 8 の自動回帰完了後に、実Windows環境で Win32 / NTFS / USN / zero-config GUI の最終確認を行う。

## 0. 判定ルール

- **実行していない項目を PASS にしない。**
- 失敗時はソースをその場で修正せず、HEAD、コマンド、標準出力/標準エラー、再現条件を保存する。
- 最初は通常ユーザー権限でGUIを確認する。管理者権限はNTFS/USN専用E2Eのために使用してよい。
- `tests/windows_step8_e2e.rs` はファイルを作成・変更・削除するため、**必ず使い捨てNTFSボリューム**で実行する。

## 1. Fresh clone と対象固定

PowerShell:

```powershell
git clone <PersonalRag repository URL> PersonalRag-Step8-Windows-Final
cd PersonalRag-Step8-Windows-Final
git switch step8-zero-config-continuous-indexing-20260830
git pull --ff-only

git rev-parse HEAD
git status --short
```

記録するもの:

- `HEAD`
- Windows version (`winver` または `Get-ComputerInfo`)
- `git status --short` が空であること

## 2. Rust / helper確認

Rust は **1.97.1** を使用する。

```powershell
rustup toolchain install 1.97.1 --profile minimal --component rustfmt --component clippy
rustup override set 1.97.1
rustc --version
cargo --version
```

PDF/Office検索まで確認する場合のみ、必要に応じて既存のhelper setupを使用する。

```powershell
powershell -ExecutionPolicy Bypass -File .\tools\setup_windows_helpers.ps1
```

不足している場合だけ、リポジトリ既存手順に従って `-Install` を使用する。

## 3. Windows full regression

以下をすべて実行する。

```powershell
cargo fmt --all -- --check
cargo clippy --offline --locked --all-targets -- -D warnings
cargo test --offline --locked
cargo build --offline --locked --release
```

期待結果:

- format error 0
- clippy warning/error 0
- Rust test failure 0
- release build success

1つでも失敗したら、この時点で `STEP8_TARGET_WINDOWS_COMPLETE` にはしない。

## 4. 専用NTFS VHDを作成

**管理者PowerShell**を開く。既存ドライブを使わないこと。

以下は `R:` を使う例。`R:` が既に存在する場合は空いているドライブ文字へ変更する。

```powershell
$Vhd = Join-Path $env:TEMP 'personalrag-step8-e2e.vhdx'
Remove-Item -LiteralPath $Vhd -Force -ErrorAction SilentlyContinue

$Diskpart = @"
create vdisk file="$Vhd" maximum=512 type=expandable
select vdisk file="$Vhd"
attach vdisk
create partition primary
format fs=ntfs label=PRSTEP8 quick
assign letter=R
exit
"@

$DiskpartFile = Join-Path $env:TEMP 'personalrag-step8-create.txt'
Set-Content -LiteralPath $DiskpartFile -Value $Diskpart -Encoding ASCII
diskpart /s $DiskpartFile

fsutil fsinfo volumeinfo R:
fsutil usn createjournal m=67108864 a=8388608 R:
fsutil usn queryjournal R:
```

期待結果:

- `R:` がNTFS
- USN Journalが作成済み
- `fsutil usn queryjournal R:` がJournal ID等を返す

## 5. Native NTFS / USN Step8 E2E

同じ管理者PowerShellで:

```powershell
$env:PERSONALRAG_STEP8_E2E_VOLUME = 'R:\'
cargo test --offline --locked --test windows_step8_e2e -- --ignored --nocapture --test-threads=1
```

このテストは自動的に以下を検証する。

1. 初回Metadata/Content build → `Ready`
2. NTFS USN checkpoint が `Valid`
3. 起動中Create
4. Modify
5. Rename
6. Delete
7. Metadata差分反映
8. Dirty Content catch-up
9. 旧Contentの即時stale化
10. 新Contentの再index
11. shutdown
12. 停止中のfilesystem変更
13. restart
14. durable USN checkpointからcatch-up
15. 最終的に `Ready` / dirty=0

期待結果:

```text
test native_ntfs_usn_continuous_indexing_survives_restart ... ok
```

## 6. VHD cleanup

E2E終了後に必ず実行する。

```powershell
Remove-Item Env:PERSONALRAG_STEP8_E2E_VOLUME -ErrorAction SilentlyContinue

$Diskpart = @"
select vdisk file="$Vhd"
detach vdisk
exit
"@
$DiskpartFile = Join-Path $env:TEMP 'personalrag-step8-detach.txt'
Set-Content -LiteralPath $DiskpartFile -Value $Diskpart -Encoding ASCII
diskpart /s $DiskpartFile
Remove-Item -LiteralPath $Vhd -Force -ErrorAction SilentlyContinue
```

## 7. Zero-config GUI 実機確認

ここからは**通常ユーザー権限**で実行する。

```powershell
.\target\release\personalrag-v2-gui.exe
```

`--root` / `--store` は指定しない。

必須確認:

1. GUIが即座に表示される。
2. 固定ローカルドライブが自動検出される。
3. `%LOCALAPPDATA%\PersonalRag` 配下をアプリ自身が管理する。
4. 既存published indexがある再起動では、background catch-up完了前でも既存検索が利用できる。
5. 初回buildではFilename/Path検索がContent完成より先に利用可能になる。
6. GUI操作中にbackground indexingが走っていても検索UIが固まらない。

## 8. 実ドライブでlive update確認

検索対象の固定ドライブに専用テストフォルダを作成する。既存データを使わない。

例:

```powershell
$TestDir = 'C:\PersonalRag-Step8-Final-Test'
New-Item -ItemType Directory -Force -Path $TestDir | Out-Null
Set-Content "$TestDir\alpha.txt" 'alpha-step8-live-needle'
```

GUIで以下を順番に確認する。

### Create

- `alpha.txt` がFilename検索へ自動反映
- `alpha-step8-live-needle` がContent検索へ自動反映

### Modify

```powershell
Set-Content "$TestDir\alpha.txt" 'beta-step8-live-needle-with-different-size'
```

確認:

- 古い `alpha-step8-live-needle` が消える
- 新しい `beta-step8-live-needle` が現れる
- 全Content rebuildを待つ必要がない

### Rename

```powershell
Rename-Item "$TestDir\alpha.txt" 'renamed.txt'
```

確認:

- `alpha.txt` が消える
- `renamed.txt` が現れる
- 内容を変えていないためContent検索は維持される

### Delete

```powershell
Remove-Item "$TestDir\renamed.txt"
```

確認:

- Filename結果から消える
- Content結果からも消える

## 9. 停止中変更 → restart catch-up

1. GUIを通常終了する。
2. 停止中にファイルを追加する。

```powershell
Set-Content "$TestDir\offline.txt" 'offline-step8-final-needle'
```

3. GUIを再起動する。
4. 既存indexが利用可能であることを確認する。
5. background catch-up後に `offline.txt` と `offline-step8-final-needle` が検索可能になることを確認する。
6. 手動の「再構築」操作が不要であることを確認する。

## 10. Recovery確認

可能なら以下を1回ずつ実施する。

### Content catch-up途中終了

- 内容変更を複数作る
- `ContentCatchUp`中にPersonalRagを終了
- 再起動
- 完成済みshardは利用可能なまま、残作業のみ収束すること

### Metadata/Content state reuse

- 一度 `Ready` まで到達
- 再起動
- 起動しただけで全Metadata/全Content rebuildが始まらないこと

### Access Denied

- 読めないディレクトリが存在しても、他volume/他directoryの検索が継続すること
- 全アプリが停止しないこと

## 11. 長時間・maintenance確認

最低でもmaintenance interval（30秒）を越えて数分間起動する。

確認:

- idle時に継続的な高CPU使用がない
- PersonalRag自身のstate書き込みを拾い続ける自己更新ループがない
- disk I/Oが永久に高止まりしない
- dirty countが0へ収束する
- shard数が更新のたび無制限に増え続ける挙動が見えない
- 検索応答性がbackground maintenanceで大きく悪化しない

## 12. 最終clean確認

```powershell
Remove-Item -Recurse -Force $TestDir -ErrorAction SilentlyContinue
git status --short
```

ソースcheckoutがcleanであること。

## 13. 結果記録テンプレート

```text
PersonalRag Step8 target-Windows final verification
HEAD:
Windows version:
User token: normal / elevated

Windows full regression: PASS / FAIL
Native NTFS USN E2E: PASS / FAIL
Zero-config GUI launch: PASS / FAIL
Fixed-volume discovery: PASS / FAIL
Initial Metadata-first availability: PASS / FAIL
Live Create: PASS / FAIL
Live Modify + stale suppression: PASS / FAIL
Live Rename + content reuse: PASS / FAIL
Live Delete: PASS / FAIL
Stopped-change restart catch-up: PASS / FAIL
ContentCatchUp restart/recovery: PASS / FAIL
Access Denied isolation: PASS / FAIL
Idle/maintenance behavior: PASS / FAIL
GUI responsiveness during indexing: PASS / FAIL
Final git clean: PASS / FAIL

Observed issues:
- none / details

Final:
STEP8_TARGET_WINDOWS_COMPLETE / STEP8_TARGET_WINDOWS_FAILED
```

全必須項目がPASSした場合のみ `STEP8_TARGET_WINDOWS_COMPLETE` とする。
