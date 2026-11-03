# Rust lost-owner mutation calibration

At `cc72c819`, temporarily omit the call to `keep_lost_owners_unknown` in
`src/references/rust.rs`. Both `lost_module_owner_witness` and the randomized
property fail. The mutation was reverted.

Randomized run: `PROPTEST_RNG_SEED=0000000000000000000000000000000000000000000000000000000000000001 TRELLIS_RUST_ABSENCE_CASES=30 cargo test --locked --test index proptest_rust_absence::rust_partial_sources_do_not_add_unchanged_sites -- --exact --nocapture`.
The target and temporary directories must be set as in the campaign report.

After 13 successes, proptest shrank the deliberate failure to:

```text
alias=false, nested=false, mode=0, filtered=false, owner=true,
custom_path=false, noise=0, reverse=false, history=[]
path=src/main.rs, partial reason=syntax_error
gained=(production, src/x/z.rs, src/y.rs, 11, 22, reference)
```

Healthy: `src/lib.rs` declares `x` and `y`; `src/main.rs` declares `x`
and an inline `y`; `src/x.rs` declares `z`; unchanged `src/x/z.rs` calls
`crate::y::f()`. Healthy ownership is ambiguous. Prefixing `src/main.rs`
with `(` hides the second `mod x` in an error. The mutation incorrectly
assigns the surviving call to `src/y.rs`. The corresponding proptest seed is
saved in `proptest_rust_absence.txt`; it is a calibration failure, not a
failure of unmodified production.
