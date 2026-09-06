# Filename Hardening — Pre-commit Static Review

## Scope

`codex/filename-hardening` へ投入する production filename stack を対象に、Windows正式再測定前の静的確認を実施した。

対象:

- `src/FilenameSearch.Core`
- `src/FilenameSearch.RouteC`
- `src/FilenameSearch`
- `src/FilenameSearch.Gui` の変更ファイル
- `tests/FilenameSearch.Tests/Program.cs`
- hardening docs / actual GUI probe script

## Static checks completed

- C# delimiter / string / comment lexical balance: PASS
- csproj XML parse: PASS
- solution project path existence against base tree + hardening tree: PASS
- TODO/FIXME/NotImplementedException production stub scan: PASS
- Unicode full casefold generated table vs Python Unicode 15.1 `str.casefold()` exhaustive scalar comparison: PASS
  - explicit mappings: 1530
  - missing: 0
  - mismatch: 0
- wildcard semantics ownership: production exact verification is `FilenameSemantics.GlobSubstring`
- exact path identity: production `FilenameRecord.Name/FullPath` is not NFC-normalized before I/O/display
- Route C production dependency direction: production uses `src/FilenameSearch.RouteC`; old `bench/route_c` is not canonical
- persistence manifest/base/meta/delta validation path reviewed
- drive-root containment and shared multi-volume store exclusion reviewed
- recursive directory delete triggers same-turn reconcile path reviewed
- post-update base search uses immutable Route C candidates + independently indexed delta/tombstone

## Runtime status

This container does not have a working .NET SDK/runtime and the SDK binary download was unavailable, therefore **no build/test PASS is claimed here**.
WPF/Windows filesystem/NTFS/native identity/performance acceptance must be executed on the official Windows machine using:

`docs/CODEX_FILENAME_FORMAL_REMEASURE.md`

A compile/runtime failure found there is a hardening failure and Codex must fix it and rerun all affected gates before `PERSONALRAG_FILENAME_PHASE_COMPLETE` is restored.
