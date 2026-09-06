# PersonalRag Filename/Path Search 正式受入れ計画（統合版）

この文書は、`docs/CODEX_FILENAME_FORMAL_REMEASURE.md` と Filename/Path Search
hardening 指示書を実行する際の補強ルールをまとめたものです。既存の HARD 条件、
filesystem identity、Unicode full case fold、wildcard substring semantics、corpus、
oracle、測定対象、完了判定は変更しません。対象は `PersonalRag.sln` と
`src/FilenameSearch.*` の production path に限定します。

## AC 条件の優先順位

この計画の AC override は、`CODEX_FILENAME_FORMAL_REMEASURE.md` に AC 必須または
Power: AC と記載されていても、今回の正式受入れに限って優先します。AC 未接続でも
compile、regression、correctness、performance、GUI、idle、persistence、multi-volume
を開始・継続し、ACLineStatus、battery/charging status、power scheme、取得可能な
throttling の兆候を各 report に記録します。override の対象は電源状態だけです。
latency、memory、build、load、GUI、idle、correctness その他の HARD gate は維持し、
バッテリー駆動を FAIL の免除理由にしません。threshold 超過は通常どおり FAIL です。

## 正式 benchmark の固定方法

正式 query benchmark は seed `123456`、決定的 shuffle、20 rounds で実行します。
最初の 2 rounds は warmup、残り 18 rounds を正式集計対象とし、p50/p95/p99/max と
worst-class percentile は 18 rounds の全 sample から算出します。route または query
class ごとに round/warmup を変えず、結果を見て増減せず、遅い sample を除外せず、
seed や query 順を変更しません。SUSTAINED_SEARCH の 10,000 query 以上と actual GUI
の fresh process 10 件以上は、それぞれの固有 sample 定義を優先しますが、seed と
決定的順序は固定します。

## 受入れ条件の不変性

Codex は Human owner の明示指示なしに HARD threshold、Filename/FullPath semantics、
case semantics、Unicode vectors、wildcard semantics、query class、1M filesystem
entry、logical 100 GiB、corpus difficulty、oracle、FP/FN=0、churn 件数・時間、GUI
process 数、corruption fixture、post-update state、completion 判定を変更・緩和・
除外しません。runner、oracle、harness の bug は難易度を下げない最小修正だけ行い、
修正理由、before/after、diff path/hash、影響範囲を
`reports/filename-hardening/FORMAL_SERIES_INVALIDATIONS.json` に記録します。修正後は
lock と source commit を更新し、旧 formal series を破棄して全測定を最初から再実行します。

## inaccessible entry

task 専用 directory で元 ACL を保存・復元できる環境では deny ACL fixture を実施し、
access denied 後も catalog/search が停止せず、inaccessible subtree を skip または
reconcile して accessible な結果を維持することを確認します。安全な ACL fixture を
作成・復元できない環境では `informational_limitation` として記録し、formal NOT_RUN
には数えません。この limitation だけで `evaluation_complete` または `pass` を false
にしません。既存 HARD fixture を環境上の難しさだけで格下げしません。completion を
許容する正式 NOT_RUN は `NOT_RUN_NO_SECOND_FIXED_VOLUME` だけです。

## Formal series lock と remote

lock 作成前に status、fetch、remote SHA、source SHA を確認し、source、runner、oracle、
corpus、query、measurement script、acceptance rules を commit します。lock 作成後は
automatic pull、merge、rebase、force push を行わず、lock の source commit で series を
完走します。remote が進んだ場合は SHA を記録して source を変えず、完走後に push 前の
差分を明示確認します。production または formal inputs に影響する remote 変更を統合
する場合は新しい source commit、lock を作り、全 formal series を再実行します。
reset --hard、無断 rebase、force push で既存変更を消しません。

`FORMAL_SERIES_LOCK.json` には少なくとも次を保存します。

```json
{
  "benchmark_rounds": 20,
  "warmup_rounds": 2,
  "measured_rounds": 18,
  "query_shuffle_seed": 123456,
  "AC_requirement_overridden": true,
  "initial_remote_branch_sha": "...",
  "formal_source_commit_sha": "...",
  "threshold_definition_hash": "...",
  "acceptance_rule_hash": "..."
}
```

lock と全 report には executable、source/runner/script の hash、corpus manifest、query
set、independent oracle、normalizer/casefold version、machine、電源状態、command、
exit code、raw metrics、threshold、pass/fail、limitation/exception reason を記録します。
