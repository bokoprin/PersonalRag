# PersonalRag V2 Product Index Lifecycle

Date: 2026-08-29  
Status: **Step 7 stabilization interface — target-Windows E2E pending**

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

## Live Windows USN producer

`watch` is native-Windows only. It opens the drive-letter volume's NTFS USN Journal, resumes from the durable Step 4 checkpoint, detects records involving known indexed FRNs or indexed parent FRNs, and then runs deterministic reconciliation/publish.

This stabilization implementation is intentionally correctness-first: USN is the change trigger, while reconciliation determines the authoritative new product state. It does not claim the final performance of a future direct-FRN mutation pipeline.

A journal reset/gap forces reconciliation rather than guessing. A relevant journal advance with no file-state delta can publish a state-only bundle so the durable checkpoint is not lost.

## Helper discovery

`ExtractorConfig::discover()` honors explicit environment overrides first, then searches executable-local helpers and common Windows installation locations before falling back to command names.

`tools/setup_windows_helpers.ps1` reports discovery state. With explicit `-Install`, it can use WinGet to provision Poppler (`pdftotext`), Zstandard (`zstd`), and Git for Windows (`unzip`). Third-party binaries are not committed into the PersonalRag source repository.

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
