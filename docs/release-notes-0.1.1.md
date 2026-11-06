# Trellis 0.1.1 release notes

**Version:** `0.1.1`. **Release date:** on tag. These notes cover pull requests merged
after the [0.1.0 release](release-notes-0.1.md) (tag `v0.1.0`) through
[PR #743](https://github.com/SquashMerge/trellis/pull/743), plus test-only
[PR #744](https://github.com/SquashMerge/trellis/pull/744). This is a fix release.
Two MCP changes amend the response contract and raise `toolVersion` from 8 to 10;
see [Upgrade notes](#upgrade-notes).

## MCP answers

- **Incoming calls leave out callers bound elsewhere:** `trellis_incoming_calls`
  no longer lists a caller whose fresh, direct semantic binding targets a different
  function with the same name. This was seen with test helpers that share a name.
  It is an owner-ruled amendment to the incoming-calls contract.
  - A caller is left out only when its selected binding is fresh, direct
    (`boundaryReason:"none"`) and has a located `targetId` other than the requested
    symbol. Callers bound to the requested symbol, `dispatch` boundaries, stale or
    possibly stale bindings, bindings with no located target, and unbound callers
    are listed as before.
  - `data.boundElsewhereOmitted` counts the callers left out, and appears only when
    there is at least one. It is exact when the response is not truncated, and a
    lower bound otherwise.
  - Paging stays correct: left-out callers are skipped before `limit` is applied,
    so a page holds `limit` callers whenever that many remain, unless the existing
    65,536-byte response fit cuts it shorter. A left-out caller never makes a page
    look truncated. Ranking and `match` are unchanged.
  ([#730](https://github.com/SquashMerge/trellis/pull/730))
- **Call-graph edge text is capped:** `trellis_call_graph` clips each edge call's
  `spelling` to the advertised 1 KiB (`calleeTextBytes: 1024`) in full and
  `compact:true` form, and marks a clipped call with `spellingTruncated:true`. A
  spelling that fits is unchanged and has no flag. The traversal itself is
  unchanged. ([#730](https://github.com/SquashMerge/trellis/pull/730))
- **Smaller rule-check pages:** `trellis_rules_check` and `trellis_rules_explain`
  list each distinct semantic basis once per page in `data.semanticBases`, and
  evidence rows refer to it by `basisRef` instead of repeating it inline.
  - `trellis_rules_check` takes an optional `status` filter (one to three of
    `conforms`, `deviates` and `unknown`). It filters `instances` and `testOnly`;
    `counts` and `testCounts` still describe the whole evaluation, and `page.status`
    reports the filter applied.
  - For check, `limit` now counts rule instances per page (default 20), and each
    instance keeps one evidence entry. Use `trellis_rules_explain` to see more
    evidence for one instance; its `limit` still sets evidence entries per instance.
  - Measured on the Trellis repository with a rust-analyzer import, finding every
    deviation took 697 pages and 14.9 MB (at `limit:5`, with 4,751 inline bases)
    before the change. With `status:["deviates"], limit:50` it takes 18 pages and
    412 KB. The first 8 of those pages hold all 105 production deviations, plus 14
    test-only ones, in 189 KB of whole responses.
  - The CLI and the browser are unchanged.
  ([#737](https://github.com/SquashMerge/trellis/pull/737))
- **Symlinked workspace ancestors:** an explicit per-call MCP `workspace` path that
  passes through a symlinked directory, such as `/tmp` on macOS (a link to
  `/private/tmp`), now selects the canonical checkout. Before, such a call failed
  with `workspace_selection_failed` (reason `not_checkout`), so a client that sends
  `workspace` on every call, such as the release smoke runner, failed every tool.
  Calls that omit `workspace` already worked, as did CLI `--workspace`. A per-call
  path whose final component is a symlink is still refused.
  ([#734](https://github.com/SquashMerge/trellis/pull/734))

## Reliability fixes

- **`trellis daemon stop` on macOS:** on macOS, shutting a socket down from another
  thread can fail to wake a read that is just starting, so stop could wait forever
  while a `trellis serve` connection or an idle MCP client stayed connected. Stop no
  longer hangs because of this read race. The `serve` wait
  ([#740](https://github.com/SquashMerge/trellis/pull/740)), idle MCP session reads
  and partly received requests
  ([#743](https://github.com/SquashMerge/trellis/pull/743)) now end on the daemon's
  own shutdown signal.
- **`trellis producer status` during the first publication:** status no longer
  fails with `revision conflict: foreign or missing native pin` when the first index
  publication after a rebuild lands while it is reading. It now reads the control
  baseline (its native pin) and the native header in one snapshot.
  ([#733](https://github.com/SquashMerge/trellis/pull/733))

## Dependencies

Updates of the Rust crates `ignore`, `globset` and `rusqlite`
([#728](https://github.com/SquashMerge/trellis/pull/728)), of npm packages used by
repository tooling under `tools/`
([#726](https://github.com/SquashMerge/trellis/pull/726)), and of a CI action
([#727](https://github.com/SquashMerge/trellis/pull/727)).

## Development

Test-only: failing CLI children in tests report their status, stdout, stderr and
the test daemon's stderr tail, and test fixtures use synthetic watcher events unless
a test opts in to OS delivery
([#738](https://github.com/SquashMerge/trellis/pull/738),
[#744](https://github.com/SquashMerge/trellis/pull/744)).

## Upgrade notes

- **MCP `toolVersion` is 10** (0.1.0 had 8). It became 9 with the incoming-calls
  and call-graph changes ([#730](https://github.com/SquashMerge/trellis/pull/730)) and
  10 with the rule-check changes
  ([#737](https://github.com/SquashMerge/trellis/pull/737)). Trellis is pre-release
  and has no compatibility mode. A client that pins a schema or `toolVersion` must
  update for:
  - the new optional `boundElsewhereOmitted` in `trellis_incoming_calls`;
  - the new optional `spellingTruncated` on call-graph `Call` and `CompactCall`;
  - `semanticBases` and `basisRef` in place of inline `basis` in rule evidence, the
    new `status` input and required `page.status`, and the new meaning of check's
    `limit`.

  The amendments are recorded in the
  [MCP contract](mcp-readonly-pilot-contract.md#amendment-724-callers-bound-elsewhere-and-bounded-graph-spellings)
  and its
  [#725 amendment](mcp-readonly-pilot-contract.md#amendment-725-rule-evidence-bases-status-filter-and-instance-pages).
- Otherwise the usual [upgrade recovery](quickstart.md#upgrading-trellis) applies.

## Known limitations

See [known limitations](known-limitations.md) before relying on the results. Still
true from 0.1.0:

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
