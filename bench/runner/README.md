# Filename bake-off runner

The runner will verify `EXPERIMENT_LOCK.json`, run each frozen route against
the same corpus and query set, and emit the per-route and combined bake-off
reports. It records build/load/search/update timings, correctness, persistent
bytes, and private memory. Official benchmark results are invalid whenever a
locked input hash changes.
