# PersonalRag fastest validated variant — 2026-08-21

This package is reconstructed from the current development tree to the fastest validated configuration discussed in the active development session.

Enabled performance waves:
- Q2 active-list fast path baseline
- q3 Periodic-first / Deferred Local Dedup
- Unix CONTENT_BLOB writev(64) direct gather write
- post-q3-dedup q1/q2 projection (kept out of the q3 emit hot loop)

Explicitly excluded as performance regressions:
- q1/q2 collection in the q3 emit hot loop
- periodic-prefix q1/q2 collection inside the q3 emitter
- q3 owner-tag recent-cache experiment

Known measured result for the post-q3-dedup projection variant in the development session:
- q1/q2 phase: about 31.8% faster
- q3 phase: about 13.8% slower from projection work
- segment total: about 1.0% faster
- E2E: about 2.43% faster, 14/21 pair wins

PRSEG2A6/v6 serialized semantics are intended to remain unchanged.

Validation performed before packaging:
- Rust 1.97.1
- cargo fmt -- --check: PASS
- cargo clippy --offline --all-targets -- -D warnings: PASS
- cargo test --offline: PASS (all suites)
- cargo build --offline --release: PASS
- pr_portable self-test: SELF_TEST_PASS
