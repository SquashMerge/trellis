# Trellis 0.1.2 release notes

**Version:** `0.1.2`. **Release date:** on tag. These notes cover pull requests merged
after the [0.1.1 release](release-notes-0.1.1.md) (tag `v0.1.1`) through
[PR #760](https://github.com/SquashMerge/trellis/pull/760). This is a small release.
Most of it is test-reliability work; one change affects what reads report while the
daemon is catching up. There are no MCP contract changes, and `toolVersion` stays 10.

## Catching-up reporting

- **`catchingUp` covers every watcher reconciliation that can publish:** the advisory
  `catchingUp` flag, sampled at read admission and returned with MCP responses and
  browser reads, now stays `true` for the whole of any watcher reconciliation that
  captures, from the moment it decides to capture until it has acknowledged its work.
  Before, two cases could report `false` while new work was about to publish:
  - a periodic inventory with no accepted hint, which can find an edit and publish a
    new head while the published watch status still showed the last idle inventory;
  - a watcher replaced by an options change, whose public status still named the old
    watcher until the worker republished it. It now stays pending until the
    replacement's status is published. A worker step that replaced the watcher
    without publishing its status republishes it at the end of that step.

  `catchingUp` is still advisory: it neither invalidates a validated prior head nor
  authorizes an unvalidated one.
  ([#752](https://github.com/SquashMerge/trellis/pull/752))
- **State waiters wake after each worker iteration:** the daemon's checkout worker
  now publishes a state change at the end of every loop iteration. Before, a drained
  request queue, an acknowledged watcher hint or a cleared semantic catch-up target
  woke nothing that was waiting on the checkout's state.
  ([#752](https://github.com/SquashMerge/trellis/pull/752))

## Development / test reliability

This completes the flaky-test work in
[#720](https://github.com/SquashMerge/trellis/issues/720) that 0.1.1 began with
standard child-failure messages
([#738](https://github.com/SquashMerge/trellis/pull/738)) and synthetic fixture
watcher events ([#744](https://github.com/SquashMerge/trellis/pull/744)).

- **One shared settled wait:** tests wait until startup reconciliation has committed
  and the watcher, request queue and semantic catch-up are idle at one published head,
  through `await_settled_for_tests` in process or the hidden test-support command
  `trellis test-settled`. It waits on owner-published state, is bounded only by the
  hang backstop, and names the busy component when it times out. The ad hoc readiness
  loops in the daemon, lifecycle and browser tests now use it, and the producer
  restart test that could read before startup reconciliation published
  ([#736](https://github.com/SquashMerge/trellis/issues/736)) reads only once settled.
  ([#752](https://github.com/SquashMerge/trellis/pull/752))
- **Flaky-pattern verify check:** `tools/flaky-patterns.py` runs in the verify
  `checks` lane and flags unretried store opens, global counters in test code, waits
  shorter than the hang backstop and unbounded socket accepts, with an allowlist that
  needs a reason per entry. An audit fixed or allowlisted every existing site
  ([#754](https://github.com/SquashMerge/trellis/pull/754)). Follow-ups reviewed the
  remaining store opens one by one, replaced a 180 ms timing window with a hook,
  repaired an assertion that could never fail, and added checks for speed assertions
  and short read timeouts ([#756](https://github.com/SquashMerge/trellis/pull/756)).
- **Weekly stress lane:** `stress.yml` and `tools/stress` run the Rust tests several
  times under CPU load on Linux and macOS, weekly and on manual dispatch. The lane
  never gates a PR ([#753](https://github.com/SquashMerge/trellis/pull/753)). After
  the repetitions it runs a deep property pass, rerunning the curated property tests
  at 1,024 cases each by default
  ([#758](https://github.com/SquashMerge/trellis/pull/758)). See
  [Stress lane](releases.md#stress-lane).
- **Native-first property suite on main:** `proptest_native_first` now runs in every
  PR's tests lane and in the nightly property run
  ([#758](https://github.com/SquashMerge/trellis/pull/758)). Its document reads
  compare every semantic row kind (producers, coverage, provenance, symbols,
  declaration bindings, references, call bindings and type relationships), not only
  provenance, and a mutation control proves the comparison catches dropped,
  duplicated or changed rows ([#760](https://github.com/SquashMerge/trellis/pull/760)).
- **Individual test fixes:** an MCP cancellation test no longer deadlocks against
  batched pause points ([#748](https://github.com/SquashMerge/trellis/pull/748)); the
  synthetic-cohort composition test keeps its Cargo target out of `$TMPDIR` and
  rebuilds it if temp cleanup removed a generated file
  ([#749](https://github.com/SquashMerge/trellis/pull/749)); and the daemon-crash
  recovery tests wait only on the hang backstop
  ([#750](https://github.com/SquashMerge/trellis/pull/750)).

## Upgrade notes

The usual [upgrade recovery](quickstart.md#upgrading-trellis) applies.

## Known limitations

See [known limitations](known-limitations.md) before relying on the results. Still
true from 0.1.1:

- **Performance gate:** the combined native-index and semantic-import
  [performance gate (#61)](https://github.com/SquashMerge/trellis/issues/61) remains
  open. Its historical measurements missed their time targets, and large repositories
  can take minutes to index and import. Semantic catch-up after an edit is measured
  in the [0.1.0 notes](release-notes-0.1.md#catch-up-performance), not bounded.
- **Default Cargo features:** `trellis producer run` analyzes Rust with default
  Cargo features only, so feature-gated code gets no semantic evidence and shows as
  `failed`. The [quickstart](quickstart.md#feature-gated-rust-code) shows the
  workaround: run `rust-analyzer` with the features you need and use
  `trellis producer pickup`. Reading producer settings from project files is planned
  in [#717](https://github.com/SquashMerge/trellis/issues/717).
- **Re-import after upgrades:** an upgrade that changes the index schema rebuilds
  the index. Trellis retries the last successful `producer run` batch, but missing or
  mismatched inputs leave **needs re-import**, and Trellis never runs a producer by
  itself. There is no old-index migration. See
  [upgrade recovery](quickstart.md#upgrading-trellis).
- **Incoming callers by name:** an unbound caller, or one with a stale or `dispatch`
  binding, is still listed by name match and can call another same-name function.
  Only a fresh direct binding elsewhere removes it.
  ([#730](https://github.com/SquashMerge/trellis/pull/730))
- Classes draws no proven relationship edges. TypeScript/TSX/JSX parsing, remote
  SSH, Herdr integration and design handoff are not included. Producer imports do not
  imply complete coverage or runtime dispatch resolution.
