# Trellis

A diagram-first code browser with native JavaScript, Rust, Java and Python indexing, SQLite snapshots,
and an authenticated local inspector. Browse files, expand their functions/methods, and
open source-linked static sequence diagrams. These are possible paths, not runtime traces.

## Start here

Requires stable Rust/Cargo and a native C toolchain (tree-sitter and bundled SQLite).
The authenticated daemon currently requires Unix private-file permissions.

```sh
cargo test --locked --all-targets
cargo run --locked -- index
mkdir -m 700 -p "$HOME/.trellis-private"
cargo run --locked -- serve --token-file "$HOME/.trellis-private/token"
```

Open **http://127.0.0.1:8877/**. Paste the token or load the private token file whose path
is printed by `serve`. Optional **Remember token on this browser** stores it locally;
Disconnect forgets it. See [token storage](docs/browser-token.md).
The workspace defaults to cwd; `--workspace PATH` changes both the browser and index roots.
The daemon does not index automatically: use **Index workspace** after source changes.
Stop the daemon with Ctrl-C.

Without SCIP, calls are visible as unresolved syntax; Trellis does not guess their targets.
For the existing feature-factory snapshot with semantic resolution, see
[the daemon guide](docs/daemon-v1.md#use-the-existing-semantic-snapshot).

Current storage uses fixed per-user cache/data locations **outside the repository**; `--state-dir`
is removed. A disposable index follows the checkout path. The Git workspace UUID remains shared
evidence identity. `serve` requires an explicit private `--token-file` outside the selected checkout.
Saved views, notes and Jev/ACP providers are retired; their existing files remain in place and ignored,
not migrated or deleted. Indexing never runs package scripts, installs source dependencies, or edits source.
See [local topology](docs/local-topology.md#t02--locations-and-store-responsibilities).

`trellis gc --report` only inventories derived-index candidates. A historical
index marked `eligible` is **not deleted**. Trellis has no derived-index deletion command or
automatic deletion path today. Future [#16](https://github.com/SquashMerge/trellis/issues/16)
automatic GC needs a separate guarded public contract: authenticate the exact historical schema,
root identity and age; hold a verified exclusive use lock; refuse live, hot-journal, unknown or
busy data; and never touch durable saved records. Deleting an eligible whole index invalidates
its generation's pins ([Decision 0006](docs/semantic-evidence/decisions/0006-revision-retention-and-index-gc.md));
retained revisions are protected only within a live index. Reporting eligibility alone must
never trigger removal.

## What works

- Native tree-sitter JavaScript (`.js`, `.mjs`, `.cjs`), Rust (`.rs`), Java (`.java`) and Python (`.py`) extraction.
- Optional JavaScript SCIP import; Rust, Java and Python currently have syntax-only, unresolved call evidence.
- Source hashes, provenance, explicit unresolved boundaries, callbacks and control regions.
- Atomic full-index publication to SQLite; background refresh, progress and cancellation.
- Snapshot-consistent source reads and bounded, cycle-safe static call queries.
- Loopback-only API with bearer authentication, Host/Origin checks and a cached-source viewer.
- Inline file → method browsing and source-linked static sequence diagrams for all four supported languages.
- Java/Python class diagrams with scoped candidate links, right-click related-class expansion, and source/method navigation.
- Reversible fluent Rust chain grouping, with every measured call retained.
- Automatic local Rust/Cargo library declaration catalog and explicit source viewing. Candidate type lanes are terminal; compiler receiver/dispatch resolution is still unavailable.
- Compact outgoing call rows with deliberate one-level branch expansion.

The workbench starts with immediate calls. Select a call or control to inspect its evidence,
then open its cached source. Expand deeper only when needed. Unsupported language constructs
remain explicit boundaries; parsing is not type resolution or an observed execution trace.
Java/Python indexing never runs Gradle, Maven, Python imports, decorators or package commands.

## Planned agent and semantic integration

The accepted direction supports agents running directly in a terminal (including terminal tabs in
Trellis or optional Herdr panes), and agents connected through ACP, primarily Mimir. Both use the
same portable MCP tools. The local `trellis mcp` stdio protocol layer is shipped; general coding-agent
ACP sessions and embedded terminals are **not implemented yet**. The authenticated browser
`serve` daemon remains separate.

Run `trellis mcp --workspace <path>` for the local, read-only stdio daemon client. The original six tools,
in order, are `trellis_workspace_describe`, `trellis_find_symbols`, `trellis_declaration`,
`trellis_outgoing_calls`, `trellis_incoming_calls` and `trellis_read_source`. The daemon serves evidence from one selected,
validated cached snapshot; before a valid head is ready, evidence tools return typed
`index_not_ready`, while describe can report not-ready status. Declaration takes a `symbolId` with
no `limit` or `view`; outgoing calls takes a `symbolId` and optional `limit`, with no `view`.
Find, declaration and outgoing results use compact `doc` references into their `documents` table.
Symbols, declaration data and calls include inclusive 1-based `lines` beside byte ranges when the
source is selectable; `NativeCandidate` has no `lines`. Use the returned `evidenceBasis` and
referenced document `contentHash` to pin a bounded `trellis_read_source` by path and lines.
Each tool's `content[0].text` is a deterministic summary of at most 512 UTF-8 bytes; the full,
schema-valid result is in `structuredContent`. MCP does not execute repository code or trigger
on-demand indexing, enroll grants, expose an HTTP listener or accept authenticated MCP intake.
See the [MCP contract](docs/mcp-readonly-pilot-contract.md) for limits and schemas. Run the native
protocol fixtures with `cargo test --locked --test mcp_protocol`.

[Reviewed architecture rules](docs/rules.md) adds three read-only MCP tools
(`trellis_references`, `trellis_rules_check`, `trellis_rules_explain`), the no-argument
`trellis_propose_layers` proposal prompt, and CLI JSON references/rules checks.
Code evidence remains pinned and cached; only the fixed `.trellis/rules.toml` is
read as current configuration with its own digest. Proposals are diffs for human
review, never server-applied changes.

The daemon already watches and refreshes native evidence. Snapshot text search, versioned diagram
artifacts, the unified terminal workbench and a browser project picker remain separate work.
Extend SCIP import beyond JavaScript through tested language adapters, initially Java; producing
semantic artifacts remains explicitly authorized work.

See the [local topology](docs/local-topology.md), [integration plan](docs/agent-integration-plan.md), [MCP contract](docs/mcp-readonly-pilot-contract.md),
[terminal workbench](docs/terminal-workbench-contract.md), [SCIP roadmap](docs/scip-multilanguage-plan.md),
[optional LSP assessment](docs/lsp-integration-plan.md), and the proposed
[semantic search plan](docs/semantic-search-plan.md).

## Development

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
node --test tests/shell-ui.test.cjs
cargo run --locked -- --help
```

`./tools/verify` runs the full local gate (formatting, clippy, tests and the browser lanes)
and is what automation runs. It bootstraps the pinned Node packages and Chromium and runs the lanes concurrently.

The browser client is embedded in the executable. Rebuild after editing `web/`.
Tests are offline and use temporary workspaces; no provider credentials or inference is used.
The existing selection-experiment budget is closed and is not changed by this implementation.

## Feature Factory

Repository features can be driven from GitHub Issues through Feature Factory. The tracked configuration
resolves issue numbers, `#number` references, and canonical issue URLs in `SquashMerge/trellis`.
See [Feature Factory operations](docs/feature-factory.md) for prerequisites, run invocation, gates, and
issue-writing guidance.

## Repository layout

- `src/`, `web/`: daemon and browser.
- `tests/mcp/`: pinned MCP conformance-test dependencies.
- `tests/fixtures/`: synthetic fixtures and retained feature-factory evidence.
- `tools/extraction/`, `tools/selection/`: standalone offline research harnesses.
- `docs/research/`: historical reports and screenshots, separate from runtime implementation.

See [research evidence](docs/research/README.md) before running historical tooling. Provider trials
and semantic-index generation require separate authorization. Private application artifacts are
excluded; local archives and runtime state must not be committed.

## License

Trellis is source-available under the [Business Source License 1.1](LICENSE) (`BUSL-1.1`).
You may use it in production free of charge if you are an individual, use it non-commercially
or for education or research, or are an organization (with affiliates) of no more than 5
employees and contractors, under US$5M annual revenue and under US$50M total outside funding.
Non-production use (reading, modifying, testing) is always free; anyone may also evaluate it in
production for up to 30 days. While a version is under the BSL, offering it, or a product
substantially derived from it, to third parties as a commercial hosted service or embedded
product is outside the free grant and needs a commercial license. Each version converts to the
Apache License 2.0 three years after it is first publicly distributed under BSL-1.1, and those
restrictions then end.
For commercial licensing, contact jcarreira@gmail.com. The [LICENSE](LICENSE) text governs; this
summary does not.

Versions published before the switch remain available under the MIT License. Third-party material
is not part of the Licensed Work: bundled fonts keep their own SIL OFL licenses under `web/fonts/`,
the Feature Factory fixtures (`tests/fixtures/extraction/inputs/feature-factory/` and
`tests/fixtures/extraction/feature-factory.*`) carry their original project's terms, and the
Candle/Voyage embedding adaptation retains its [upstream terms](src/search_embed/licenses/README.md).

## Read more

- [Architecture and accepted direction](SPEC.md)
- [MCP-first direct/ACP agent integration plan](docs/agent-integration-plan.md)
- [Per-checkout index, stdio MCP and native refresh topology](docs/local-topology.md)
- [Read-only stdio MCP contract](docs/mcp-readonly-pilot-contract.md)
- [Superseded: grant-based pilot implementation slices](docs/mcp-pilot-implementation-plan.md)
- [Superseded: grant-pilot slice 0 storage and cancellation findings](docs/mcp-pilot-slice0-findings.md)
- [Unified terminal and ACP workbench design](docs/terminal-workbench-contract.md)
- [Multi-language SCIP semantic import roadmap](docs/scip-multilanguage-plan.md)
- [Optional LSP bridge: Java-first assessment](docs/lsp-integration-plan.md)
- [Agent integration architecture diagrams](docs/architecture/agent-integration-context.md)
- [Daemon usage, data contract, security and limitations](docs/daemon-v1.md)
- [First-pass validation record](docs/daemon-validation.md)
- [Automatic local libraries and resolution limits](docs/automatic-libraries.md)
- [Rust indexing and static sequences](docs/rust-support.md)
- [Java and Python indexing and static sequences](docs/java-python-support.md)
- [Class diagrams and right-click related classes](docs/class-diagrams.md)
- [Current-directory file tree and external participants](docs/cwd-file-browser.md)
- [File → method → sequence workflow](docs/file-method-sequence.md)
- [Extraction spike](docs/research/EXTRACTION-RESULTS.md)
- [Jev/Opus exploratory comparison](docs/research/selection/HARD-RESULTS.md)

The [semantic-index program](https://github.com/SquashMerge/trellis/issues/8) now serves eligible
cached snapshots through the daemon and stdio MCP ([#17](https://github.com/SquashMerge/trellis/issues/17));
its watcher refreshes native evidence without running repository code. Next: multi-language SCIP
import and separately gated snapshot search, diagram artifacts and embedded terminal/ACP integration.
Continue static sequence coverage and library adapters beyond Rust/Cargo. TypeScript parsing, terminal UI,
React/Tauri and database diagrams remain outside the current implementation.
