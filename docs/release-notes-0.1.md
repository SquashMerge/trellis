# Trellis 0.1.0 release notes

**Version:** `0.1.0`. **Release date:** on tag. These notes cover merged pull requests
through [PR #721](https://github.com/SquashMerge/trellis/pull/721). This is the first
release, so they summarize features rather than compare with a prior release.

## A local code browser for people and agents

Trellis indexes **JavaScript, Rust, Java and Python** on your machine. Browse a
checkout in the authenticated, loopback-only web app, or give a coding agent the
same cached evidence through the read-only stdio MCP server. Native indexing and
file watching do not run repository code. Optional semantic producers have separate,
explicit trust settings.

- **Browser:** Architecture with reviewed layer rules, a complexity Treemap, Files
  with source and a type/member outline, Classes, and source-linked Sequence diagrams.
  Hidden tooling layers are authored rules, not an inferred classification.
  ([#508](https://github.com/SquashMerge/trellis/pull/508),
  [#530](https://github.com/SquashMerge/trellis/pull/530),
  [#535](https://github.com/SquashMerge/trellis/pull/535))
- **Architecture layout:** arrows run through gutters between band rows and through
  side channels, so they no longer cross layer bands or module chips. Arrow width
  grows with the reference-site count on a log scale; the tag and inspector keep the
  exact count. ([#639](https://github.com/SquashMerge/trellis/pull/639))
- **Architecture → Sequence:** boundary call sites are grouped by their measured
  caller. **Open sequence** opens that caller in Interactions with the boundary calls
  highlighted, and **Back** restores the Architecture selection. When a caller can't be
  opened, the reason is shown. `rules_check` and `rules_explain` evidence carries the
  caller's `stableLookupKey` so an agent can chain to other MCP tools.
  ([#691](https://github.com/SquashMerge/trellis/pull/691))
- **Sequence Interactions:** an enriched Sequence opens in **Interactions**, which
  draws only calls that cross from one participant to another, in source order, with
  returns, activation bars and loop frames. Internal steps are counted, not drawn.
  **Show all steps** switches to the full view. Syntax-only views, views with no shown
  call and the Files preview stay calls-first, keeping branch, loop, early-exit and
  unknown context. Returns, activations and loop frames are presentation; these are
  bounded **static possible paths, not runtime traces**, and hiding detail does not
  recover calls omitted by backend limits.
  ([#531](https://github.com/SquashMerge/trellis/pull/531),
  [#610](https://github.com/SquashMerge/trellis/pull/610),
  [#638](https://github.com/SquashMerge/trellis/pull/638))
- **Interactions bound after filtering:** the 200-arrow limit applies after internal
  steps are filtered out, so a cross-participant call after many internal steps is no
  longer cut. When arrows are cut, the status line says how many later interactions
  are not shown; a server-side cut reads `truncated by sequence limits`.
  ([#675](https://github.com/SquashMerge/trellis/pull/675))
- **Agent navigation:** MCP supports symbol search, declarations, cached source,
  calls and call graphs, references and architecture-rule checks. Readable
  `stableLookupKey` selectors can locate declarations across native-ID changes;
  collisions return candidates rather than guessing. Production declarations rank
  before test declarations within each find match tier, and `find_symbols` ranks
  callables and types before fields, parameters and variables. Responses retain
  bounds, partial-result warnings and paging/compact options.
  ([#534](https://github.com/SquashMerge/trellis/pull/534),
  [#604](https://github.com/SquashMerge/trellis/pull/604),
  [#607](https://github.com/SquashMerge/trellis/pull/607),
  [#676](https://github.com/SquashMerge/trellis/pull/676))
- **Call graphs that fit:** `trellis_call_graph` lists each semantic basis once
  instead of repeating it per proof, and accepts `compact:true`, which keeps nodes,
  edges, call sites and boundaries but drops binding, coverage and provenance detail.
  Defaults are sized to the 64 KB response: 8 nodes / 10 calls, or 50 / 60 with
  `compact:true`. `data.responseOmitted` counts what a byte cut left out. Compact
  `outgoing_calls` leaves out external calls unless `includeExternal:true`.
  ([#676](https://github.com/SquashMerge/trellis/pull/676))

## Native-first publication

An edit no longer waits for semantic evidence to be rewritten. Trellis publishes the
native index first, so the edit is readable right away, then catches semantic
evidence up in the background in small, resumable steps. Reads stay available
throughout.
([#668](https://github.com/SquashMerge/trellis/pull/668))

- While catch-up runs, responses label what they serve. Unchanged documents keep
  their previous validated semantic evidence as `olderBasis`, which is never called
  fresh. Changed documents whose producer input was retained show `catchingUp`;
  changed documents without retained input show `needsReimport`. Graph, Sequence and
  rule traversal stops at a changed or dependent document with a labelled boundary,
  never a negative conclusion. The browser, CLI and MCP share these labels.
- A `trellis producer run` import keeps what catch-up needs while the producer
  profile's effective inputs still hold, so later edits don't fall back to a full
  semantic rejoin.
- A read that arrives while the semantic commit holds the index waits for it, up to
  10 seconds, and then reads the new head, instead of failing with
  `rebuild_in_progress`.
  ([#682](https://github.com/SquashMerge/trellis/pull/682))

### Catch-up performance

The first native-first build took about 9 minutes to catch up after one edit on the
Trellis checkout. Later changes reduced it:

- Batched stage writes, per-page budget checks, indexed per-document join queries
  and stable SQLite query plans made catch-up about 9× faster in that PR's
  measurement (about 58 s against about 520 s).
  ([#679](https://github.com/SquashMerge/trellis/pull/679), merged through
  [#668](https://github.com/SquashMerge/trellis/pull/668))
- Semantic catch-up keeps its read connection between steps
  ([#692](https://github.com/SquashMerge/trellis/pull/692)) and reads stored rows by
  set rather than one row per query
  ([#693](https://github.com/SquashMerge/trellis/pull/693)).
- Status caches its semantic projection per published head, so polling status during
  catch-up no longer costs about half a core
  ([#694](https://github.com/SquashMerge/trellis/pull/694)). SQLite's global memory
  statistics are off, removing a process-wide mutex that catch-up and readers
  contended on ([#695](https://github.com/SquashMerge/trellis/pull/695)).

A 2026-11-05 dogfood run compared `0.1.0-rc.2` with main at `6e2bb818`, which adds
#692–#695 (later merges are fixes, not performance changes). It used a copy of the Trellis checkout with Rust and JavaScript imported
through `producer run` and a reader polling `status` and `symbols` throughout.
Medians of three rounds (two for `Cargo.toml`), in seconds of semantic catch-up:

| Edit | rc.2 | main `6e2bb818` |
| --- | ---: | ---: |
| First edit after daemon start | 40.2 | 31.6 |
| Later edits (four) | 11.3–15.8 | 9.4–13.4 |
| `Cargo.toml`, fresh daemon | 127.3 | 112.8 |

Native results were readable in mostly 1–3 s. No read failed in any run, and the
reader completed about twice as many reads during catch-up. Peak RSS was 3.6–7.4 GiB.
**The host load was not controlled** (1-minute load average 4–24), and single runs
varied up to 2×, so only the direction of these results is firm. They are not a
benchmark or a latency guarantee.

## Optional semantics and local search

- **Semantic evidence for all four languages:** explicitly plan and run pinned
  `rust-analyzer`, `scip-typescript` (JavaScript only), `scip-java` or `scip-python`,
  or import a supported artifact. Without an import, calls stay syntax-derived,
  with unverified targets. Fresh selected bindings improve Sequence expansion and
  Architecture references/rules; dynamic or unknown dispatch is not an execution
  path. Rust inherent methods can expand when directly bound; trait dispatch remains
  bounded, and Python dispatch remains unknown.
  ([#487](https://github.com/SquashMerge/trellis/pull/487),
  [#533](https://github.com/SquashMerge/trellis/pull/533),
  [#569](https://github.com/SquashMerge/trellis/pull/569))
- **One bad document no longer fails an import:** a SCIP document over a
  per-document cap, or with an invalid range, is admitted with no claims and reported
  partial (`document_cap_exceeded` or `invalid_range`); the rest of the artifact
  imports. A committed JavaScript bundle no longer blocks evidence for every other
  file. `scip-typescript` runs with a generated ES2022 `tsconfig.json` in its private
  worktree unless the checkout has its own.
  ([#635](https://github.com/SquashMerge/trellis/pull/635),
  [#655](https://github.com/SquashMerge/trellis/pull/655))
- **Honest coverage:** imports record per-document partial reasons. MCP reports
  native coverage separately (`nativeCoverageTier: "syntax"`) from semantic
  availability and item-level evidence. The status coverage summary now names failed
  documents first, for example `failed: 106 · not in producer output; partial: 252 ·
  native macro/attribute gaps only`. Edits can make imported evidence stale or
  possibly stale; re-run producers to refresh it. Missing evidence is not proof
  that a call or reference does not exist. Rust measures arguments in an allowlist
  of expression macros; other macros and attributes still leave gaps.
  ([#532](https://github.com/SquashMerge/trellis/pull/532),
  [#544](https://github.com/SquashMerge/trellis/pull/544),
  [#611](https://github.com/SquashMerge/trellis/pull/611),
  [#718](https://github.com/SquashMerge/trellis/pull/718))
- **Labels match the evidence used:** `rules_check` and `rules_explain` no longer warn
  `syntaxOnly` when a deviation rests on a semantic binding. Source-menu actions for
  measured declarations say `measured declaration` instead of `syntax`.
  ([#677](https://github.com/SquashMerge/trellis/pull/677))
- **Semantic evidence across upgrades:** Trellis keeps the last successful
  `producer run` input per producer. After an index rebuild, or an upgrade that
  changes the Trellis executable, it re-admits that batch through normal validation,
  without running the producer, and recovered evidence can be fresh again. Anything it
  can't recover is reported as **needs re-import** per language. A language that never
  had a producer is reported as not configured, not as needing re-import.
  ([#647](https://github.com/SquashMerge/trellis/pull/647),
  [#653](https://github.com/SquashMerge/trellis/pull/653),
  [#656](https://github.com/SquashMerge/trellis/pull/656),
  [#657](https://github.com/SquashMerge/trellis/pull/657),
  [#677](https://github.com/SquashMerge/trellis/pull/677))
- **Local embedding search:** `trellis model install` explicitly downloads and
  verifies the reviewed model. Weights are not bundled or downloaded implicitly;
  lexical search remains available without them. User-wide settings and managed
  snapshots work across checkouts, including MCP clients. Changes take effect at
  the next daemon startup, not by hot reload. Embedding similarity is search,
  not compiler semantic evidence.
  ([#606](https://github.com/SquashMerge/trellis/pull/606))
- **`trellis daemon stop`:** stops a running daemon without starting one. It closes
  idle MCP and `serve` connections, cancels producers and drains work, and returns
  once the daemon has released its socket and lock. `model install` and `model remove`
  report it as the restart command; the next command starts a daemon with the new
  model settings.
  ([#667](https://github.com/SquashMerge/trellis/pull/667))

## Selected reliability fixes

- A live `serve` connection keeps the daemon alive without an open browser;
  shutdown and lost-daemon diagnostics are explicit.
  ([#614](https://github.com/SquashMerge/trellis/pull/614))
- `/status/head`, which the browser polls, no longer builds the full status. It
  answers with a strong `ETag` and `304 Not Modified` for an unchanged head, and the
  browser revalidates with `If-None-Match`.
  ([#678](https://github.com/SquashMerge/trellis/pull/678))
- Read-only CLI commands (`status`, `symbols`, `query` and others) wait out brief
  index-use contention, for example just after a daemon restart, within their existing
  read deadline. If it lasts longer they fail with the typed
  `storage_busy: CLI read contention wait expired`.
  ([#688](https://github.com/SquashMerge/trellis/pull/688))
- `trellis producer status` works before the first index publication after a rebuild
  and reports `head: null`, instead of failing with `index_not_ready`.
  ([#708](https://github.com/SquashMerge/trellis/pull/708))
- Producer runs hold the process-group leader until the last signal, so cleanup
  can't signal a reused process ID, and report `groupStatus` as `empty`, `survived` or
  `unknown`.
  ([#633](https://github.com/SquashMerge/trellis/pull/633))
- GC reports and automatic GC retire the SQLite handles they open to inspect
  inactive checkouts.
  ([#623](https://github.com/SquashMerge/trellis/pull/623))
- Syntax errors, over-budget evidence, oversized/non-UTF-8 sources and oversized
  config inputs produce gaps or diagnostics instead of taking out the whole checkout.
  ([#575](https://github.com/SquashMerge/trellis/pull/575),
  [#580](https://github.com/SquashMerge/trellis/pull/580),
  [#584](https://github.com/SquashMerge/trellis/pull/584))
- Architecture shares background computation; MCP reads handle a build race, and
  unassigned-file counts agree with MCP even for module-only partial files.
  ([#571](https://github.com/SquashMerge/trellis/pull/571),
  [#587](https://github.com/SquashMerge/trellis/pull/587),
  [#613](https://github.com/SquashMerge/trellis/pull/613))
- Obsolete disposable indexes rebuild automatically. Incompatible MCP relay/daemon
  contracts refuse service with an actionable restart error. Unchanged documents
  retain their direct method bindings after a local edit elsewhere.
  ([#602](https://github.com/SquashMerge/trellis/pull/602),
  [#601](https://github.com/SquashMerge/trellis/pull/601),
  [#590](https://github.com/SquashMerge/trellis/pull/590))
- Shutdown ordering and fixture process cleanup have regression coverage. The MCP
  contract suite checks every advertised tool against real Rust and JavaScript
  imports, including structured errors.
  ([#615](https://github.com/SquashMerge/trellis/pull/615),
  [#625](https://github.com/SquashMerge/trellis/pull/625),
  [#626](https://github.com/SquashMerge/trellis/pull/626),
  [#616](https://github.com/SquashMerge/trellis/pull/616))

## Install

The release build targets are ARM64 macOS 14+ (Metal enabled) and x86-64/ARM64
Linux GNU (glibc 2.31+, CPU-only). Archives are unsigned and contain no model
weights. Tag publication requires all three target archives, their checksums and
build provenance. The installed-binary MCP smoke runner checks every advertised tool
and reports missing capabilities; it picks a probe from a language with a configured
producer and reports other languages as `not_configured`.
([#345](https://github.com/SquashMerge/trellis/pull/345),
[#354](https://github.com/SquashMerge/trellis/pull/354),
[#617](https://github.com/SquashMerge/trellis/pull/617),
[#622](https://github.com/SquashMerge/trellis/pull/622),
[#631](https://github.com/SquashMerge/trellis/pull/631))

Use the [quickstart](quickstart.md) for archive installation, explicit model setup,
`serve --token-file`, Claude Code/Desktop MCP configuration and producer setup.

## Known limitations

See [known limitations](known-limitations.md) before relying on the results. In
particular:

- **Performance gate:** the combined native-index and semantic-import
  [performance gate (#61)](https://github.com/SquashMerge/trellis/issues/61) remains
  open. Its historical measurements missed their time targets, and large repositories
  can take minutes to index and import. Semantic catch-up after an edit is measured
  above, not bounded.
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
- **MCP call answers ([#724](https://github.com/SquashMerge/trellis/issues/724),
  fix planned for 0.1.1):** `trellis_incoming_calls` can list a caller as `resolved`
  when its semantic binding targets a different function with the same name; this
  was seen with test helpers that share a name. Check the caller's `targetId` before
  relying on it. `trellis_call_graph` edge text is not capped at the advertised
  1 KiB (`calleeTextBytes: 1024`), even with `compact:true`.
- Classes draws no proven relationship edges. TypeScript/TSX/JSX parsing, remote
  SSH, Herdr integration and design handoff are not included. Producer imports do not
  imply complete coverage or runtime dispatch resolution.
