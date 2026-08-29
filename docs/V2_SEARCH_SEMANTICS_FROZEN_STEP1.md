# PersonalRag V2 Search Semantics

Status: **FROZEN for V2 1.0 deterministic content search**  
Frozen: 2026-08-28  
Unicode data version: **15.1.0**  
Implementation package: `personalrag-v2` **0.3.0**

## 1. Scope

This document freezes the deterministic **content-search semantics** used by PersonalRag V2 before the production persistent index is designed.

The frozen content modes are:

1. literal substring,
2. wildcard,
3. regex.

Filename/path query syntax is intentionally not frozen here; it belongs to roadmap step 3 (filename/path index). PDF/Office extraction boundaries belong to roadmap step 5. Natural-language/semantic search is deferred until after deterministic filename/content search is complete.

## 2. Common text model

All supported extractors SHALL present content to the search engine as:

- Unicode text represented internally as UTF-8,
- a sequence of **logical text units**,
- hard boundaries between logical units.

No content pattern may match across a logical-unit boundary.

For plain text/source/log input, the current logical unit is one line. Future PDF/Office adapters SHALL define their logical units in accordance with `V2_SEARCH_ARCHITECTURE.md`.

## 3. Unicode normalization

PersonalRag V2 is pinned to **Unicode 15.1.0** for the V2 1.0 search semantics.

### 3.1 Case-sensitive mode

The comparison representation is:

```text
input -> NFC
```

Canonical equivalents therefore match. For example, precomposed `é` and `e + U+0301` are equivalent for search.

Case itself is preserved.

### 3.2 Case-insensitive mode

The comparison representation is:

```text
input -> NFC -> Unicode full default case fold -> NFC
```

Properties:

- locale-independent,
- no smart-case,
- no Turkic locale tailoring,
- Unicode full case-fold expansions are supported (for example `ß -> ss`),
- Greek final/non-final sigma follows Unicode full default case-fold behavior.

### 3.3 Compatibility normalization

V2 SHALL **not** apply NFKC/NFKD by default.

Therefore compatibility-equivalent characters such as full-width ASCII and ASCII are not silently collapsed.

### 3.4 Result locations

Normalization and case-fold expansion do not change the public result coordinate system. Search hits SHALL map back to the byte offset of the corresponding location in the original UTF-8 logical unit.

If multiple normalized positions originate from one source scalar (for example the two folded `s` scalars produced from `ß`), the same original location SHALL be emitted only once for an individual query match start.

## 4. Literal mode

Literal mode performs arbitrary substring matching inside one logical unit.

- default mode: case-insensitive,
- optional mode: case-sensitive,
- 1-character/byte and longer queries are supported,
- overlaps are allowed,
- candidate filtering may return false positives,
- final verification must be exact under the requested Unicode/case semantics,
- supported literal queries must have zero false negatives.

An empty literal query returns no content matches at the core API level. The future GUI may interpret an empty content field as “no content constraint.”

## 5. Wildcard mode

Wildcard mode is deliberately small and deterministic.

Supported syntax:

