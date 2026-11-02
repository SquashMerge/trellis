# Trellis 0.1 — draft release notes

**DRAFT — NOT PUBLISHED.** Owner review copy for [#529](https://github.com/SquashMerge/trellis/issues/529)
and [#618](https://github.com/SquashMerge/trellis/issues/618). Release version, tag, date and
qualification results are **TBD**. These notes describe merged code, not a qualified
archive or an available release. They authorize no build, tag or publication.

**Source cutoff:** 2026-11-02 18:03:28 UTC. Base and inspected `main` head:
[`95eed4305edee23e2309fe2a786d1bb6a94147f9`](https://github.com/SquashMerge/trellis/commit/95eed4305edee23e2309fe2a786d1bb6a94147f9),
through merged [PR #622](https://github.com/SquashMerge/trellis/pull/622).
This is a first-release feature summary, not a comparison with a prior release.

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
- **Calls first:** Sequence starts with measured calls while keeping their branch,
  loop, early-exit and unknown context. **Show all steps** restores admitted detail
  and incidental calls. These are bounded **static possible paths, not runtime traces**;
  hiding detail does not recover calls omitted by backend limits.
  ([#531](https://github.com/SquashMerge/trellis/pull/531),
  [#610](https://github.com/SquashMerge/trellis/pull/610))
- **Agent navigation:** MCP supports symbol search, declarations, cached source,
  calls and call graphs, references and architecture-rule checks. Readable
  `stableLookupKey` selectors can locate declarations across native-ID changes;
  collisions return candidates rather than guessing. Production declarations rank
  before test declarations within each find match tier. Responses retain bounds,
  partial-result warnings and paging/compact options.
  ([#534](https://github.com/SquashMerge/trellis/pull/534),
  [#604](https://github.com/SquashMerge/trellis/pull/604),
  [#607](https://github.com/SquashMerge/trellis/pull/607))

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
- **Honest coverage:** imports record per-document partial reasons. MCP reports
  native coverage separately (`nativeCoverageTier: "syntax"`) from semantic
  availability and item-level evidence. Edits can make imported evidence stale or
  possibly stale; re-run producers to refresh it. Missing evidence is not proof
  that a call or reference does not exist. Rust measures arguments in an allowlist
  of expression macros; other macros and attributes still leave gaps.
  ([#532](https://github.com/SquashMerge/trellis/pull/532),
  [#544](https://github.com/SquashMerge/trellis/pull/544),
  [#611](https://github.com/SquashMerge/trellis/pull/611))
- **Local embedding search:** `trellis model install` explicitly downloads and
  verifies the reviewed model. Weights are not bundled or downloaded implicitly;
  lexical search remains available without them. User-wide settings and managed
  snapshots work across checkouts, including MCP clients. Changes take effect at
  the next manual daemon startup, not by hot reload. Embedding similarity is search,
  not compiler semantic evidence.
  ([#606](https://github.com/SquashMerge/trellis/pull/606))

## Selected reliability fixes

- A live `serve` connection keeps the daemon alive without an open browser;
  shutdown and lost-daemon diagnostics are explicit.
  ([#614](https://github.com/SquashMerge/trellis/pull/614))
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
  contracts now refuse service with an actionable restart error. Unchanged documents
  retain their direct method bindings after a local edit elsewhere.
  ([#602](https://github.com/SquashMerge/trellis/pull/602),
  [#601](https://github.com/SquashMerge/trellis/pull/601),
  [#590](https://github.com/SquashMerge/trellis/pull/590))
- Local edit publication retains prepared semantic statements and promptly reports
  the existing rebuild-in-progress state when semantic writes block readers.
  Edit latency remains open work; these changes do not establish a performance
  qualification pass.
  ([#627](https://github.com/SquashMerge/trellis/pull/627))
- Shutdown ordering and fixture process cleanup have regression coverage. The MCP
  contract suite checks every advertised tool against real Rust and JavaScript
  imports, including structured errors.
  ([#615](https://github.com/SquashMerge/trellis/pull/615),
  [#625](https://github.com/SquashMerge/trellis/pull/625),
  [#626](https://github.com/SquashMerge/trellis/pull/626),
  [#616](https://github.com/SquashMerge/trellis/pull/616))

## Install and limitations

The release build targets are ARM64 macOS 14+ (Metal enabled) and x86-64/ARM64
Linux GNU (glibc 2.31+, CPU-only). Archives are unsigned and contain no model
weights. Release packaging and validated tag publication are implemented, but this
draft does not establish that any candidate passed qualification. The installed-binary
MCP smoke runner checks every advertised tool and reports missing capabilities;
it does not turn a partial-capability run into release qualification. Release-candidate
jobs also free unused hosted-runner SDK space before building.
([#345](https://github.com/SquashMerge/trellis/pull/345),
[#354](https://github.com/SquashMerge/trellis/pull/354),
[#617](https://github.com/SquashMerge/trellis/pull/617),
[#622](https://github.com/SquashMerge/trellis/pull/622))

Use the [quickstart](quickstart.md) for archive installation, explicit model setup,
`serve --token-file`, Claude Code/Desktop MCP configuration and producer setup.
See [known limitations](known-limitations.md) before relying on the results:
performance and memory work remains open, Classes draws no proven relationship
edges, and TypeScript/TSX/JSX parsing, remote SSH, Herdr integration and design
handoff are not included. Producer imports do not imply complete coverage or
runtime dispatch resolution.

## Owner qualification checklist — not completed by this draft

- [ ] Choose the actual release commit, version, tag and date; refresh these notes
  against that commit's merged PRs.
- [ ] Record all qualification evidence required by [#529](https://github.com/SquashMerge/trellis/issues/529)
  and the [release process](releases.md), including native target/archive checks,
  full verification and installed-binary/MCP/UI dogfood. A PR's test report is not
  release qualification.
- [ ] Resolve the remaining edit-latency and contention work
  [#620](https://github.com/SquashMerge/trellis/issues/620) and
  [#624](https://github.com/SquashMerge/trellis/issues/624). The physical-payload
  reuse experiment did not meet its latency gate; it is not a shipped feature or
  an approved schema change.
- [ ] Owner approves final notes and separately controls any tag/publication.
