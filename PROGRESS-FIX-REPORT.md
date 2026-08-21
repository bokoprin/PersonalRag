# PersonalRag Progress Fix Report — 2026-08-15

## 症状

Windows GUIで12,992 filesのrebuildが約41秒で正常完了する一方、index進捗が途中でほぼ更新されず、最後に突然100%へ変化していた。

原因はGUI描画停止ではなく、Portable Coreのpipeline buildが大きなhydration batch全体を読み終えてからのみ`on_progress`を呼んでいたこと。`segment_docs=5000`ではhydration batchが最大10,000 pathsとなるため、I/Oに時間がかかるWindows実データでは数十秒progress snapshotが変化しないケースがある。

また、全file hydration完了後もQ2/POS sidecar、verify、publishが残っているのにfile countersだけで100%表示になり得た。terminal stateとphase表示の食い違いが見えるケースもあった。

## 実装

### 1. observed parallel hydration

`search-core/src/builder.rs`のdisk hydrationを局所リファクタリング。

- worker -> coordinatorの結果返却を64 files chunk化
- coordinatorは128 filesまたは100 msごとにprogress callback
- `processed_files`、`bytes_read`、`current_path`をhydration中に更新
- segment size / build batch sizeは維持
- cancel flagをworker loop内でも確認
- workers=1でも一時Vecをfileごとに作らず同じrecord pathを共有

このため表示更新のためだけにsegmentを細分化せず、build throughputを維持する。

### 2. frontend finalization semantics

`frontend/src/background_state.ts` / `main.ts`:

- active rebuildでfile counterがtotalへ到達しても99%にcap
- 100%は`state=completed`のみ
- finalization中は残り時間を0秒と断定せず計算中扱い
- `completed/cancelled/failed`ではterminal stateをphase表示の正本とする

## 回帰 / correctness

- Search Core fmt: PASS
- Search Core clippy `-D warnings`: PASS
- Search Core unit: 3/3 PASS
- Search Core production: 29/29 PASS
  - 新規: large hydration batch内でintermediate progress snapshotが出ることを検証
- release build: PASS
- SELF_TEST_PASS
- GUI bridge clippy `-D warnings`: PASS
- GUI bridge tests: 4/4 PASS (large filesystem stress 1件は既存どおりignored)
- frontend TypeScript typecheck (Tauri API stub): PASS
- frontend progress logic targeted check: PASS
- Windows target `x86_64-pc-windows-gnu` search-core check/clippy: PASS
- Windows target `x86_64-pc-windows-gnu` bridge-core check/clippy: PASS

Windows test executableのcross-linkはこの環境にMinGW linker/dlltoolが無いため未実施。ただしWindows-specific Rust cfgのtype/lint gateは通過している。Tauri側Rust code自体は今回変更していない。

## A/B

10,000 small text files / 2 segments / 4 scan workers / 4 build workers。

5-run median:

- Performance Pass 1: 43.100 ms
- Progress Fix: 35.711 ms
- median ratio: 1.207x

進捗細粒度化によるperformance regressionは観測されず、このsynthetic cached workloadではむしろ改善した。

同一10k corpusのbaseline / modified index directoryは全regular filesでbyte-identical。

## Windowsで期待する表示

以前:

`0 files -> 数十秒ほぼ変化なし -> 100%`

修正後:

`128 / 12992 -> 256 / 12992 -> ... -> 12800 / 12992 -> 99% finalizing -> completed 100%`

実際の更新間隔はfilesystem速度と350msのfrontend poll周期で間引かれるため、画面上では128件ごとすべてが見えるわけではないが、長時間snapshotが固定される問題は解消される設計。
