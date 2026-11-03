# Native incremental discovery run

`proptest_native_incremental.rs` runs short, shrinkable histories over two JS
paths with stable Java and Rust gap fixtures. Each operation is published through
an in-process watcher and separately through a coordinator reporting its mode;
both are compared to a new cold store on the same physical root and options.
The test compares the entire retained native artifact and source map, the
selected SQL evidence (using the existing natural-key snapshot helper), graph,
class pages and source reads. The first retained pin remains readable and equal
after each later publication. Watcher hints use bounded notify ingress and are
checked for admission and acknowledgement. A separate test checks pre-cutoff
rejection and retry without sleeps.

## Observed discovery run

With seed `612026`, 120 generated histories completed 281 steps: 281
coordinator `mode:local`, zero `mode:full`; 194 steps changed JS bytes and 87
did not; the watcher selected a new revision on 276 steps. The operation counts
for edit, add, remove, rename, atomic replacement, restore, touch, multi-edit and
partial were respectively 62, 32, 27, 32, 25, 28, 26, 25 and 24. These are
attempt counts, not necessarily successful creates or renames. Four transitions
replaced partial JS with valid source. The fixture asserts nonempty declarations,
calls, references and Rust capability gaps. The deterministic partial/recovery
test additionally asserts the parser-error inventory at both steps.

No unmodified-product discrepancy was observed. The oracle is a fresh FULL
native measurement, not semantic FULL rejoin; it shares Trellis's full extractor
and therefore cannot detect defects common to both extraction paths. Generated
histories use quiescent captures, one to four operations and only two changing JS
paths. Native OS notification timing, larger projects and general scheduling are
outside this run. Watcher mode itself is not instrumented: the explicit LOCAL
counts are from the parallel coordinator history.

## Calibration

Temporarily changed the local-only `publish_local_native_recovery_with_semantic`
before-commit callback to update the selected changed `a.js` call spelling to
`calibration_corruption`. The retained-artifact assertion failed against the cold
store; the SQL-only assertion also independently showed
`native_version_calls` spelling `calibration_corruption` versus `helper`.
Proptest minimized the injected failure to `Edit(0, 1)` (replace `a.js`'s
`alpha0` with `beta0`). This is a deliberately injected fault, not a product
finding. The production mutation was reverted and `git diff -- src` was empty.

## Commands

All Cargo commands use the pinned toolchain and the dedicated target directory:

```sh
export CARGO_TARGET_DIR="$HOME/projects/squashmerge/trellis-targets/proptest-native-incremental"
PROPTEST_CASES=120 PROPTEST_RNG_SEED=612026 cargo test --locked --features test-support --test index index_atomic::proptest_native_incremental -- --test-threads=1
PROPTEST_CASES=120 PROPTEST_RNG_SEED=612026 cargo test --locked --features test-support --test index index_atomic::proptest_native_incremental::generated_native_histories_match_cold -- --nocapture
# With the temporary local-only mutation in place (expected failure):
PROPTEST_CASES=20 PROPTEST_RNG_SEED=612026 PROPTEST_MAX_SHRINK_ITERS=50 PROPTEST_DISABLE_FAILURE_PERSISTENCE=1 cargo test --locked --features test-support --test index index_atomic::proptest_native_incremental::generated_native_histories_match_cold -- --nocapture
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --all-targets --features test-support -- -D warnings
git diff -- src
```

Full `./tools/verify` was not run: its full test/browser lanes start daemon and
browser processes and manage subprocess lifecycles, outside this task's process
constraint. The generated run defaults to four cases; `PROPTEST_CASES` scales it.
