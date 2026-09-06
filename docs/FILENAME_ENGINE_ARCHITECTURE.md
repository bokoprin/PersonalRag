# Filename Engine Architecture — Production Hardening v2

## 1. Status

Route C の bake-off decision は維持する。
この文書は Route C の選抜をやり直すものではなく、Champion を長時間動く Windows 製品へ
載せる際の production invariants を固定する。

## 2. Layers

```text
Fixed local volumes
    |
    +-- FileSystemCatalog (one per volume)
    |      |
    |      +-- exact metadata + stable FileKey
    |      +-- bounded change feed
    |      +-- generation
    |      +-- GenerationStore
    |
    +-- MultiVolumeCatalog
            |
            +-- federated Filename / FullPath Search
            +-- typed change feed
            |
            +-- GUI
            +-- future Content Engine
```

Search engine:

```text
Immutable Route C base
        +
Indexed mutable delta
        +
Tombstones
        |
        +---- exact verification
        |
Background compaction
        |
New immutable Route C base
```

## 3. Exact identity vs search key

`FilenameRecord.Name` と `FilenameRecord.FullPath` は filesystem が返した文字列を保持する。

禁止:

```text
record.FullPath = record.FullPath.Normalize(...)
```

検索時だけ別の search key を作る。

Case insensitive:

```text
NFC
-> Unicode full case folding
-> NFC
```

`FilenameSemantics.NormalizerVersion` は persistence manifest に保存する。
normalizer version が異なるindexを再利用しない。

Wildcard は whole-string glob ではない。

```text
report_*.xlsx
```

は対象文字列内の部分patternとして判定する。

## 4. File identity

公開 identity:

```text
FileKey
- VolumeId
- NativeId
- IsNative
```

Windows では:

- `VolumeId`: volume GUID
- `NativeId`: `BY_HANDLE_FILE_INFORMATION` file reference

native identity を取得できない filesystem では path hash fallback を許すが `IsNative=false` とする。

`FileId` は Route C 内部ordinal互換のため残す。
将来 Content Engine の正本identityは `FileKey`。

hard link は同じ `FileKey` を複数pathが共有できるため、`FileKey` の一意性を要求しない。

## 5. Catalog generation and change feed

`FileSystemCatalog` はデータ変更batchごとに generation を単調増加させる。

```text
CatalogSnapshot
- Generation
- Records[]
- SourceGenerations[SourceId]

CatalogChangeBatch
- Generation
- SourceId
- SourceGeneration
- Changes[]
- Reconciled
```

change:

- Added
- Updated
- Removed
- Renamed
- Moved
- Reconciled

Future Content Engine は `GetSnapshot()` で初期状態を取得してからこのfeedを購読する。
MultiVolumeCatalogのglobal Generationはprocess-local観測順序であり、durable追跡は
`SourceId + SourceGeneration` を用いる。二重の filesystem watcher を作らない。

## 6. Route C delta invariant

旧 bake-off Route C の `Upsert()` は overlay 非空時にbase全scanへfallbackするため、
production path から呼ばない。

`src/FilenameSearch.RouteC/RouteCEngine` は **candidate generator** であり、単体の戻り値を
ユーザー向けexact resultとして扱わない。production correctness boundaryは
`PersonalRag.FilenameSearch.FilenameSearchEngine` / `IFilenameCatalog`。

production adapter は:

- immutable Route C base
- independent DeltaIndex
- tombstone

を使う。

通常queryは:

```text
base Route C candidates
+
delta candidates
-
overwritten/tombstoned base ids
-> exact verification
```

一件更新しただけで base 1M件scanへ退化してはならない。

Compaction threshold:

```text
distinct overlay >= max(4096 entries, base entries / 100)
OR
delta journal >= 4096 changes / 16 MiB
```

を初期値とする。後者は同一ファイルの反復更新でもdelta journalが無制限成長しないための条件。
formal resultで変更する場合は根拠をレポートする。

## 7. Persistence

Visible store path は manifest。
sidecar directory:

```text
<manifest>.data/
    writer.lock
    dirty.marker
    base-<generation>.routec
    base-<generation>.meta
    delta-<generation>.log
```

base files:

- immutable
- SHA-256 verified
- visible manifest自体も `manifest.sha256` で検証

delta:

- append-only
- recordごと SHA-256
- base commit後の changes を replay

normal update:

```text
append delta
```

であり、1変更で全baseを書き直してはいけない。

compaction:

1. current snapshot取得
2. off-live Route C build
3. tempへbase/meta/delta作成
4. durable flush
5. manifest atomic publish
6. reload verification
7. live engine swap
8. old generation cleanup

`writer.lock` は同一storeへの複数writerを拒否する。

`dirty.marker` が残っている場合、次回起動時は committed base + delta を読んだ後
filesystem reconcile を必ず実施する。

## 8. Filesystem layer

1 volume = 1 `FileSystemCatalog`。

`MultiVolumeCatalog` が固定ローカルdriveを自動検出して統合する。

Path containment は drive root を正しく扱う。

正:

```text
root = C:\
child = C:\Users\...
```

store directory が root 内にある場合、index自身を watcher/search 対象から除外する。
Multi-volume storeがC:上にある場合、C: catalogは自分のgeneration directoryだけでなく
**共有store root全体**を除外し、他volumeのindex書込みをC:のfilesystem eventとして再取込しない。

## 9. Watcher / overflow

active backend は bounded watcher fallback。

unbounded channel は使わない。

overflow / queue saturation:

```text
force reconcile
```

へcollapseする。

directory delete は、directory自身のdeleteをcatalogで認識した時点で同じworker turn内に
reconcileをscheduleし、子孫delete eventが偶然届くことへ依存しない。

reconcileは cancellation token を確認する。

## 10. GUI consistency

catalog data generationが変わったら、現在queryをdebounceして自動再検索する。

coreだけ更新され、画面が古いまま残る状態を許さない。

GUI query limit は first useful batch の定義と同じ100件。

## 11. Canonical project direction

production:

```text
src/FilenameSearch.Core
src/FilenameSearch.RouteC
src/FilenameSearch
src/FilenameSearch.Gui
```

benchはproduction srcへ依存する方向に寄せる。
productionが`bench/route_c`へ依存してはならない。

旧 `src/Astra.*` は内容検索研究referenceであり current filename production pathではない。

## 12. Formal gates before refreeze

Filename phase は次を再PASSするまで refreezeしない。

- exact Unicode path identity
- wildcard / full casefold oracle
- 1M core
- 1M GUI first batch
- actual GUI child process startup
- one-update post-update latency
- 10k / sustained churn後もindex pathを維持
- persistent write amplification
- stopped same-name modify metadata catch-up
- directory tree delete
- all fixed-volume federation
- writer lease
- base/delta corruption
- dirty shutdown
- idle CPU / I/O / RAM
