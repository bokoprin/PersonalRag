# PersonalRag Portable Search Engine — Rust production core

This crate is the Rust production port of the validated C++20 portable search prototype.
The C++ implementation remains in this package as an oracle/reference implementation; new production integration should use the Rust crate.

## Frozen production defaults

- persistent immutable segment: `PRSEG005`
- q3 builder: pair-radix
- content q1: 256-bit byte-presence mask per ContentUnit
- content q2: absent
- content q3: cost/density-adaptive `inline_u32` / `delta_varint` / `block256_bitmap` / `dense_bitset`
- q3 directory: prefix10
- exact verifier: raw mmap store
- query planner: candidate-driven exhaustive search + order-driven First-N
- adaptive builder: exact-content Dedup when sampled duplicate ratio is >= 20%, otherwise Direct

The writer intentionally emits the same segment bytes and manifest text as the frozen C++ format for the same corpus/options.

## Modules

- `builder`: deterministic source-adapter helper, segment builder, q1/q3 indexes, durable manifest publish
- `format`: frozen v5 physical format, endian/checksum helpers, format errors
- `index`: checksum-verified mmap reader, lazy interactive reader, q3 decoders, exact verifier, locality-preserving query APIs
- `generation`: immutable base/delta generation merge, logical-map/tombstone sidecars, CURRENT publish, compaction
- `integration`: stable LogicalDocId/generation incremental planning boundary
- `mapped_file`: Unix `mmap`, Windows `CreateFileMappingW`/`MapViewOfFile`, fallback reader
- `types`: portable `DocumentInput` correctness boundary

No third-party Rust crates are required; builds and tests are fully offline.

## Build and hard gate

```bash
cargo fmt -- --check
cargo clippy --offline --all-targets -- -D warnings
cargo test --offline
cargo build --offline --release
cargo run --offline --bin pr_portable -- self-test
```

## CLI

```bash
# Build a deterministic filesystem corpus
./target/release/pr_portable build-disk ROOT adaptive INDEX_DIR 5000 2 0 8388608

# Full segment checksum + manifest verification
./target/release/pr_portable verify INDEX_DIR

# Exhaustive exact search
./target/release/pr_portable query INDEX_DIR content timeout
./target/release/pr_portable query INDEX_DIR name module

# Order-driven First-N
./target/release/pr_portable query INDEX_DIR content timeout 100
```

Normal `query` opens the manifest only and lazily maps segments on first use. `verify` and verified generation open still perform full checksum verification.

`DocumentInput` is the production correctness boundary. Windows USN/MFT/native enumeration belongs in a source adapter and must not change search semantics.

## Compatibility acceptance criteria

The port is accepted only when:

1. Rust reads/checksums/queries a C++-generated index.
2. C++ reads/checksums/queries a Rust-generated index.
3. identical input/options yield byte-identical `.prseg` files and `manifest.txt`.
4. Naive exact evaluation matches 1-byte, 2-byte, 3-byte, long, Unicode/Japanese, case-folded, duplicate-content queries.
5. all four q3 encodings are exercised.
6. bit-flip/truncation are rejected.
7. fresh-process open/query passes.

The current Linux hard gate satisfies these criteria. The Windows mmap branch is implemented but cannot be cross-compiled in this sandbox because only the Linux Rust standard library target is installed; it must be compiled/run on Windows before application integration is declared complete.

## R5-R11 production query/generation additions

- `ContentQueryPlan`: rare/low-density First-N candidate-driven, common First-N order-driven, exhaustive candidate-driven, adaptive 1/2/4 worker selection.
- `PRQ2C001`: optional exact two-byte sidecar for synchronous exact-count/export workloads. It does not change `PRSEG005`; missing sidecars fall back to q1-mask + exact verification.
- `SearchSession`: long-lived single-generation query session with persistent workers.
- `MergedSearchSession`: long-lived immutable base+delta session using one global `(source, segment)` task queue and one total worker budget.
- `MergedIndex::auto_compaction_decision`: count/bytes/tombstone based compaction recommendation.
- `PRMAP001` is mmap-backed; logical-document strings stay in the mapped sidecar until a caller actually needs them.

Additional CLI:

```bash
# Build optional exact q2 accelerators next to immutable PRSEG005 files
./target/release/pr_portable build-q2-sidecars INDEX_DIR 1

# Inspect the current generation's compaction recommendation
./target/release/pr_portable compaction-status GENERATION_STORE

# Inspect RAM/CPU-based builder tuning
./target/release/pr_portable tune-build
```

`PRSEG005` remains the frozen segment format after R5-R11. A format-v6 change was explicitly reviewed and rejected for now because no remaining measured format-layout bottleneck outweighed the compatibility cost.


## R12 PRPOS001 selective positional accelerator

`PRPOS001` is an optional, rebuildable per-segment accelerator for dense content trigrams. It does not change `PRSEG005`. The production codec is Elias-Fano and the speed-first dense threshold is 500,000 ppm (50%).

For exhaustive literals of at least four bytes, the planner may choose `PositionalDriven` when the estimated candidate density is at least 50%, at least two workers are justified, and production positional sidecars are complete. Covering trigram positions are intersected at exact query offsets, so fully covered literals do not need raw-text verification. First-N and q1/q2/q3 short paths remain unchanged.

```bash
# Production PRPOS sidecars (Elias-Fano, 50%, durable publish)
./target/release/pr_portable build-pos-sidecars INDEX_DIR ef 500000 1

# Verify all production positional sidecars
./target/release/pr_portable verify-pos-sidecars INDEX_DIR ef

# Research codec A/B
./target/release/pr_portable build-pos-sidecars INDEX_DIR delta 500000 0
./target/release/pr_portable build-pos-sidecars INDEX_DIR svb 500000 0
./target/release/pr_portable build-pos-sidecars INDEX_DIR block256 500000 0
./target/release/pr_portable profile-pos INDEX_DIR ef 10 4
```

The decoded posting cache is bounded to 16 postings per segment. Missing sidecars fall back to the existing query engine; stale/corrupt sidecars are rejected when loaded/verified. See `../REPORT-R12-PRPOS.md`.


## R15-R19 PRPOS003 long-only adaptive dense grams

`PRPOS003` is an optional, checksum-bound exact Unit accelerator for **dense 9..16-byte literals**. It does not change `PRSEG005`. The builder uses dense q3 starts and a single-scan shrinking frontier through q4..q16, but q4..q8 are not persisted because `PRPOS002` already owns that range. Only q9..q16 records are written.

Production build:

```bash
./target/release/pr_portable build-pos3-sidecars INDEX_DIR 500000 500000 16 adaptive 1
./target/release/pr_portable verify-pos3-sidecars INDEX_DIR
```

The exhaustive planner selects `AdaptiveGramDriven` only when query length is 9..16, estimated density is >=50%, at least two workers are justified, every PRPOS003 sidecar exists, and all sampled first-two segments have the exact record. Otherwise it falls through to PRPOS002/PRPOS001/PRSEG. First-N deliberately remains on the R14 Candidate/Order paths because cold TTFR A/B rejected PRPOS003 First-N.

Final release uses `codegen-units = 1`. See `../REPORT-R15-R19-PRPOS003.md`.
