# NTFS USN / MFT Decision

## Decision

2026-09 hardeningでは filesystem/search API をvolume単位へ分離し、stable native `FileKey` を導入した。

active change backendは `WatcherVolumeChangeFeed` とする。

直接USN Journal/MFT readerは **このcommitではproduction defaultにしない**。

## Why

USN/MFTは全drive startup/restart catch-upを大幅に改善できる可能性が高い一方:

- volume handle access policy
- journal wrap / reset
- FRN -> current path reconstruction
- hard link
- mount point
- non-NTFS fallback
- permission差
- journal availability

を正しく扱う必要がある。

Filename Search coreの正確性をUSN実装の成熟度へ依存させるより、まずbackend boundaryを固定し、
official 1M / all-volume measurementで必要性を判断する。

## Activation rule

`docs/CODEX_FILENAME_FORMAL_REMEASURE.md` の正式測定で以下のどちらかを満たせない場合:

- existing-index actual GUI startup <= 2.0s
- restart catch-up <= 5.0s

Codexは `IVolumeChangeFeed` / volume discovery boundaryの後ろに NTFS USN backendを実装する。

このとき:

- SearchRequest
- FileKey
- CatalogChangeBatch
- FilenameSearchEngine
- GUI

の外部contractを変更しない。

## Fallback

USN unavailable / non-NTFS / access denied:

```text
recursive reconcile + bounded FileSystemWatcher
```

へ自動fallbackする。

fallbackでもcorrectness gateは緩和しない。
