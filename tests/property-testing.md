# Curated property tests

The scheduled `Nightly property tests` workflow runs these properties at 256
cases each. Normal runs use per-suite defaults (4–40); set
`TRELLIS_PROPTEST_CASES` to override all of them. `PROPTEST_RNG_SEED` can make
generated runs reproducible. The nightly sets it to the GitHub run ID, so an
attempt can be replayed without fixing the same seed across different runs.
Proptest also replays checked-in regression seeds.

For a local deep run, use the selection in
`.github/workflows/proptest-nightly.yml`. The witnessed-rejoin tests run their
process-wide test-oracle settings in a dedicated child process, so they also
work with ordinary concurrent `cargo test` runs.
The scheduled workflow reports a failed run in an issue, linking the logs; an
existing open issue of the same title receives a comment instead of a duplicate.
The nightly nextest profile allows the stateful retention property 15 minutes
while preserving the normal five-minute timeout for every other test. It sets
`TRELLIS_RETENTION_TRACE` so a failed or timed-out run reports each started
history and its completed duration; normal test runs do not emit this trace.

The suite compares individual unchanged-source reference sites after degradation
in JavaScript, Python, Rust and Java; compares witnessed LOCAL rejoin against
FULL; checks the Decision 0006 head-rooted retention model with two producer
slots; compares Architecture/metrics cached results and synchronized races with
fresh computation; and exercises SCIP ranges, encodings, enums and bounds.
The reference projections are structural rather than semantic bindings, and the
cache and FULL differentials share some production components. These bounded
fixtures do not prove correctness for all inputs or schedules. The Python
property covers colliding star exports and three degradation modes.
