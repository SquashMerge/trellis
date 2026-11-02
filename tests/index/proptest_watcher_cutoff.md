# Watcher cutoff discovery property

`proptest_watcher_cutoff.rs` drives a real in-process `WatcherWork` and isolated
store on a real temporary filesystem. Native OS delivery is disabled, and the
periodic inventory is deferred; all events use the watcher's bounded ingress.
Forced reconciliations and the admission/post-cutoff hooks order the critical
window without sleeps. No daemon or network listener is involved.

Each shrinkable schedule has one to three rounds, with one to three edits and
events before admission and another one to three in either cutoff window. Edits
advance among four source versions, rewrite the same bytes, or submit an event
without a write. The independent model tracks source, selected bytes and pin,
ingress and accounted generations, and pending intent. It explicitly includes
the additional full-wake generation after an invalidated capture or failed
acknowledgement. A pre-cutoff event must leave the prior pin selected; a
post-cutoff event permits the captured selection but stays unacknowledged. A
subsequent reconciliation must account for all intent and select the final
source. Each round compares the final retained native artifact, source map,
selected SQL tables, graph and classes to a fresh full index on the same root;
the original pinned artifact, SQL and source remain readable and unchanged.

The unchanged control submits hints without writing and checks that the pin
does not advance. A same-byte *rewrite* may change capture stats, so the model
does not assume it is an unchanged capture (Decision 0006). Deterministic
changed-byte witnesses cover both cutoff windows even if generated samples do
not. Proptest prints the schedule and persists shrinking failures; the discovery
run below uses a fixed seed.

## Results and calibration

With seed `612026`, a rerun with the configured case count explicitly reading
`PROPTEST_CASES` passed 120 generated schedules and all three deterministic
witnesses. The earlier configuration hard-coded four cases and did not prove
the reported 120-case count. No unmodified-product discrepancy was observed. An earlier
test iteration incorrectly equated same source bytes with an unchanged capture;
the deterministic no-op test exposed that model error, and the oracle was
corrected to account for capture stats.

Two temporary mutations were reverted:

* Removing the generation-change cutoff check caused the pre-cutoff witness to
  fail with `workspace inventory drift` before selecting stale bytes. This is an
  independent protection, not the requested stale-selection calibration.
* Replacing `acknowledge(&batch)` with an acknowledgement of a newly drained
  batch made the post-cutoff witness fail on actual acknowledged generation:
  trace `[Round { before: [Advance], window: AfterCutoff, during: [Advance] }]`,
  actual `3`, expected `1`. The selected source had already been checked as the
  middle capture. Thus the newer event was incorrectly acknowledged despite not
  being represented by that capture. This is deliberately injected, not a
  product finding. `git diff -- src` is empty after reverting both mutations.

The full-index oracle shares the production extractor, and these schedules use
one changing JS path, bounded short bursts, synchronous hook ordering, and
forced reconciliation. They do not cover native notification timing, overflow,
debounce/backoff, retirement, option changes, or concurrent processes. The
initial watcher generation is taken from the initialized watcher epoch; all
subsequent submitted and full-wake increments are independently modeled.

## Commands

```sh
export CARGO_TARGET_DIR="$HOME/projects/squashmerge/trellis-targets/proptest-watcher-cutoff"
PROPTEST_CASES=120 PROPTEST_RNG_SEED=612026 cargo test --locked --features test-support --test index index_atomic::proptest_native_incremental::watcher_cutoff -- --test-threads=1
# With each respective temporary mutation (expected failure):
cargo test --locked --features test-support --test index index_atomic::proptest_native_incremental::watcher_cutoff::pre_cutoff_changed_witness -- --nocapture
cargo test --locked --features test-support --test index index_atomic::proptest_native_incremental::watcher_cutoff::post_cutoff_changed_witness -- --nocapture
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --all-targets --features test-support -- -D warnings
git diff -- src
```

Full `./tools/verify` was deferred as requested pending #615.