| Syntax | Meaning |
|---|---|
| `*` | zero or more normalized Unicode scalars within the same logical unit |
| `?` | exactly one normalized Unicode scalar |
| `\x` | treat `x` literally, including `*`, `?`, and `\` |

All other characters are literals. Regex metacharacters have no special meaning in wildcard mode.

Examples:

```text
Create*W
file?.txt
literal\*star
```

Wildcard uses the same NFC/case-fold pipeline as literal search. `*` and `?` operate on the normalized comparison representation, so a full case-fold expansion may contain more normalized scalars than the original source scalar.

The implementation compiles wildcard syntax to the frozen safe regex engine. A conservative mandatory literal is extracted where possible and used by the candidate index; exact NFA verification determines final results.

An empty wildcard pattern and a dangling escape are rejected.

## 6. Regex mode

Regex is a secondary deterministic content-search mode. The V2 engine uses a **Thompson-style NFA with backward dynamic programming**, not a backtracking engine. Matching complexity is bounded by text length times compiled NFA size rather than exponential backtracking paths.

### 6.1 Supported grammar

Supported constructs:

- literal Unicode scalars,
- `.`,
- `^` and `$` at logical-unit boundaries,
- capturing-syntax groups `(...)` for grouping only (captures are not returned),
- alternation `|`,
- character classes `[...]` and `[^...]`,
- scalar ranges such as `[A-Z]`,
- `*`, `+`, `?`,
- `{m}`, `{m,}`, `{m,n}`,
- escapes `\n`, `\r`, `\t`,
- classes `\d`, `\D`, `\w`, `\W`, `\s`, `\S`,
- Unicode scalar escape `\u{HEX}`,
- escaped punctuation/metacharacters.

The predefined class semantics are:

- `\d`: Rust 1.97.1 `char::is_numeric()`,
- `\w`: Unicode alphanumeric or `_`,
- `\s`: Rust 1.97.1 `char::is_whitespace()`,
- uppercase forms are complements.

### 6.2 Explicitly unsupported/rejected

V2 1.0 regex does not support:

- backreferences,
- lookahead/lookbehind,
- inline flags,
- non-capturing/special `(?...)` groups,
- named groups/captures,
- word-boundary assertions such as `\b`,
- Unicode property escapes such as `\p{...}`,
- lazy/possessive quantifiers,
- features that require catastrophic/backtracking semantics.

Unsupported syntax SHALL return an explicit pattern error rather than silently changing meaning.

Implementation safety guards also reject patterns exceeding the configured node/nesting/repetition/NFA-state limits.

### 6.3 Unicode and classes

Regex literals use the same NFC/case semantics as literal search, including canonical equivalence across adjacent literal atoms and `\u{...}` escapes.

In case-insensitive mode, a class member/range endpoint whose full case fold expands to multiple scalars is rejected. This avoids pretending that one class atom can consume a multi-scalar fold such as `ß -> ss`.

### 6.4 Mandatory-literal fast path

The parser conservatively derives literals that must occur in every successful match. The longest available mandatory literal is used as the candidate-index anchor.

Examples:

```regex
ERROR_[0-9]{4}          -> mandatory literal "ERROR_"
Create.*File            -> mandatory literal "Create" or "File"
Create(File|Directory)W -> mandatory literal "Create"
```

A query with a mandatory literal is **indexable** and may use unigram/bigram/global-trigram/rare-trigram candidate filtering before exact NFA verification.

A regex without a mandatory literal, for example `[A-Z]{8}`, remains correct but verifies all relevant blocks and is outside the guaranteed fast path.

Mandatory-literal extraction is intentionally conservative: failing to discover an anchor may reduce performance, but may never change correctness.

### 6.5 Match enumeration

Regex matching is substring-oriented unless anchored. Overlapping match starts are allowed. An empty regex string itself is rejected; a non-empty regex that can match an empty sequence may report starts according to its exact NFA semantics and is outside the preferred selective fast path unless it also has a useful mandatory literal.

## 7. Candidate index versus exact semantics

The candidate index is a performance mechanism only.

- candidate false positives are allowed,
- candidate false negatives are forbidden for supported indexed semantics,
- final literal verification is exact normalized substring verification,
- final wildcard/regex verification is the NFA verifier,
- case-sensitive queries may use the shared folded candidate index, but are verified against the NFC case-preserving representation.

## 8. Invalid UTF-8 boundary

The current canonical P0/P1 prototype constructs the full Unicode semantics for valid UTF-8 logical units. Byte-preserving literal fallback remains available internally for invalid UTF-8, while wildcard/regex skip invalid-UTF-8 units.

The production ingestion policy for legacy/non-UTF encodings will be specified when file-type ingestion is productized; it SHALL decode supported encodings to the frozen Unicode text model before indexing rather than weakening the semantics above.

## 9. Frozen versus deferred

Frozen by this document:

- Unicode 15.1.0,
- NFC (not NFKC),
- explicit case-sensitive / case-insensitive behavior,
- locale-independent full default case fold,
- original UTF-8 byte-offset mapping,
- literal semantics,
- wildcard dialect,
- regex dialect and non-backtracking verifier model,
- conservative mandatory-literal candidate path,
- hard logical-unit boundaries.

Deferred without reopening these semantics:

- production persistent file layout,
- filename/path index and its query syntax,
- Windows incremental indexing,
- PDF/Office extraction/storage,
- Everything-style GUI,
- final Windows E2E/performance/failure acceptance,
- semantic/LLM search.

Any future change to a frozen item requires an explicit format/semantic version decision and regression evidence; it must not be introduced as an incidental optimization.
