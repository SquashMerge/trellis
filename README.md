# Trellis

A diagram-first code browser with native JavaScript, TypeScript, Rust, Java and Python indexing, SQLite snapshots,
and an authenticated local inspector. Browse files, expand their functions/methods, and
open source-linked static sequence diagrams. These are possible paths, not runtime traces.

**New to Trellis?** Read the [quickstart](docs/quickstart.md) and the [known limitations](docs/known-limitations.md).

## Start here

Requires stable Rust/Cargo and a native C toolchain (tree-sitter and bundled SQLite).
The authenticated daemon currently requires Unix private-file permissions.
Official macOS ARM64 releases include Metal acceleration; Linux releases are CPU-only.
Ordinary Cargo builds keep CPU-only defaults. For a Metal-enabled macOS source install,
use `cargo install --path . --locked --features metal`. See [Release builds](docs/releases.md).

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
The daemon indexes the checkout and refreshes it when files change.
Ctrl-C stops `serve`; the daemon keeps running until you [stop it](docs/quickstart.md#2-start-trellis-and-connect-the-web-app).

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

## Local semantic search

`trellis_search_symbols` can combine lexical results with local voyage-4-nano embeddings.
Weights are optional. Nothing downloads implicitly: only an explicit install command can
fetch them. No separate Hugging Face CLI is needed:

```sh
trellis model install
trellis model list
trellis status --workspace /absolute/path/to/checkout
```

Install fetches the selected reviewed pin, verifies all nine files against pinned SHA-256
hashes, then publishes the complete snapshot atomically. A valid existing snapshot is
verified without fetching; an incomplete or invalid one is refused. Remove it explicitly
before reinstalling:

```sh
trellis model remove voyageai/voyage-4-nano --revision 5fb34e870f49f19e79c94d5e118f5ee730133279
trellis model install
```

The default model is `voyageai/voyage-4-nano` at revision
`5fb34e870f49f19e79c94d5e118f5ee730133279`. Snapshots are shared across checkouts under
`<modelsDir>/voyageai/voyage-4-nano/<revision>`. The default `modelsDir` is:

- macOS: `~/Library/Application Support/dev.squashmerge.trellis/models`
- Linux: `$XDG_DATA_HOME/trellis/models`, or `~/.local/share/trellis/models` when unset.

Defaults work without a configuration file and are never written automatically. Optional
user-wide overrides live at the literal path `~/.config/trellis/config.toml`:

```toml
models_dir = "/absolute/path/to/shared/models"
backend = "auto"

[model]
name = "voyageai/voyage-4-nano"
revision = "5fb34e870f49f19e79c94d5e118f5ee730133279"
```

All overrides are optional; `models_dir` must be absolute (no `~` expansion), and `backend`
accepts `auto`, `cpu`, or `metal`. Invalid configuration fails clearly instead of falling
back. Official macOS ARM64 releases include Metal; source builds need
`cargo install --path . --locked --features metal` for acceleration. Explicit Metal never
silently falls back to CPU. Trellis never executes downloaded Python.

This persistent setup also works for Claude Desktop without model environment variables in
its MCP launcher. `TRELLIS_SEARCH_MODEL` and `TRELLIS_SEARCH_BACKEND` remain development/test
overrides: environment wins over file, then defaults; an empty model override disables it.

**Settings are captured only at daemon startup.** Install/remove do not write configuration.
Neither model commands nor configuration edits restart/reconfigure a running daemon. Follow the
[manual restart guide](docs/semantic-search-local-cache.md#configuration-and-daemon-restart)
before expecting changes. `model list` prints JSON for the next startup without fetching,
loading weights or starting a daemon; its `filesPresent` is not hash verification.

CLI status, workspace describe and browser diagnostics include the daemon's effective
`searchIndex.configuration` and per-field sources, beside `backend` and `unavailableReason`.
Missing managed files preserve lexical retrieval with `model_not_configured`; a failed
verification/load reports `model_load_failed`. Configuration is not query readiness, and a
null reason does not prove vector availability. See the
[local model/cache guide](docs/semantic-search-local-cache.md) for details.

## What works

- Native tree-sitter JavaScript (`.js`, `.mjs`, `.cjs`), TypeScript (`.ts`, `.tsx`, `.mts`, `.cts`), Rust (`.rs`), Java (`.java`) and Python (`.py`) extraction.
- Optional SCIP import for all five languages through `trellis producer`; without it, calls stay syntax-only and unresolved.
- Source hashes, provenance, explicit unresolved boundaries, callbacks and control regions.
- Atomic full-index publication to SQLite; background refresh, progress and cancellation.
- Snapshot-consistent source reads and bounded, cycle-safe static call queries.
- Loopback-only API with bearer authentication, Host/Origin checks and a cached-source viewer.
- `trellis api`: the few browser routes the Herdr mod reads, through the daemon socket, so the mod can read another machine's daemon with Herdr's `exec` ([details](docs/daemon-v1.md#trellis-api-browser-routes-through-the-cli-817)).
- Inline file → method browsing and source-linked static sequence diagrams for all five supported languages.
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
Outgoing and incoming calls show each call's published semantic binding (resolution, target,
ambiguous candidates and freshness) from the sole published producer whose profile covers
each call's language, or from an explicit optional `semanticProducerId`. A language that none
or several producers cover stays syntax-only and the `syntaxOnly` warning says why. The call
graph, `trellis query` (`--semantic-producer-id`), `trellis symbols`, `trellis status` (per
language) and the browser apply the same per-language policy (owner ruling 2026-10-30, #483).
Find, declaration and outgoing results use compact `doc` references into their `documents` table.
Symbols, declaration data and calls include inclusive 1-based `lines` beside byte ranges when the
source is selectable; `NativeCandidate` has no `lines`. Use the returned `evidenceBasis` and
referenced document `contentHash` to pin a bounded `trellis_read_source` by path and lines.
Each tool's `content[0].text` is a deterministic summary of at most 512 UTF-8 bytes; the full,
schema-valid result is in `structuredContent`. MCP does not execute repository code or trigger
on-demand indexing, enroll grants, expose an HTTP listener or accept authenticated MCP intake.
See the [MCP contract](docs/mcp-readonly-pilot-contract.md) for limits and schemas. Run the native
protocol fixtures with `cargo nextest run --locked --features test-support --test mcp -E 'test(/^mcp_protocol::/)'`.

`trellis_call_graph` (#40) answers a rooted, revision-pinned outgoing call graph from one
`symbolId`: a pinned breadth-first traversal bounded by `depth` (0-5), `maxNodes` (1-150) and
`maxCalls` (0-500), optionally using one selected semantic producer (`semanticProducerId`). Only
fresh, exact, unambiguous internal direct or constructor bindings expand; every other call is a
labelled boundary, and frontiers, `partial` and warnings say what was left out. It is static
evidence, not a runtime trace. An answer over the response cap is reduced to a smaller `maxCalls`
and says so (`truncationReason:"response_bytes"`). The current SCIP importer sets `dispatch: unknown` on every call binding, so until dispatch evidence lands (#12, #14), `trellis_call_graph` on imported data stops at the root's direct callees.

[Reviewed architecture rules](docs/rules.md) adds three read-only MCP tools
(`trellis_references`, `trellis_rules_check`, `trellis_rules_explain`), the no-argument
`trellis_propose_rules` proposal prompt, and CLI JSON references/rules checks.
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

The browser client is embedded in the executable. Rebuild after editing `web/`.
Tests are offline and use temporary workspaces; no provider credentials or inference is used.
The existing selection-experiment budget is closed and is not changed by this implementation.

### Running selected tests

`./tools/verify` is the full gate. `tools/test-selected <selector>...` runs only the tests of
the files you name. A selector is a repo-relative path to an existing file, or
`path::test_name`. Every argument is a selector (there are no options), so a tool can append
selectors it did not write. All selectors are mapped before anything runs; if one cannot be
mapped, the script names it, runs nothing and exits 2. It never runs more than was selected.

| Selector | Command (run from the repository root) |
| --- | --- |
| `src/**.rs` in the library | `cargo nextest run … --lib -E 'test(/^<module>::/)'` |
| `src/**.rs` only in a binary (`src/main.rs`, `src/bin/<name>.rs` and their modules) | `cargo nextest run … --bin <name>`, with the same module filter |
| `tests/<name>.rs`, `tests/<name>/main.rs` | `cargo nextest run … --test <name>` |
| another file of `tests/<name>/` | `cargo nextest run … --test <name> -E 'test(/^<module>::/)'` |
| any of the Rust rows with `::test_name` | the same target with `-E 'test(/^<module>::(.+::)?<test_name>$/)'` |
| `tests/*.test.cjs`, `tests/*.test.mjs`, `tools/semantic-contract/test/*.test.mjs`, `tools/synthetic-cohorts/test/*.test.mjs` | `node --test <reporters> <file>` |
| `herdr-mod/test/*.test.mjs` | `node --import ./herdr-mod/test/register.mjs --test <reporters> <file>` |
| a Node row with `::test name` | adds `--test-name-pattern=^<test name, escaped>$` |
| `tools/*-test.py`, `tools/**/test_*.py` | `python3 <file>` |
| a Python row with `::test_method` or `::Class.test_method` | adds `-k '*.<name>'` |

`…` is `--locked --features test-support --no-fail-fast --no-tests=fail`, with
`CARGO_TARGET_DIR` set to the tests lane's directory (`target-test-support`, or
`$CARGO_TARGET_DIR-test-support`), so a selected run reuses that lane's build. Cargo runs
under the toolchain pinned in `rust-toolchain.toml`.

For Rust, `<module>` is the path by which the target's `mod` declarations reach the file,
`#[path]` included, so `src/foo_tests.rs` declared as `mod tests` in `src/foo.rs` runs
`foo::tests::`. A file's selector runs the tests of its module and of the modules below it;
a crate root (`src/lib.rs`, `tests/<name>/main.rs`) runs its whole target. A test name is
matched whole, anywhere in the file's module. Tests that live elsewhere are not run: for
`src/foo.rs` tested from `tests/core/foo.rs`, select the test file.

A command that runs no selected test fails, whatever its own exit status. nextest exits 4.
`node --test` exits 0 when a name matches nothing, so each Node command also gets a second
reporter that records the tests that finished; the script fails the command unless a test
that was not skipped ran (the named one, when a name is given; a suite's name is not a
test's). unittest before Python 3.12 exits 0 after running nothing, so the script reads its
`Ran N tests` report and fails the command when every test was skipped or none ran. A
Python test file is run as the file itself, so it must end in `unittest.main()` under
`if __name__ == "__main__":`, as all of them do. (`unittest discover -p <name>` is not
used for one file: it reads the name as a pattern and also runs a file of that name in a
package below.) The script itself runs on Python 3.9 and newer.

So a selector fails in one of two ways. A selector that cannot be mapped is refused
before any command runs (exit 2). A selector that maps but whose command runs no selected
test fails the run when that command is reached, which may be after earlier selectors'
commands ran: mapping does not prove that a test exists.

Each selector is one command; identical commands run once. Each command is printed before
it runs, a failure does not stop the later ones, and the exit status is the first failure's.
`TRELLIS_TEST_SELECTED_PRINT=1` prints the commands without running them.

Refused:

- a selector that starts with `-`, an absolute path, a path that does not exist, a
  directory, and a path that resolves outside the checkout, symbolic links included;
- `tests/browser/**`: the suite needs the built binaries and the pinned Chromium, so the
  browser lane owns it (`./tools/verify --lane browser`);
- `tests/mcp-catalog-sdk.test.mjs` and `tests/mcp-smoke.test.mjs` while the `tests/mcp`
  packages are not installed (`./tools/verify` installs them);
- `tools/selection/**` and `tools/extraction/**`: standalone research harnesses with their
  own packages, which no verify lane runs;
- `tests/fixtures/**` and `tests/proptest-regressions/**`: test data, with no single test;
- a Rust file that no library, binary or test target reaches, and a helper that several
  test binaries share (`tests/support/*.rs`, `tests/common/*.rs`);
- Rust files of a target whose modules cannot be read to their end: a module cycle, more
  than 20000 modules, a source file over 16 MiB or not UTF-8, or a `path` attribute other
  than `#[path = "…"]` with a plain or raw string (one under `cfg_attr`, a byte string, an
  escape other than `\\`, `\"` and `\'`). A file the library reaches still maps when
  the unread target is another one;
- a test name that is not an identifier path (Rust) or `test_method` / `Class.test_method`
  (Python); a Node name that is not one whole string literal of the file; a Python method
  that no class of the file, or not the named class, defines. Names built at run time and
  inherited test methods are refused by this;
- every other layout, such as `web/*.js`, `examples/`, `build/` and documents.

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

- [Quickstart](docs/quickstart.md)
- [Known limitations](docs/known-limitations.md)
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
Continue static sequence coverage and library adapters beyond Rust/Cargo. Terminal UI,
React/Tauri and database diagrams remain outside the current implementation.
