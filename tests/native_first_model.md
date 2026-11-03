# Native-first exploratory model (#620)

`native_first_model.rs` is a **pure reference-model and generator scaffold**.
It does not call Trellis's native-first implementation and its passing tests are
not product coverage. The current source predates the implementation PR and is
based on the design/addendum snapshot identified in the campaign findings log.
Reconcile its assumptions with the final committed owner-approved text first.

The generated model histories cover native and semantic pins, selection-scoped
older bases and pending evidence, recorded dependencies and labelled traversal
boundaries, synthetic staged accumulation, exact-target CAS and supersession.
The same scenario assertions also reject deliberately faulty model variants.
The synthetic FULL table is not a real independent producer rejoin.

## When the implementation PR appears

1. Check out the exact PR head in a separate campaign worktree and port this
   test-only scaffold. Preserve its property seed and captured failure histories.
2. Add an adapter to the real checkout worker, publisher and public readers.
   Use deterministic owner hooks/signals to observe native commit before the
   semantic stage, explicit pending/older-basis labels and each nested target,
   retained pins, semantic-only publication, and obsolete-stage disposal.
3. Compare completed output to an actual from-scratch FULL at the *same* native
   head, not the synthetic table. Separately verify FIFO/watcher ordering,
   cancellation/drop and stage-owner cleanup; the model does not implement them.
4. Do not convert the model's conservative cross-basis `UnresolvedDependency`
   and whole-fixture no-negative rule into product requirements without
   confirming the final contract. Scope membership, semantic-input identity and
   actual per-query coverage are currently abstractions. The broad generated
   history comparison skips some consumed-result graphs and the bound-graph
   check skips unresolved cases.

For the current model-only check, with a dedicated target and pinned toolchain:

```sh
/usr/bin/lockf -k /tmp/trellis-factory-verify.lock env \
  -u TRELLIS_SEARCH_MODEL -u TRELLIS_SEARCH_BACKEND -u TRELLIS_INDEX_DIAGNOSTICS \
  CARGO_TARGET_DIR="$HOME/projects/squashmerge/trellis-targets/proptest-620-native-first-model" \
  CARGO_INCREMENTAL=0 PROPTEST_CASES=128 PROPTEST_RNG_SEED=62020261110 \
  cargo test --locked --test native_first_model -- --nocapture --test-threads=1
```

The canonical investigation, model corrections, calibration evidence and
remaining limitations live in `trellis-evidence/proptest/FINDINGS.md` outside
the repository. Clean only the dedicated target after use.
