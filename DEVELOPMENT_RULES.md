# PersonalRag V2 Development Rules

## Source of truth

- This V2 repository is the only canonical PersonalRag implementation.
- Do not restore, copy, wrap, or depend on the removed legacy implementation.
- Actual source, normative specifications, and actual test results override historical reports or assumptions.
- Historical evidence/reports may document superseded designs; they are not normative unless HANDOFF explicitly says otherwise.

## Required implementation workflow

For every implementation/change:

1. Run the existing regression tests before modifying source.
2. Record the current passing/failing baseline.
3. Design the change and its acceptance criteria.
4. Implement the smallest coherent change.
5. Add or update focused tests for the changed behavior.
6. Run focused tests.
7. Run the full V2 regression gate again.
8. Review design, implementation, tests, correctness, failure behavior, and SLO impact.
9. If review finds a problem, repeat design -> implementation -> focused tests -> full regression -> review, at most 3 repair loops.

If a required command cannot run, do not count it as PASS. Record why it cannot run, the command that should be run, the expected result, and the remaining risk.

## Frozen Step 1 / 2 / 3 / 4 / 5 rules

Steps 1, 2, 3, 4, and 5 are complete and frozen.

Before changing them, read:

- `docs/V2_SEARCH_SEMANTICS.md`
- `docs/V2_PERSISTENT_FORMAT.md`
- `docs/V2_METADATA_INDEX.md`
- `docs/V2_INCREMENTAL_INDEX.md`
- `docs/V2_DOCUMENT_EXTRACTION.md`

A semantic change that alters comparison/index bytes MUST NOT silently reuse an existing persistent identity. It requires an explicit format/semantic-id decision plus migration-or-rejection tests.

Current content identity:

- `PRV2IDX1`
- format version `2`
- semantic id `0x0003_0001`

Current metadata identity:

- `PRV2MET1`
- format version `1`
- semantic id `0x0003_0001`

Unicode version: **15.1.0**.

## Correctness rules

- Supported deterministic searches must have zero false negatives.
- Candidate-stage false positives are allowed; final results require exact verification.
- Logical content-unit boundaries are hard boundaries.
- Filename/path and content case behavior must follow the frozen Unicode semantics.
- Regex/wildcard candidate filtering must never change exact final semantics.
- Filename/path-only search must not require the content index.
- Stable FileID and exact path identity must survive persistence and Step 4 incremental updates.
- New search/index paths should be tested against an independent/simple oracle where practical.

## Product SLO rules

Content search:

- first useful batch hard limit: <=300 ms
- preferred first useful batch: <=100 ms
- persistent content index hard limit: <=10% selected source bytes
- preferred persistent content index: <=5%
- combined sparse content anchors: <=1.5% selected source bytes

Metadata search:

- first useful batch hard limit: <=300 ms
- preferred first useful batch: <=100 ms
- short-query zero-hit full scans must be measured explicitly because no q3 anchor exists
- million-file-scale memory, load time, and bytes/file must be tracked on every structural metadata change

A local micro-optimization is not accepted if the complete design violates correctness or product SLOs.

## Roadmap / scope discipline

Completed:

1. Unicode / regex / wildcard search semantics freeze
2. production persistent content index
3. filename/path metadata index
4. Windows incremental indexing
5. PDF / Office extraction and `PRV2VER1` verification store

Next:

6. Everything-style GUI with a second content-search field
7. E2E / performance / failure acceptance
8. V2 1.0

Semantic/LLM search is deferred until deterministic filename/path and content search are complete.

Step 5 is frozen. Document extraction-aware changes must preserve `PRV2VER1` v1 binding/recovery semantics and the frozen Step 1/2/3/4 identities. Step 6 must not redesign those backend contracts merely for GUI convenience or add semantic search.

## Packaging

- Generated build outputs such as `target/` must not be included in source handoff packages.
- `SOURCE_MANIFEST.sha256` must describe the sealed source package excluding itself.
