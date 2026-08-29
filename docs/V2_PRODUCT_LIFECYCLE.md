# PersonalRag V2 Product Index Lifecycle

Date: 2026-08-30  
Status: **Step 7 final stabilization interface — target-Windows final E2E pending**

## Purpose

Steps 1–6 exposed the deterministic engine and GUI, but Step 7 Windows verification found that a fresh user had no supported product command to create the initial bundle and no runnable product process wired the live USN Journal into bundle publication. This layer closes those product wiring gaps without changing frozen persistent format identities.

## Binary

```text
personalrag-v2-indexer
```

Commands:

```text
personalrag-v2-indexer init   --root <indexed-root> --store <index-store> [helper overrides]
personalrag-v2-indexer update --root <indexed-root> --store <index-store> [helper overrides]
personalrag-v2-indexer watch  --root <indexed-root> --store <index-store> [--interval-ms 250] [--once] [helper overrides]
personalrag-v2-indexer status --root <indexed-root> --store <index-store> [helper overrides]
personalrag-v2-indexer helpers
```

Helper overrides:

```text
--pdftotext <path> --unzip <path> --zstd <path>
```

Environment alternatives:

```text
PERSONALRAG_ROOT
PERSONALRAG_STORE
PERSONALRAG_PDFTOTEXT
PERSONALRAG_UNZIP
PERSONALRAG_ZSTD
```

## Initial indexing

`init`:

1. validates the indexed root and keeps the store outside it,
2. captures a Windows USN checkpoint before the initial scan when available,
3. scans deterministic metadata with stable platform file IDs,
4. builds/publishes Step 2 content plus Step 5 document verification,
5. writes Step 3 metadata, empty Step 4 delta, and incremental state,
6. publishes `PRV2BND1` last,
7. reloads the new bundle fail-closed before reporting success.

Capturing the journal checkpoint before the potentially long scan prevents a filesystem change racing with initial indexing from being silently skipped by the later live watcher.

## Explicit reconciliation

`update` performs a deterministic filesystem reconciliation against base+overlay state and publishes only when data or durable relevant state changes. It remains available independently of USN.

## Live Windows watcher

`watch` is native-Windows only and is designed to work under a normal non-elevated desktop token. It first attempts the drive-letter volume's NTFS USN Journal. If raw-volume access is unavailable (for example `ERROR_ACCESS_DENIED` under a normal token), it falls back to recursive Win32 directory-change notifications. `WATCH_READY` reports `mode=usn` or `mode=directory-notify` and includes the fallback reason when applicable.

In USN mode, the watcher resumes from the durable Step 4 checkpoint and uses relevant FRNs/parent FRNs as a trigger. In directory-notification mode, Win32 change notifications act only as the trigger. In both modes, deterministic reconciliation is the authoritative source of truth before bundle publication.

This final-stabilization implementation is intentionally correctness-first. A USN journal reset/gap reconciles rather than guessing, and the non-elevated fallback avoids making administrator elevation a normal product requirement.

## Helper discovery

`ExtractorConfig::discover()` honors explicit environment overrides first, then searches executable-local helpers and common Windows installation locations before falling back to command names. On Windows, OOXML ZIP access prefers the built-in native `tar.exe`; Git/MSYS `usr\bin\unzip.exe` is deliberately not auto-selected because it can mangle Win32 verbatim paths.

`tools/setup_windows_helpers.ps1` reports discovery state. With explicit `-Install`, it uses WinGet only for Poppler (`pdftotext`) and Zstandard (`zstd`); the preferred ZIP reader is Windows' built-in `tar.exe`. Third-party binaries are not committed into the PersonalRag source repository.

## Frozen identities

No Step 1–5 persistent identity changes in this layer:

- `PRV2IDX1` v2
- `PRV2MET1` v1
- `PRV2DEL1` v1
- `PRV2INC1` v1
- `PRV2BND1` v1
- `PRV2VER1` v1
- semantic ID `0x0003_0001`

The Windows content-to-metadata mapping fix canonicalizes only slash direction for the internal cross-index lookup. It does not redefine stored path identity or frozen query semantics.

## Product capacity acceptance

Whole-store capacity is measured with `tools/measure_product_capacity.ps1`. The normative percentage hard gate is evaluated at selected-source sizes of 4 MiB or larger; smaller roots are reported diagnostically because fixed two-bundle rollback/header overhead dominates the denominator. The final Windows retest must record 4/96/256 MiB complete-store ratios.
