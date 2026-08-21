# Validation — Performance Pass 1 / 1M-ready

2026-08-15

## Pre-change regression

Portable Search Core:
- rustfmt: PASS
- clippy `-D warnings`: PASS
- unit: 3/3 PASS
- production: 28/28 PASS
- release build: PASS
- self-test: `SELF_TEST_PASS`

Bridgeのoffline validation環境を作る過程で、FullConnected版に残っていたRust 1.97 clippy debtとRegex test expectationの誤りを検出しました。これらは本高速化と併せて修正し、現在はbridgeもwarning-freeです。

## Post-change regression

Portable Search Core:
- rustfmt: PASS
- clippy `-D warnings`: PASS
- unit: 3/3 PASS
- production: 28/28 PASS
- release build: PASS
- self-test: `SELF_TEST_PASS`

GUI bridge:
- rustfmt: PASS
- clippy `-D warnings`: PASS
- tests: 4 PASS / 0 FAIL / 1 large stress ignored by normal gate
- 50k-file large scanner stress (`--release --ignored`): PASS

Tauri source:
- rustfmt: PASS

## Windows cross-target gate

Target: `x86_64-pc-windows-gnu`, Rust 1.97.1

- search-core check all targets: PASS
- search-core clippy all targets `-D warnings`: PASS
- search-core Windows test link: PASS
- bridge-core check all targets: PASS
- bridge-core clippy all targets `-D warnings`: PASS
- bridge-core Windows test link: PASS

Generated tests are valid PE32+ x86-64 Windows executables.

## Compatibility

New scan-metadata build API and old path API were run against the same 20k corpus. Every generated index file compared byte-for-byte identical across paired A/B runs.

The existing path API remains available, so non-GUI users retain previous behavior.

## A/B benchmark summary

Synthetic filesystem corpus:
- 100,000 files total
- 80,000 selected
- 20,000 under excluded directories
- parallel scanner mode

Median results:

| workload | before | after | improvement |
|---|---:|---:|---:|
| parallel scan | 50.250 ms | 46.615 ms | 1.078x / 7.2% |
| 20k base build | 58.554 ms | 41.965 ms | 1.395x / 28.3% |
| 20k-hit size sort | 25.446 ms | 7.177 ms | 3.545x / 71.8% |

The benchmark is intentionally bridge-focused; the already-optimized Portable query/index algorithms were not changed.

## Remaining real-machine gate

The final package is cross-compiled and cross-linked for Windows core/bridge. The final optimized Tauri executable itself should still be built once by `Build-And-Run.cmd` on Windows because this Linux environment does not contain the Tauri/Node dependency cache. The immediately preceding FullConnected Tauri baseline already ran successfully on the user's Windows machine.
