# Manual metrics oracle regeneration

This directory is an independent Cargo workspace; it is not in Trellis dependencies. CI **never** runs regeneration. Commit both lockfiles and reviewed goldens. `regenerate.py` verifies SHA-256 of both committed locks and the adapter git revision before doing any work. `python3 tools/metrics-oracle/regenerate.py` verifies pins without network. `python3 tools/metrics-oracle/regenerate.py --regenerate` builds a fresh external RCA checkout and regenerates raw fixture outputs; review all changed JSON and reconcile `comparisons.json` explicitly. The external checkout is temporary, never a modified Trellis source checkout.

Adapter pin: `https://github.com/StrangeDaysTech/arborist-metrics.git` at `d246d2e465346cbc2a7cc468d34ddbb7d77d5a0a`, with `default-features = false` and `java,python,javascript,rust`. Actual API is `analyze_source` and `FunctionMetrics { name, start_line, end_line, cognitive, cyclomatic, sloc, ... }`; there are no byte spans or lambda rows. Manual invocation from repo root:

```sh
cargo run --locked --manifest-path tools/metrics-oracle/Cargo.toml --release -- rust tests/fixtures/metrics/rust/controls.rs
```

RCA v0.0.25 is exactly `57ab3973b53e7bf392d925cd5e6c6e889b8c22c0`. Make a fresh checkout **outside** this repository. The committed `rust-code-analysis-v0.0.25.Cargo.lock` is the upstream workspace's lock; generating a fresh lock during regeneration is forbidden. The first `cargo generate-lockfile --manifest-path <RCA_CHECKOUT>/Cargo.toml` selected an incompatible second tree-sitter version through RCA's unconstrained `tree-sitter-rust` dependency. During initial lock creation only, `cargo update --manifest-path <RCA_CHECKOUT>/Cargo.toml -p tree-sitter@0.27.1 --precise 0.20.9` removed it; this exact resulting lock built RCA. No upstream files were changed in the repository.

```sh
git clone https://github.com/mozilla/rust-code-analysis.git <RCA_CHECKOUT>
git -C <RCA_CHECKOUT> checkout --detach 57ab3973b53e7bf392d925cd5e6c6e889b8c22c0
git -C <RCA_CHECKOUT> rev-parse HEAD
cp tools/metrics-oracle/rust-code-analysis-v0.0.25.Cargo.lock <RCA_CHECKOUT>/Cargo.lock
cargo build --locked --manifest-path <RCA_CHECKOUT>/Cargo.toml -p rust-code-analysis-cli --release
<RCA_CHECKOUT>/target/release/rust-code-analysis-cli --metrics --output-format json --paths tests/fixtures/metrics/rust/controls.rs
```

The checkout HEAD must match the printed revision before copying/building. Run both oracle commands for all reviewed fixtures. RCA produces a file-level unit record and nested `spaces`: compare only metrics and granularity actually emitted, and require exact name and inclusive line boundaries to correlate function spaces. RCA MI variants and file-wide Halstead cannot stand in for per-declaration metrics. `comparisons.json` records actual native-derived scores, stable fixture-only native syntax IDs, every emitted comparable per-function field and explained differences or concrete unavailability. The offline integration test re-extracts all 18 sources and checks every emitted oracle row and every registered numeric cell without running these oracle binaries. Do not regenerate this register by copying oracle values into native columns; first capture actual native facts and compute actual metrics, then correlate by exact name and 1-based inclusive physical lines.
