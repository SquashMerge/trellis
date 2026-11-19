# Trellis 0.2.1 release notes

**Version:** `0.2.1`. **Release date:** on tag. These notes cover pull requests merged
after the [0.2.0 release](release-notes-0.2.md) (tag `v0.2.0`) through
[PR #1243](https://github.com/SquashMerge/trellis/pull/1243). <!-- TO CONFIRM: PR #1243 is the last 0.2.1 merge; if it is not, name the last merged PR here. -->
This is a fix release.
Most of it makes reference and rules evidence more accurate, mainly for Rust: far fewer
references are `unknown`, and rust-analyzer's struct, trait, union, enum variant and
module definitions now bind. The MCP contract changes: `toolVersion` goes from **30 to
34**, `Kind` gains `variant`, `trellis_show` answers `recorded` instead of `shown`, and
there is a new non-retryable failure code, `input_bounds_exceeded`. The native extractor
goes from `native-v17` to `native-v20`, so every checkout is re-indexed on first use. The
index schema stays 43. See [Compatibility](#compatibility).

## References and rules accuracy

These changes apply to `trellis_references`, the rules check and explain, Architecture
and Deviations. They change how references are resolved when read, not stored facts,
unless a bullet says otherwise.

### Rust

- **Private globs:** a private glob import (`use super::*;`) maps only the names its
  module or block spells. A glob none of whose names is spelled is unknown with reason
  `rust_wildcard_unused`. A re-exporting glob (`pub use`) still exposes every name.
  ([#1104](https://github.com/SquashMerge/trellis/pull/1104))
- **Enum variant paths:** `Enum::Variant` maps to the enum's file when the path proves
  exactly one certain enum whose body spells the variant once, with no `cfg` or
  `cfg_attr`. Associated items, paths through a type alias or `Self`, globs over an enum
  and conditional variants stay unknown.
  ([#1207](https://github.com/SquashMerge/trellis/pull/1207))
- **Restricted visibility:** a `pub(self)`, `pub(super)` or `pub(in path)` item is
  certain in its own module and below it, and at a site in or below the module its path
  names. Before, it stayed unknown wherever it was looked up.
  ([#1217](https://github.com/SquashMerge/trellis/pull/1217))
- **Re-exports that share their module's name:** in a crate proved to be edition 2018
  or later, `mod parse; pub use parse::parse;` no longer makes every item re-exported
  from that module unknown from other files.
  ([#1165](https://github.com/SquashMerge/trellis/pull/1165))
- **Fewer opaque scopes.** A macro or attribute that could define names makes its
  module or block opaque, so later paths in that scope stay unknown. Fewer things do
  that now:
  - a statement only when it holds a macro call; `x != 1`, `!flag` or a `!` in a
    literal no longer does ([#1175](https://github.com/SquashMerge/trellis/pull/1175));
  - a macro in a `static` or `const` initializer makes only that initializer opaque,
    not its module ([#1197](https://github.com/SquashMerge/trellis/pull/1197));
  - built-in inert attributes (`allow`, `doc`, `deprecated`, `should_panic` and so on)
    and `rustfmt::`/`clippy::` tool attributes are matched by whole name, and the serde,
    clap and thiserror helper attributes are inert after the `#[derive]` that registers
    them ([#1189](https://github.com/SquashMerge/trellis/pull/1189));
  - `#[tokio::main]`, `#[tokio::test]` and `#[async_trait]`, once the path is proved
    to be the root manifest's dependency on `tokio` or `async-trait`
    ([#1195](https://github.com/SquashMerge/trellis/pull/1195));
  - the standard library's statement macros (the `format!` family, `vec!`,
    `matches!`, the assert families, `panic!`, `todo!`, `dbg!` and others), once proved
    to reach the standard library. `thread_local!` binds exactly the statics it names,
    as uncertain bindings. `include!` stays opaque
    ([#1229](https://github.com/SquashMerge/trellis/pull/1229));
  - `serde_json`'s `json!`, `rusqlite`'s `params!` and `named_params!`, `anyhow`'s
    `anyhow!`, `bail!` and `ensure!`, and `tracing`'s `trace!` to `error!`, once proved
    to reach that package ([#1233](https://github.com/SquashMerge/trellis/pull/1233)).

  This is about reference resolution only. Which macro arguments have their calls
  measured is unchanged ([known limitations](known-limitations.md#rust-macros)).
- **`src/main.rs` keeps its package:** external-package attribution follows Cargo's
  target auto-discovery, so `src/main.rs` and its modules keep their attribution when
  `[[bin]]` lists only other files.
  ([#1192](https://github.com/SquashMerge/trellis/pull/1192))
- **Edition 2015 stays cautious:** in a crate not proved to be edition 2018 or later, a
  `use` path that the crate root could also bind stays unknown instead of being guessed.
  ([#1180](https://github.com/SquashMerge/trellis/pull/1180))
- **Test roles from `cfg`:** a reference site under `#[cfg(test)]` on syntax that is not
  a declaration (a statement, block, match arm, `use` item, field, parameter or
  argument, for example) is test
  ([#1106](https://github.com/SquashMerge/trellis/pull/1106)). The new optional
  `test_features` key in `.trellis/rules.toml` names Cargo features that gate test
  support. Sites under `#[cfg(feature = "<name>")]` or
  `#[cfg(any(test, feature = "<name>"))]` then count as test. This changes only
  reference-site roles, when they are read. Declaration roles, `callerRole`, find,
  search, metrics and the treemap don't change, and nothing is re-indexed
  ([rules.md](rules.md#amendment-988-test-features);
  [#1109](https://github.com/SquashMerge/trellis/pull/1109)).
- **Less Rust resolution work:** resolution remembers its prelude walks and site
  paths. The answers and the work bound are unchanged. ([#1225](https://github.com/SquashMerge/trellis/pull/1225))

### JavaScript and TypeScript

- **Node 22 builtins and the nearest `package.json`:** bare imports of every public
  builtin of Node v22.23.5, bare and behind `node:`, are labelled from the
  `node-standard-library:v2` catalog (59 roots, including the `node:`-only `sea`,
  `sqlite` and `test`). Bare npm specifiers resolve against the captured `package.json`
  nearest the importing file, never an outer one, and `node_modules` is never read.
  ([#1221](https://github.com/SquashMerge/trellis/pull/1221))

### Semantic bindings

- **Rust structs, traits and unions:** a native Rust type accepts the `Struct`,
  `Trait` and `Union` kinds that rust-analyzer 1.98.1 states. Before, use-site
  references to them were published `unresolved`.
  ([#1237](https://github.com/SquashMerge/trellis/pull/1237))
- **Enum variants and associated types:** each Rust enum variant is a native
  declaration of the new kind `variant`, with the enum as its parent, and a trait's
  associated type is an `alias`. rust-analyzer's definitions of both now bind.
  ([#1241](https://github.com/SquashMerge/trellis/pull/1241))
- **Rust modules:** rust-analyzer's whole-file module definition binds the document's
  root module, so uses of a module resolve and keep their target across edits.
  ([#1242](https://github.com/SquashMerge/trellis/pull/1242))
- **Python call dispatch:** a plain or module-attribute call to a free function is
  `direct`, calling a class is `constructor`, and a member call to a method is
  `dynamic`, so Python call graphs expand. Everything else stays `unknown`.
  ([#1203](https://github.com/SquashMerge/trellis/pull/1203))
- **Agreeing bindings are one call:** binding rows of one call that name at most one
  target between them are one call. scip-typescript, for example, gives `new Set()` one
  row per declaration of a merged library symbol. Before, the read failed as corruption
  and the daemon rebuilt the index.
  ([#1116](https://github.com/SquashMerge/trellis/pull/1116))

On Trellis's own checkout, with rust-analyzer and scip-typescript imported, the
production references the rules check reports as unknown fell from 7,043 when the
rules ratchet was first recorded (2026-11-15) to 306 at
[PR #1242](https://github.com/SquashMerge/trellis/pull/1242). Not all of that drop is
resolution: part of it is bare imports of Node builtins and npm packages, which are now
labelled as external packages and so are no longer counted as unknown
([#1221](https://github.com/SquashMerge/trellis/pull/1221)).

## MCP

- **`trellis_show` answers `recorded`:** the outcome that records the request for open
  viewers is `recorded`, not `shown`. It means recorded, not applied: a pinned viewer
  keeps its view, and a viewer at another snapshot shows a note instead.
  ([#1213](https://github.com/SquashMerge/trellis/pull/1213))
- **A status filter pages only instances:** with `status`, `trellis_rules_check` leaves
  `unassigned` and `externalPackages` out of its pages, and `page.total` counts only the
  matching instances. ([#1213](https://github.com/SquashMerge/trellis/pull/1213))
- **The coverage warning says where:** the `coverageIncomplete` warning of find,
  search, declaration, outgoing and incoming calls names the incomplete tier and
  languages, for example `Selected semantic coverage is incomplete for rust`.
  ([#1213](https://github.com/SquashMerge/trellis/pull/1213))
- **Explain's `limit` reaches a pair's sites:** for a `$layers` or `$external` pair,
  `trellis_rules_explain`'s `limit` (1–50) is also the number of sites sampled, and
  `omittedEvidence` is the exact remainder. Before, every pair had at most 5 entries,
  and `omittedEvidence` counted only those 5. A check instance's `nextCall` asks for up
  to 50. No schema changes. ([#1121](https://github.com/SquashMerge/trellis/pull/1121))
- **Lookup keys are bounded by the document, not a clock:** the stable lookup-key
  derivation is bounded by the source alone (2 MiB, syntax depth 128, one million
  visits, the macro byte budget, and 80,000 tree-sitter progress ticks of parse work).
  Before, it also had a 250 ms parse budget, so whether a large document got a key
  could depend on machine load. A document beyond a bound, or with a missing
  implementation join, answers the new non-retryable `input_bounds_exceeded`, whose
  message names which, instead of the retryable `deadline_exceeded`.
  `deadline_exceeded` remains for the 10-second hang backstop.
  ([#1214](https://github.com/SquashMerge/trellis/pull/1214),
  [#1226](https://github.com/SquashMerge/trellis/pull/1226),
  [#1221](https://github.com/SquashMerge/trellis/pull/1221))
- **`variant`:** symbols, declarations and call tools can return `kind:"variant"`, a
  Rust enum variant, and `trellis_find_symbols` ranks it with fields.
  ([#1241](https://github.com/SquashMerge/trellis/pull/1241))
- **Proposal prompt:** the closed TOML grammar accepts `test_features`.
  ([#1109](https://github.com/SquashMerge/trellis/pull/1109))

## Daemon and producers

- **A new run's directory is never swept:** a `producer run` started while the
  daemon's activation sweep ran could lose its new run directory and fail with
  "No such file or directory". A run directory now appears only with its first
  `run.json`. ([#1240](https://github.com/SquashMerge/trellis/pull/1240))
- **Automatic re-runs wait out storage contention:** an automatic re-run that met
  `rebuild_in_progress` or `storage_busy` was recorded as a failure that stuck until
  the next source change. It is now held and retried at the next state change, such as
  the publication that held the index. `index_not_ready` is still a failure
  ([#1218](https://github.com/SquashMerge/trellis/pull/1218)). A held re-run is
  released by any publication after its launch was decided, instead of waiting for an
  unrelated change or the 60-second backstop
  ([#1222](https://github.com/SquashMerge/trellis/pull/1222)).
- **Automatic re-run admission and a lock-order deadlock:** a watcher-intent read
  could deadlock with the worker as the first serving session committed, blocking every
  later reader of the checkout's state
  ([#1079](https://github.com/SquashMerge/trellis/pull/1079)). An automatic re-run
  decided before the serving session was replaced could start before the new watcher's
  first inventory. The checkout runtime now admits the start on its own state
  ([#1092](https://github.com/SquashMerge/trellis/pull/1092)).
- **macOS, through a symlink:** running `trellis` through a symlink to its binary made
  `serve` exit at once, and a daemon started that way failed its first capture. It now
  pins the canonical executable.
  ([#1188](https://github.com/SquashMerge/trellis/pull/1188))
- **rust-analyzer imports on x86-64 Linux:** a run of a hash-verified rust-analyzer was
  refused at import with `invalid_artifact: tool binding`, because the batch carried
  the pin's version text, not what that binary reports. A batch from a Trellis run now
  records the version its hash-verified binary reports.
  ([#1138](https://github.com/SquashMerge/trellis/pull/1138))
- **Node heap failures show the fatal line:** for `node_heap_exhausted`, `stderrLines`
  holds V8's last GC line and its `FATAL ERROR` line instead of V8's native stack.
  ([#1202](https://github.com/SquashMerge/trellis/pull/1202))
- **More reads repair damage:** a CLI `symbols` or `query` read
  ([#1089](https://github.com/SquashMerge/trellis/pull/1089)) and the browser's Files,
  symbols, syntax-match, methods and Sequence reads
  ([#1202](https://github.com/SquashMerge/trellis/pull/1202)) that find a damaged
  selected source now schedule the re-index, as the other reads already did. The
  reply is still the typed refusal.
- **Stable keys on a busy machine:** references, rules and interfaces no longer derive
  stable lookup keys under a 250 ms budget. Before, on a busy machine a route handler
  or a measured caller could read as having no stable key.
  ([#1118](https://github.com/SquashMerge/trellis/pull/1118))
- **Nested repositories in the working tree:** an untracked nested Git repository is
  one added change at its directory. Before, it made the whole working tree unavailable
  (`unsupported_path`). ([#1202](https://github.com/SquashMerge/trellis/pull/1202))
- **`trellis metrics --source-set-id`** accepts the UUID form that a Git checkout's
  source set uses, as well as the path form.
  ([#1032](https://github.com/SquashMerge/trellis/pull/1032))

## Web app

- **Files outline:** a Rust enum lists its variants as members, with a struct-like
  variant's fields under the variant
  ([#1241](https://github.com/SquashMerge/trellis/pull/1241)). The outline's syntax facts
  are bounded by the file alone, with the same bounds as lookup keys, instead of 250 ms
  per file. A file beyond a bound says so in the outline's warnings on every read,
  never as a silent partial
  ([#1214](https://github.com/SquashMerge/trellis/pull/1214),
  [#1226](https://github.com/SquashMerge/trellis/pull/1226)).
- **Sequence has one renderer:** when Pele can't draw a view, the diagram area shows
  "This Sequence could not be drawn: <reason>" instead of falling back to a second
  renderer. ([#1013](https://github.com/SquashMerge/trellis/pull/1013))
- **Python `with` and `try` in Sequence:** their bodies are drawn. Before, each was one
  opaque step, and every call under a lock or a `try` disappeared. The `with` enter and
  exit, and `except`, `else` and `finally`, stay opaque boundaries.
  ([#1203](https://github.com/SquashMerge/trellis/pull/1203))

## Metrics

- **`maxNesting`:** an `else`, `else if`, case, `catch` or `finally` arm no longer adds
  a nesting level: it is as deep as its statement
  ([#1117](https://github.com/SquashMerge/trellis/pull/1117)). A body under a Rust
  `while`, a Java enhanced `for` or a Java try-with-resources was scored one level too
  shallow, and a Python `except` had no handler region. Both are fixed in `native-v19`
  ([#1215](https://github.com/SquashMerge/trellis/pull/1215)).
- `metricsVersion` stays `metrics:v1`. It names the record's shape, which is
  unchanged, so some values change under the same version.

## Development

- **0.2.x refactoring program:** most of the merged pull requests are moves and splits
  under [the refactoring spec](refactoring/0.2.x-spec.md), which are not meant to change
  behaviour.
- **Rules ratchet:** `tools/rules-ratchet.py` records `trellis rules check` on
  Trellis's own checkout, and the local verify runs it on request
  ([#1063](https://github.com/SquashMerge/trellis/pull/1063)). It compares lost coverage
  per pair and source ([#1144](https://github.com/SquashMerge/trellis/pull/1144)),
  tracks the sources of excepted pairs
  ([#1217](https://github.com/SquashMerge/trellis/pull/1217)), and its nightly outcomes
  are reported distinctly ([#1125](https://github.com/SquashMerge/trellis/pull/1125)).
  <!-- TO CONFIRM when PR #1243 merges: -->
  **[To confirm: PR #1243]** The rules ratchet no longer counts the synthetic-cohort
  templates ([#1243](https://github.com/SquashMerge/trellis/pull/1243)).
- **Tooling:** verify shares one browser and npm cache across checkouts
  ([#1033](https://github.com/SquashMerge/trellis/pull/1033)) and can take a shared
  heavy-run lease ([#1150](https://github.com/SquashMerge/trellis/pull/1150)).
  `tools/test-selected` runs the narrowest test command for selected files
  ([#1146](https://github.com/SquashMerge/trellis/pull/1146)). Dependencies build
  without debug info in dev and test; release builds are unchanged
  ([#1181](https://github.com/SquashMerge/trellis/pull/1181)).
- **Test reliability:** individual tests no longer depend on wall time, machine state
  or unpinned ordering ([#1040](https://github.com/SquashMerge/trellis/pull/1040),
  [#1107](https://github.com/SquashMerge/trellis/pull/1107),
  [#1155](https://github.com/SquashMerge/trellis/pull/1155),
  [#1212](https://github.com/SquashMerge/trellis/pull/1212),
  [#1224](https://github.com/SquashMerge/trellis/pull/1224),
  [#1230](https://github.com/SquashMerge/trellis/pull/1230)), and more orderings and
  budgets are pinned ([#1136](https://github.com/SquashMerge/trellis/pull/1136),
  [#1148](https://github.com/SquashMerge/trellis/pull/1148),
  [#1168](https://github.com/SquashMerge/trellis/pull/1168),
  [#1202](https://github.com/SquashMerge/trellis/pull/1202)). The corruption test of
  [#903](https://github.com/SquashMerge/trellis/issues/903) is deterministic
  ([#1089](https://github.com/SquashMerge/trellis/pull/1089)).
- **Dependencies:** the tree-sitter runtime 0.27.2
  ([#1157](https://github.com/SquashMerge/trellis/pull/1157)); `base64`, `notify`,
  `rusqlite`, `toml`, `toml_edit` and `uuid`
  ([#1152](https://github.com/SquashMerge/trellis/pull/1152)); npm packages used by
  tests and tooling ([#961](https://github.com/SquashMerge/trellis/pull/961),
  [#962](https://github.com/SquashMerge/trellis/pull/962),
  [#969](https://github.com/SquashMerge/trellis/pull/969),
  [#981](https://github.com/SquashMerge/trellis/pull/981),
  [#982](https://github.com/SquashMerge/trellis/pull/982)); and a CI action
  ([#971](https://github.com/SquashMerge/trellis/pull/971)).

## Compatibility

Trellis is pre-release; there is no backward compatibility.

- **MCP `toolVersion` 30 → 34.** Clients that pin the catalog must refresh it.
  - 31: the proposal prompt accepts `test_features`. No schema changes.
  - 32: the `trellis_show` outcome `shown` is replaced by `recorded`. A status filter
    on `trellis_rules_check` pages only instances, and the coverage warning text names
    tier and languages.
  - 33: the Node catalog descriptor is `node-standard-library:v2` with a 59-root
    `entry` enum, and a standard-library JavaScript or TypeScript `package` may start
    with one underscore. Every tool's failure `code` enum gains
    `input_bounds_exceeded`. A lookup-key input bound now answers it, not
    `deadline_exceeded`.
  - 34: `Kind` gains `variant` in find, search, declaration, outgoing calls, incoming
    calls and the call graph. A struct-like variant's fields now have the variant as
    their parent, so their `symbolId`s change.
  - With no catalog change: explain's `limit` reaches a pair's sites.

  Each change is an amendment in the [MCP contract](mcp-readonly-pilot-contract.md).
- **Native extractor `native-v17` → `native-v20`:** `native-v18` is the tree-sitter
  0.27.2 runtime, `native-v19` adds the control regions above, and `native-v20` adds
  `variant` declarations and trait associated types as `alias`. The index schema stays
  43. An index from an older extractor is rebuilt, not migrated.
- **`.trellis/rules.toml`** gains the optional top-level key `test_features`. A rules
  file without it behaves as before.
- **Contract amendments:** the [MCP contract](mcp-readonly-pilot-contract.md) (#985,
  #988, #979/#980, #1088, #1119, #1220, #1234), [contract-v1](semantic-evidence/contract-v1.md)
  (#473 kind table, Python dispatch #977, #1234 variants and associated types, #1235
  module definitions, #1126 producer version of a run, the JavaScript/TypeScript
  identifier grammar limit), Decision 0001 (#977), Decision 0011 (#988), Decision 0017
  Amendment 8 (#1126), [rules.md](rules.md) (#986, #988, #1088, #1209 and the Rust
  reference rules above), [files-view.md](files-view.md) (#1119, #1220, #1234),
  [local topology](local-topology.md) (#975, #1082, #1216), [metrics](metrics.md)
  (#984, #987) and the [sequence slice contract](sequence-slice-contract.md) (#1007).

## Upgrade notes

The usual [upgrade recovery](quickstart.md#upgrading-trellis) applies. The native
extractor is now `native-v20`, so each checkout is fully re-indexed on first use, and
Trellis retries each producer's last successful `producer run` batch. If a retry is
refused and leaves **needs re-import**, run the producer again.

## Known limitations

See [known limitations](known-limitations.md) before relying on the results. They are
unchanged from [0.2.0](release-notes-0.2.md#known-limitations), plus:

- **Rust references:** the limits of the new rules are recorded in
  [rules.md](rules.md). For example, a restricted item looked up from another workspace
  member, an enum variant reached through a type alias or `Self`, a two-hop glob chain
  to a dependency's macro, and `include!` stay unknown.
- **JavaScript and TypeScript identifiers:** the pinned grammars lex some characters
  that ECMAScript doesn't allow, such as a stray `§`, as identifier characters. Such a
  document can be measured as complete. This is a recorded grammar limit in
  [contract-v1](semantic-evidence/contract-v1.md).
  ([#1171](https://github.com/SquashMerge/trellis/pull/1171))
