# Content Engine Boundary

Filename hardening後、Content Searchはこの境界の外側へ追加する。

## Identity

Content recordの正本key:

```text
FileKey
- VolumeId
- NativeId
```

`FileId` はFilename Route C内部ordinalとして扱い、Content indexの永続identityにしない。

## Initial snapshot + input feed

Content Engineは、起動時にまず `IFilenameCatalog.Changed` を購読してから:

```text
IFilenameCatalog.GetSnapshot()
    -> CatalogSnapshot
       - Generation
       - Records[]
       - SourceGenerations[SourceId] = SourceGeneration
```

を取得し、その後:

```text
IFilenameCatalog.Changed
    -> CatalogChangeBatch
       - Generation          // federation側の観測generation
       - SourceId            // volume/catalog identity
       - SourceGeneration    // source側のdurable generation
       - Changes[]
       - Reconciled
```

を使用する。Changedを先に購読することでsnapshot取得中の変更を取りこぼさない。
snapshotの `SourceGenerations` 以下のqueued eventは破棄し、gapを検出した場合は
`GetSnapshot()`を再取得して追いつく。

Added:
- content extract/index

Updated:
- size/mtime等に応じて再extract

Renamed/Moved:
- FileKeyが同じならcontent本体を再indexせずpath metadataだけ更新可能

Removed:
- FileKeyのcontent dataをtombstone/remove

Reconciled:
- `CatalogChangeBatch.Reconciled=true` をsource generation boundaryとして扱う
- data変更が0件でもreconcile完了通知を受け取れる

## Prohibited

Content Engineが独自に:

- FileSystemWatcher
- drive discovery
- rename heuristic
- root ownership
- FileId allocator

を実装しない。

filesystem ownershipはFilename catalog layerに一元化する。

## Snapshot consistency

Filename resultとContent resultをANDする場合、少なくともCatalog Generationを結果metadataへ持たせ、
古いcontent generationと新しいfilename generationを無条件に混ぜない。

最初のContent phaseでは:

```text
filenameFederatedGeneration
filenameSourceGenerations[SourceId]
contentIndexedSourceGenerations[SourceId]
```

を観測可能にする。multi-volumeではfederationのprocess-local generationだけをdurable checkpointにせず、
`SourceId + SourceGeneration` を正本にする。

## Exact path

Content extractorへ渡すpathは `FilenameRecord.FullPath` のexact filesystem spelling。
検索用NFC/casefold keyをI/O pathに使わない。

## Future multi-volume

Content storageは`FileKey.VolumeId`でpartition可能にする。
volume offline時もidentity collisionを起こさない。
