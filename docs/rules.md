# Reviewed architecture rules

Trellis checks human-reviewed architecture in the fixed checkout file
`.trellis/rules.toml`. Checks do not infer layers or write rules. The
[Architecture workbench](architecture-view.md#review-and-authoring) can author reviewed
exceptions and exact file assignments through an explicit diff preview and confirmation.
MCP and CLI rules operations remain read-only.
Layers select indexed **native file modules by path** and proved **external package
identities**. Package targets are not native modules or resolved library symbols. It exposes
read-only references, rule check/explain tools, a proposal prompt and
`trellis rules check --format json`. See the [MCP wire contract](mcp-readonly-pilot-contract.md)
for transport, attribution and limits.

## Start with reviewed layers

```toml
version = 1

# Cross-peer and shared-to-ordered references default to forbid.
# To allow either direction explicitly, add before the layer tables:
# [layer_policy]
# peers = "allow"
# shared_outbound = "allow"

[layers.web]
order = 1
paths = ["src/web/**"]

[layers.listeners]
order = 1
paths = ["src/listeners/**"]

[layers.service]
order = 2
paths = ["src/service/**"]

[layers.shared]
shared = true
paths = ["src/model.rs", "src/testing.rs"]
```

Cross-peer references and shared-to-ordered references are deviations by default.
Omitting `[layer_policy]` or either key is equivalent to setting that key to
`"forbid"`. Set `peers = "allow"` or `shared_outbound = "allow"` explicitly to
allow that direction. An exact reviewed exception can approve an individual pair
without changing its underlying deviation status.

An integer `order` is explicit: lower means higher, equal integers are peers.
(#783: with two or more detected apps, `order` is compared only within an app.
See [Amendment (#783)](#amendment-783-apps-and-per-app-layers).)
Table order has no meaning. A layer has either `order` or `shared = true`, never
both. Path assignment uses the most specific matching patterns:

- An exact file path beats every glob.
- A glob is narrower only when its accepted valid path language is a proper subset
  of another glob's language, not when it has a longer prefix or fewer wildcards.
- Discard broader matches. If the remaining patterns all select one layer, use it.
  Equivalent or overlapping incomparable patterns in different layers produce a
  located ambiguity error. Repeated patterns within one layer deduplicate.

For example, `src/store/search/**` beats `src/store/**`, `src/?.rs` beats
`src/*.rs`, and `src/*??*` beats `src/*` on `src/ab`. By contrast,
`src/a*/b.rs` and `src/ab/*.rs` are incomparable on `src/ab/b.rs` and are
ambiguous across layers. A unique exact match wins even if broader globs tie.
Table/array order, layer names and layer `order` never break assignment ties.

Containment considers all valid paths, not only indexed files. Comparisons are
lazy, reused within a request, cancellable and charged to the existing bounded
work budget. Exhaustion fails atomically with `bounded_work`, never a guessed
winner or ambiguity. Unassigned files remain visible and unknown, not
automatically placed.

For an observed production structural reference pair:

| Direction | Result |
| --- | --- |
| Same layer | conforms |
| Ordered source to larger order (downward, including skipped levels) | conforms |
| Ordered source to smaller order (upward) | deviates |
| Different ordered layers with equal order | deviates unless `peers = "allow"` |
| Any source into shared, including shared into shared | conforms |
| Shared into ordered | deviates unless `shared_outbound = "allow"` |
| Unassigned endpoint or unresolved mapping/ownership | unknown |

(#783: a pair between layers that share no app is `unknown` with reason `cross_app`. See [Amendment (#783)](#amendment-783-apps-and-per-app-layers).)

Conformance describes **that observed structural pair**, not the absence of hidden
semantic dependencies in a module. A stricter no-skipping rule is not implicit.

### Layer display hint

By the [owner's #391 ruling](https://github.com/SquashMerge/trellis/issues/391), a
layer may set an optional presentation hint for the browser Architecture view:

```toml
[layers.fixture-inputs]
order = 9
paths = ["tests/fixtures/**"]
display = "hidden" # "expanded" | "collapsed" | "hidden"
```

- `"expanded"` draws the band open on its module chips on first view.
- `"collapsed"` draws a summary band (header, count chips and one row of module chips
  coloured by metric, or a short stack in a shared box, #449) until the viewer opens it. This is the same as no `display` field.
- `"hidden"` leaves the band out of the diagram and lists it in the hidden-layer note under
  the drawing (`N hidden layers · M files · E production edges`), from which the viewer can show it.

**Developer tooling is hidden** ([owner ruling 2026-10-31](https://github.com/SquashMerge/trellis/issues/524)):
scripts, build helpers, examples and other tooling are not runtime architecture, so their
layer carries `display = "hidden"`. It then waits behind the same hidden-layer toggle as
the fixture layer instead of reading as the top of the stack:

```toml
[layers.tooling]
order = 1
display = "hidden"
paths = ["tools/**", "build.rs", "examples/**"]
```

**Application layers.** The browser's *application layers* are the layers that are not
hidden by default: neither authored `display = "hidden"` nor an unhinted test or fixture
layer. Unassigned files stay in. The Sequence start list (most complex methods) and the
Classes tab's availability count only production callables in application layers. Both
read the default production [workbench metrics population](metrics.md#workbench-population),
which already leaves out fixture roots, stored test callables and authored hidden layers, so
`tools/*.py` in a hidden tooling layer neither leads the start list nor earns the Classes
tab. A checkout's own production code in a class-catalog language still does (Rust since #452).

Any other `display` value, or a non-string, is a located `invalid_rules` schema error.
Without the field a band starts collapsed (#391 ruling). Every other layer is drawn,
including one with no cross-layer references: the full stack (#449).
Under the default production metrics population, a test or fixture layer **without**
a `display` field (#424: every file is under a fixture root or holds only stored test
callables) joins the hidden-layer note until **Include tests and fixtures** is on. An
authored `display` hint on such a layer always wins.
Assignment, verdicts, reference counts, exceptions and every check, explain,
references and MCP result are unchanged. The Architecture layer inventory carries
`layers[].display` (`null` when absent). Under the explicit #395 browser
[`workbench=true` metrics population](metrics.md#workbench-population), authored
`display="hidden"` also excludes that layer's files from default production heat.
Include tests and fixtures restores those contributions. This scoped heat rule does
not change stored roles or policy. Client-side expansion and collapsing do not change
metric population.
Rules edits preserve the field like any other authored text.

## Amendment (#783): apps and per-app layers

**Status: owner-ratified 2026-11-07 (#783).** This section records the
[owner's 2026-11-07 ruling](https://github.com/SquashMerge/trellis/issues/783) and the owner's
ruling on the drafted wording ([PR #792](https://github.com/SquashMerge/trellis/pull/792)).
Where a checkout has two or more apps, it wins over the single global `order` space above,
the direction table and the [proposal workflow](#proposal-workflow). A checkout with zero or
one app behaves exactly as above, so its rules file stays valid unchanged.

**Apps come only from build manifests (ruling 1).** Detection reads manifest bytes from the
same admitted snapshot as the code. It never reads live files or runs `cargo` or `npm`. An
app is one of:

- **A Cargo package with a binary target.** That is the root `Cargo.toml` package or a
  member of the root manifest's `[workspace]` (`members` globs expanded against the
  snapshot, `exclude` honoured). The package needs an explicit `[[bin]]` or one of Cargo's
  default `src/main.rs` / `src/bin/*.rs` targets, unless `autobins = false`. Its name is
  `package.name`. Its root is the manifest's directory. Its entries are its binary target
  names. A package with several binaries is one app, and a library-only member is not an
  app. A nested Cargo package that is not a workspace member is not detected.
- **A `package.json`,** anywhere in the snapshot outside `node_modules/`. Its name is
  `name`, or the manifest's checkout-relative directory (`.` at the checkout root) when
  `name` is absent or not a nonblank string. Its root is the manifest's directory.
- **A Python project** (status: owner-ratified 2026-11-08 (#807)). That is a `pyproject.toml`,
  anywhere in the snapshot outside `node_modules/`, with a `[project]` table whose `name`
  is a nonblank string, or, without `[project]`, a Poetry `[tool.poetry]` table with one.
  Its name is that `name`. Its root is the manifest's directory. Its entries are the
  modules (the part before `:`) of its `[project.scripts]`, `[project.gui-scripts]` and
  `[tool.poetry.scripts]` targets, and may be empty. A `pyproject.toml` with neither table,
  such as a tool-only file, is not an app. Each file is judged alone: a uv
  `[tool.uv.workspace]` changes nothing, so a virtual workspace root is not an app, and a
  member, an excluded project and a project outside the workspace are each an app. Capture
  retains a nested `pyproject.toml` that sits above Python sources, as it does a nested
  `Cargo.toml` above Rust sources.
- **A runnable Maven or Gradle module** (status: owner-ratified 2026-11-10 (#854)). A
  `pom.xml` is a Maven app when its packaging is `war`, or `jar` (the default) with a
  declared main class: `maven-jar-plugin`'s manifest `mainClass`, `spring-boot-maven-plugin`,
  or `exec-maven-plugin`'s `mainClass`. A Gradle build file (`build.gradle` or
  `build.gradle.kts`) is a Gradle app when it applies the `application` plugin or sets
  `mainClass` or `mainClassName`. A library jar, an aggregator POM and a build without either
  is not an app. A multi-module build gives one app per runnable module: the modules are the
  POM `<modules>` and the root `settings.gradle` `include` list. Its root is the build file's
  directory, and its entries are the declared main classes. Capture retains a nested
  `pom.xml`, `build.gradle` and `build.gradle.kts` that sits above Java sources.

The app kinds are `cargo`, `npm`, `python`, `maven` and `gradle` (`python` owner-ratified
2026-11-08 (#807); `maven` and `gradle` owner-ratified 2026-11-10 (#854)).

This amendment adds no authored apps table. Authored apps, for checkouts that manifests
can't split, such as a Node backend and a JavaScript frontend in one package, are
[Amendment (#794)](#amendment-794-authored-apps): `[apps.<name>]` tables that override
detection under their roots.

The following detect no app:

- **A manifest whose path a hidden layer selects.** The path is assigned by the usual
  [path rules](#start-with-reviewed-layers), as if it were a file, and the assigned layer
  has `display = "hidden"`. This keeps tooling, test-harness and fixture manifests
  (`tools/x/package.json`, `tests/fixtures/**/Cargo.toml`) from becoming apps (#524).
- **A malformed or over-bound manifest.** It adds a `limitations` entry and is never an
  error, like other unproved manifests.

Detection shares the existing manifest byte and work bounds.

**App names.** A layer names an app by this exact, case-sensitive name. When two detected
apps share a name, neither can be named: naming it is a located `invalid_rules`
`ambiguity`. Both still appear in the app inventory. Because detection reads the code
snapshot, adding or removing a manifest can change which rules are valid, just as a second
source set can invalidate a portable exception.

**Each layer names its app (ruling 2).**

- **Ordered layers** use `app`, a nonblank string.
- **Shared layers** use `apps`, a nonempty array of nonblank strings. A shared layer serves
  the apps it lists. When `apps` is omitted, it serves every app.
- Either key on the other kind of layer is a located `invalid_rules` `schema` error, and so
  is a repeated name in `apps`.

```toml
[layers.entry]
app = "trellis"
order = 3
paths = ["src/main.rs", "src/http.rs", "src/http/**"]

[layers.shared]
apps = ["trellis"]
shared = true
paths = ["src/model.rs"]
```

| Detected apps | `app` and `apps` |
| --- | --- |
| None | Not allowed. Either key is a located `invalid_rules` `reference` (unknown app). All layers share one order space, as today. |
| Exactly one | Optional. A name given must be that app's (otherwise `reference`). An omitted `app` means that app. |
| Two or more | **Every ordered layer names its app,** including hidden tooling, test-harness and fixture layers. A missing `app` is a located `invalid_rules` `schema` error at the layer's table header. A shared layer may list `apps` or omit it to mean every app. An unknown name is a `reference` error. |

A layer's `app` is authoritative for its files in verdicts and views. Trellis does not check
that the layer's paths lie under the app's roots. Detection supplies the valid names, the
inventory and the grouping of unassigned files. An unassigned file belongs, for listing
only, to the app whose root contains it most specifically, or to no app. Unassigned files
are listed and never dropped. `unassignedFileCount` stays checkout-wide.

**`order` is compared only within an app (ruling 3).** Each app is its own order space.
Equal orders in two apps are not peers, and each app's numbering may restart at 1.
`[layer_policy]` stays checkout-wide and applies inside each app.

The direction table applies when the two layers **share an app**:

- for two ordered layers, the same app;
- for an ordered layer and a shared layer, the shared layer serves that app;
- for two shared layers, they serve a common app.

So references into a shared layer from an app it serves conform. References out of a shared
layer into an ordered layer of an app it serves follow `shared_outbound`, as today.

**References between apps are interface evidence (ruling 4), not `$layers` deviations.**
This adds a row to the direction table:

| Direction | Result |
| --- | --- |
| Source and destination layers that share no app | unknown, with reason `cross_app` |

`cross_app` joins `layer_unassigned` in the instance's `unresolved` reasons. Such a pair is
never counted as a deviation and never fails `trellis rules check`. It stays `unknown` until
[#686](https://github.com/SquashMerge/trellis/issues/686) measures interfaces between apps.
A later amendment then decides how a matched interface reports. The
[implementation notes (#784)](#implementation-notes-784) give the per-app counts that
`trellis rules check` prints.

The `rules:v2` instance shape is unchanged. Production and test rows, exact counts,
evidence and sampling are unchanged.

**Unchanged:**

- Tooling, test harnesses and fixtures stay hidden layers (#524), not apps. In a checkout
  with two or more apps, each names the app it belongs to. Assign fixtures to the app whose
  tests consume them.
- Exceptions stay per file pair, with the [portable form](#reviewed-exceptions-not-count-baselines)
  and matching unchanged. A cross-app pair is not a deviation, so it needs no exception; an
  exception that matches one keeps its `unknown` status and `excepted=true`, as for any
  instance.
- `$external` is unchanged.

**Worked example: Trellis itself.** [#784](https://github.com/SquashMerge/trellis/issues/784)
adds a one-file `web/package.json` (`{"name":"trellis-web","private":true}`). Before that,
`web/` has no manifest and detection finds one app.

With the rules below, the detected apps are:

- **`trellis`**: Cargo, root `.`, entries `trellis` (`src/main.rs`) and
  `trellis-producer-stub`;
- **`trellis-web`**: npm, root `web`.

The other manifests (`tools/**/package.json`, `tools/metrics-oracle/Cargo.toml`,
`tests/browser/package.json`, `tests/mcp/package.json` and every `tests/fixtures/**`
manifest) fall in hidden layers, so they detect no app. Path lists are shortened here; the
real file keeps today's lists.

```toml
version = 1

[layer_policy]
peers = "forbid"
shared_outbound = "forbid"

# --- app "trellis-web" (web/package.json). Its numbering restarts at 1.
[layers.web-tests]
app = "trellis-web"
display = "hidden"
order = 1
paths = ["tests/browser/**", "tests/*-ui.test.cjs"]

[layers.web-shell]
app = "trellis-web"
order = 2
paths = ["web/app.js", "web/shell.js"]

[layers.web-views]
app = "trellis-web"
order = 3
paths = ["web/architecture.js", "web/classes.js", "web/files.js", "web/sequence.js",
         "web/treemap.js", "web/*-pele.js", "web/navigation.js", "web/metric-heat.js",
         "web/viewer-metrics.js"]

[layers.web-vendor]
apps = ["trellis-web"]
shared = true
paths = ["web/vendor/**"]

# --- app "trellis" (root Cargo.toml). Orders compare only with other "trellis" layers.
# Fixtures, the test harness and tooling are hidden layers of this app, placed above the
# runtime layers so their references into the daemon are checked as downward.
[layers.fixture-inputs]
app = "trellis"
display = "hidden"
order = 0
paths = ["tests/fixtures/**", "tools/synthetic-cohorts/templates/**"]

[layers.tests]
app = "trellis"
display = "hidden"
order = 1
paths = ["tests/**"]

[layers.tooling]
app = "trellis"
display = "hidden"
order = 2
paths = ["tools/**", "src/bin/**", "build.rs", "examples/**"]

[layers.entry]
app = "trellis"
order = 3
paths = ["src/main.rs", "src/auth.rs", "src/http.rs", "src/http/**", "src/daemon/**", "src/mcp/**"]

[layers.query]
app = "trellis"
order = 4
paths = ["src/navigation.rs", "src/references.rs", "src/references/**", "src/rules.rs", "src/rules/**"]

[layers.indexing]
app = "trellis"
order = 5
paths = ["src/indexer.rs", "src/index_coordinator.rs", "src/index_coordinator/**", "src/producer/**"]

[layers.storage]
app = "trellis"
order = 6
paths = ["src/store.rs", "src/store/**", "src/search_vectors.rs"]

[layers.shared]
apps = ["trellis"]
shared = true
paths = ["src/model.rs", "src/testing.rs", "src/lib.rs"]
```

What the example shows:

- `web/app.js` → `web/architecture.js` is checked against the `trellis-web` order only.
- `web-views` and `entry` both have order 3, but they are in different apps, so they are
  not peers.
- A reference from `src/bin/**` (tooling) into `src/store.rs` (storage) is an ordinary
  checked `$layers` edge inside the `trellis` app: downward, so it conforms.
- `tests/browser/**` and `tests/*-ui.test.cjs` beat the broader `tests/**`, so the web tests
  belong to `trellis-web`. `tests/mcp/**` stays in `tests` (`trellis`).
- A reference between a `trellis-web` layer and a `trellis` layer (both non-shared) is
  `unknown`/`cross_app`: interface evidence for #686, not a deviation.

## Implementation notes (#784)

**Status: proposed (owner-delegated, run 803).** This section records how
[#784](https://github.com/SquashMerge/trellis/issues/784) implements
[Amendment (#783)](#amendment-783-apps-and-per-app-layers). It adds no rule beyond the
amendment. Where the two differ in detail, this section is the exact behaviour.

**App kinds.** An app's `kind` is `cargo`, `npm`, `python`, `maven` or `gradle`. `python` is
a ratified app kind (owner ruling 2026-11-08), detected from `pyproject.toml` since
[#807](https://github.com/SquashMerge/trellis/issues/807). `maven` and `gradle` are ratified
app kinds (owner ruling 2026-11-10), detected from `pom.xml` and Gradle build files since
[#854](https://github.com/SquashMerge/trellis/issues/854).

**Captured manifests.** Detection reads only manifest bytes the indexer captured with the
code snapshot, and never the live files. The snapshot retains every present `package.json`
and `Cargo.toml` at any depth, outside `node_modules/`, under the existing byte bounds. It
retains the root `pyproject.toml` and each nested `pyproject.toml` that sits above a captured
Python source (#807), under the same bounds. It retains the root `pom.xml`, `build.gradle`,
`build.gradle.kts`, `settings.gradle` and `settings.gradle.kts`, and each nested `pom.xml`,
`build.gradle` and `build.gradle.kts` that sits above a captured Java source (#854). The
per-manifest size bound applies to the root manifests only; a nested manifest counts toward
the total bound. A manifest that is absent from the snapshot is not an app and is not unread.

**Outcome table.** Every captured manifest gets exactly one outcome:

| Manifest | Outcome |
| --- | --- |
| Any path with a `node_modules` component | not listed |
| Absent (no captured bytes) | no app |
| Root `Cargo.toml`: invalid TOML or over the size bound | unread |
| Root `Cargo.toml` with `[workspace]` and no `[package]` | no app; its members are still detected |
| `Cargo.toml` with `[package]` but no string `package.name`, or a `[[bin]]` without a string `name` | unread |
| `Cargo.toml` with `[package]` and no binary entries (library only) | no app |
| `Cargo.toml` with `[package]` and at least one binary entry | app (`cargo`) |
| Workspace member `Cargo.toml`: invalid TOML or over the size bound | unread |
| Workspace member `Cargo.toml` without `[package]` | no app |
| Nested `Cargo.toml` that is not a workspace member | no app; never parsed for identity |
| `package.json` that is not a JSON object, or is over the size bound | unread |
| `package.json` that is a JSON object | app (`npm`) |
| `pyproject.toml`: invalid TOML, over the size bound, `project` or `tool` or `tool.poetry` not a table, a scripts table or target of the wrong type, or a `[project.scripts]` or `[project.gui-scripts]` target that is not a `module:attr` string | unread |
| `pyproject.toml` with neither `[project]` nor a `[tool.poetry]` that has `name` | no app |
| `pyproject.toml` whose `[project].name` (or, without `[project]`, `[tool.poetry].name`) is not a nonblank string | unread |
| `pyproject.toml` with `[project]` or a named `[tool.poetry]` | app (`python`) |
| `pom.xml` that is not well-formed XML in the reader's subset (a DOCTYPE, an unknown entity, an unclosed or mismatched element), not UTF-8, over the size bound, not a `<project>`, or without a nonblank `artifactId` | unread |
| `pom.xml` below a present `pom.xml` whose `<modules>` chain does not list its directory | no app; never parsed |
| `pom.xml` with packaging `pom`, a packaging other than `jar` or `war`, or `jar` with no declared main class | no app; its `<modules>` are still followed |
| `pom.xml` with packaging `war`, or `jar` with a declared main class | app (`maven`) |
| Root `settings.gradle` or `settings.gradle.kts` | no app; unread when not UTF-8, over the size bound, or with an unterminated string or comment or unbalanced brackets |
| Gradle build file that the root settings do not include, below the root, when root settings exist; or below another present build file when they do not | no app; never parsed |
| Gradle build file that is not UTF-8, over the size bound, or has an unterminated string or comment or unbalanced brackets | unread |
| Gradle build file without the `application` plugin, `mainClass` or `mainClassName` | no app |
| Gradle build file with the `application` plugin, `mainClass` or `mainClassName` | app (`gradle`) |

An unread manifest detects no app. A manifest that a hidden layer selects (see
[Amendment (#783)](#amendment-783-apps-and-per-app-layers)) is neither an app nor unread.

**Defaults.**

- **Workspace members** come from the root manifest's `[workspace].members` and `exclude`,
  matched as globs whose `*` does not cross `/`, against the directories of the captured
  nested `Cargo.toml` files. `exclude` wins. A member is parsed only when it is captured.
- **Cargo entries** are the explicit `[[bin]].name` values. Unless `package.autobins` is
  `false`, they also include `package.name` when `<root>/src/main.rs` is a snapshot
  document, and the file stem of each `<root>/src/bin/*.rs` that no `[[bin]].path` names.
  Entries are sorted and deduplicated.
- **npm** name is `name` when it is a nonblank string, otherwise the manifest's directory
  (`.` at the checkout root). Its entries are empty.
- **Python** name is `[project].name`, or `[tool.poetry].name` without `[project]`. Its
  entries are the modules (the text before `:`, trimmed) of the `[project.scripts]`,
  `[project.gui-scripts]` and `[tool.poetry.scripts]` targets. A `[project]` script target
  must be a `module:attr` string with both sides nonblank; anything else, including a table,
  makes the manifest unread. Only `[tool.poetry.scripts]` accepts a table target: it uses
  its `callable`, and one without `callable` (a file script) adds nothing. Entries are sorted
  and deduplicated, and may be empty. A uv `[tool.uv.workspace]` changes nothing: each
  `pyproject.toml` is judged alone.
- **Maven** modules: a present `pom.xml` with no present `pom.xml` in an ancestor directory
  is judged alone. Each judged POM's `<module>` values, resolved against its directory (a
  trailing `/pom.xml` dropped; a path leaving the checkout ignored), name further judged POMs.
  The POM is read only as XML text: no parent POM, profile, `pluginManagement` entry or
  property inheritance is read. A Maven app's name is its `artifactId`, with no fallback to
  a parent's. Its entries are the nonblank main classes the three plugins declare (in their
  `configuration` or an execution's), with a whole-value `${name}` resolved from the POM's own
  `<properties>`; `spring-boot-maven-plugin` alone declares a main class it finds itself, so
  its app may have no entries.
- **Gradle** projects: the root build file, and each build file whose directory is a project
  that a literal `include` of the root `settings.gradle` or `settings.gradle.kts` names
  (`:lib:core` is `lib/core`). Without root settings, each build file with no present build
  file in an ancestor directory is judged alone. Build scripts are code: they are scanned as
  tokens, never run, so `application` is recognised only as `id 'application'`,
  `id("application")` or a bare `application` in `plugins {}`, or as `apply plugin:
  'application'` / `apply(plugin = "application")`, and `mainClass`/`mainClassName` only as
  an assignment, `.set(…)` or a Groovy call with a value. The name is Gradle's project name:
  the root's literal `rootProject.name`, else `.`; an included project's last path segment;
  a standalone nested project's directory name. Entries are the literal (uninterpolated)
  main classes assigned.
- **Roots** are the manifest's directory. `.` contains every path, and a path belongs to
  the app with the most specific containing root.

**Unread manifests.** When `n` manifests are unread, the check output adds the limitation
"`n` build manifest(s) could not be read for app detection (malformed or over the size
bound); they detect no app." Over the work bound, detection fails `bounded_work`.

**`apps` and `noApp` in check output.** `trellis rules check --format json` and the MCP
`trellis_rules_check` payload gain:

```text
apps:[{name,source,manifest,counts:Counts}]   # one entry per app of the inventory (#794)
noApp:{counts:Counts}
```

(#794: `source` is `manifest` or `override`, `manifest` is null for an authored app, and the
inventory lists authored apps by name before the detected apps in manifest-path order. See
[Amendment (#794)](#amendment-794-authored-apps).)

Both count production instances only. `trellis_rules_explain` carries neither field. With no
detected app, `apps` is empty and every instance is under `noApp`.

**The `bucket` rule.** Each production instance is given to at most one app:

- A `$layers` instance with both a source and a destination layer goes to the single app
  those two layers have in common. It goes to no app when they share none (`cross_app`) or
  share more than one.
- Any other instance (a `$layers` instance with an unassigned end, `$external`, or a
  declaration rule) is attributed by its own file. The file's layer decides when that layer
  names exactly one app; otherwise the app whose root most specifically contains the file
  decides. For these instances the layer is the source layer when it is set, otherwise the
  target file's assigned layer.

A file in a single-app layer counts under that app even when it lies outside that app's
root. Instances in no app are exactly the `cross_app` instances, instances between layers
that share more than one app, instances attributed by a file that is in no single-app layer
and outside every app root, and every instance when no app is detected.

A deviation that involves a shared layer has exactly one common app, because only shared to
ordered can deviate. So `Σ apps[].counts + noApp.counts = counts`, field by field. A
`cross_app` instance stays in `counts.unknown` and in `noApp.counts.unknown`; it is dropped
from the Architecture summary's layer edges.

**Single-app output.** With exactly one detected app, `apps[0].counts` equals `counts` only
when no row's own file is unassigned and outside the app's root. Such rows count under
`noApp`. So a single-app checkout whose app root is a subdirectory, and which has unassigned
files outside that root, reports part of its CLI and MCP per-app output under `noApp`.

**Validation order.** Layers are validated in name order and the first error wins:

- With no detected app, any `app` or `apps` key is a `reference` error.
- With one app, a name that is not that app is a `reference` error.
- With two or more apps, an ordered layer without `app` is a `schema` error at the layer's
  table header. A name matching no app is a `reference` error. A name matching two or more
  apps is an `ambiguity` error that names both manifests.

**Limitation.** App detection reads manifests only. A checkout that one manifest cannot
split, such as a Node backend and a JavaScript frontend in a single `package.json`, is one
detected app; [Amendment (#794)](#amendment-794-authored-apps) lets the rules file author the
split. Trellis's own `web/package.json` makes its web files a second app.

## Amendment (#794): authored apps

**Status: owner-ratified 2026-11-08 ([#794](https://github.com/SquashMerge/trellis/issues/794)).**
This section records the owner's 2026-11-08 rulings on #794: D1–D6 and D8 accepted as
recommended ([ruling](https://github.com/SquashMerge/trellis/issues/794#issuecomment-6730859881)),
and D7 superseded by the rename and widening of the proposal prompt
([ruling](https://github.com/SquashMerge/trellis/issues/794#issuecomment-6730845360)). It
adds authored apps to [Amendment (#783)](#amendment-783-apps-and-per-app-layers) and the
[implementation notes (#784)](#implementation-notes-784). A rules file without `apps`
behaves exactly as under those sections.

**Authored apps (D1, D5).** `apps` is the eighth closed top-level key: a map of
`[apps.<name>]` tables, like `[layers.<name>]`. The table key is the app's name: nonblank and
case-sensitive, the name layers use in `app` and `apps`. The TOML parser rejects a repeated
table. Each table has exactly one key:

| Key | Rule |
| --- | --- |
| `roots` | Required. A nonempty array of distinct strings. Each is `.` (the whole checkout) or a literal checkout-relative directory: no leading or trailing `/`, no empty, `.` or `..` component, no glob character (`*`, `?`, `[`, `]`, `{`, `}`, `!`), backslash, colon or NUL. |

`kind`, `language`, `entries` and every other key are unknown keys, so a located `schema`
error. An authored app has no manifest. On the wire its `kind` and `manifest` are null and
its `entries` empty; `languages` is measured from its files, as for any app.

```toml
# One root package.json holds a Node backend and a JavaScript front end.
[apps.backend]
roots = ["."]

[apps.web]
roots = ["client", "shared/ui"]
```

**Shadowing and precedence (D2, D3).** The effective inventory is computed in this order:

1. every manifest's outcome, as in the [implementation notes (#784)](#implementation-notes-784);
2. a manifest that a hidden layer selects detects no app, as before;
3. **shadowing:** a detected app is dropped when its root equals, or lies under, any
   authored root;
4. **union:** the authored apps, in name order, then the surviving detected apps, in
   manifest-path order.

An authored app always wins over detection inside its roots. Detection outside every
authored root is untouched. A file belongs, for listing and for the `bucket` rule, to the
app whose root contains it most specifically, over every root of every app, authored or
detected; on a tie the earlier app in the inventory wins. Authored roots may nest (`.` and
`client`), so the most specific wins. One root in two authored apps is an error.

| Intent | Expressed as | Result |
| --- | --- | --- |
| Split one manifest (a backend and a front end, a server and a CLI) | `[apps.backend] roots=["."]` and `[apps.web] roots=["client"]` | The root manifest's app is shadowed. Files under `client/` go to `web`, every other file to `backend`. |
| Merge a workspace's members | `[apps.platform] roots=["crates"]` | Every member under `crates/` is shadowed; one app remains. |
| Add an app with no manifest (a front end inside a Rust crate) | `[apps.ui] roots=["ui"]` | The crate's detected app survives; `ui/` files go to `ui`. |
| Rename a detected app | `[apps.good-name] roots=["<the detected root>"]` | The equal root shadows it. |
| Drop a manifest that is not an app | a hidden layer that selects the manifest | Unchanged from #783. |

**Layers.** `app` and `apps` resolve against the effective inventory. The table of zero, one
or two or more apps in [Amendment (#783)](#amendment-783-apps-and-per-app-layers) counts
effective apps. Order is compared per app, and the direction table, `cross_app`, the
`bucket` rule and `Σ apps[].counts + noApp.counts = counts` are unchanged. A layer's `app`
stays authoritative over roots. A detected app that an authored root shadows can no longer
be named.

**Validation (D3, D4, D6).** Each error is a located `invalid_rules`. The file is parsed
first; then, against the same admitted snapshot as detection, the authored apps are checked
in name order, each app's roots before its name, and the first error wins. Layer `app` and
`apps` are then checked against the effective inventory as in #784.

| Condition | `kind` | Location |
| --- | --- | --- |
| Missing `roots`, a blank app name, or an unknown key | `schema` | the table, or the unknown key |
| `roots = []`, a non-string root, an invalid root (including one outside the checkout, such as `../x` or `/x`), or a root repeated in one app | `schema` | the list or the root |
| More than 1024 apps (`too_large`, like the other named collections) or more than 1024 roots in one app | `too_large` | the file, or the first root over the limit |
| The same root in two authored apps | `ambiguity` | its second occurrence, in name order |
| A root with no indexed document under it (D6) | `reference` | the root |
| An authored name equal to the name of a detected app that survives shadowing (D4) | `ambiguity` | the authored table; the message names the manifest |

Detected apps that share a name keep the #783 rule: they are an error only when a layer
names them, and authoring an app at their roots is the fix. Because root validity reads the
snapshot, adding or removing a directory can change which rules are valid, as manifests
already do.

**Output.** The rules check `apps[]` entry is `{name, source, manifest, counts}`, with
`source` `manifest` or `override` and `manifest` null for an authored app. It keeps one entry
per inventory item, in inventory order. The Architecture summary's `apps[]` carries
`source`, `kind` (null), `manifest` (null), `roots` (as authored, possibly several) and
`entries` (empty) for an authored app. Interfaces are extracted when detected and authored
apps together number two or more, and their ends resolve through the effective apps, so an
authored split of one manifest has interfaces between its apps.

**Edits.** Rules edits (exception preview and apply, file assignment) keep `[apps]` tables
byte for byte, like other authored text.

**Proposal prompt (D7 superseded).** The MCP prompt is renamed `trellis_propose_rules`
(formerly `trellis_propose_layers`) and covers the whole rules file. It lists the effective
apps, authored first, and explains how to author `[apps.<name>]` tables, so the agent may
draft them for review. It keeps authored tables as written unless the evidence shows they
are wrong. See [MCP Amendment (#794)](mcp-readonly-pilot-contract.md#amendment-794-authored-apps-and-the-trellis_propose_rules-prompt).

**Worked example: mimir (acceptance).** mimir has a Python service (`pyproject.toml`,
`mimir-agent`) and a Vite front end (`package.json`, `mimir-frontend`) whose manifests both
sit at the checkout root, and a Python plugin in `plugins/cultivate`. Detection alone gives
two apps with root `.`, so every file outside the plugin, including the front end's, goes to
whichever root manifest sorts first. The authored override expresses the real layout:

```toml
[apps.mimir-agent]
roots = ["."]

[apps.mimir-frontend]
roots = ["frontend"]

[apps.mimir-cultivate]
roots = ["plugins/cultivate"]
```

The root `.` shadows every detected app, so the plugin is authored too (keeping its
detected name is allowed, because its detected app is shadowed). Files under `frontend/` go
to `mimir-frontend`, files under `plugins/cultivate/` to `mimir-cultivate`, and every other
file to `mimir-agent`.

## Amendment (#855): interface wrappers

**Status: owner-ratified 2026-11-10 ([#855](https://github.com/SquashMerge/trellis/issues/855)).**
The owner ruled that JavaScript and TypeScript request wrappers for HTTP interface evidence are
configured in this file. They are not configured in each client's `package.json`.

`[interfaces]` is an optional top-level table, and `wrappers` is its only key. Each
`[[interfaces.wrappers]]` entry has these keys:

- `callee` (required): an identifier or `object.property`;
- `method`: `"options"` (the default) or `"argument"`;
- `at`: 1 to 8, with a default of 1;
- `prefix`: a rooted literal with no query, fragment or pattern;
- `root`: a directory, following the `[apps]` root rules, with a default of `.`;
- `app`: the name of one app of the effective inventory, the same inventory `check` uses.
  The wrapper applies to the files assigned to that app (owner ruling 2026-11-10).

`root` and `app` are exclusive. Each of these is a located `invalid_rules` error: any other key,
any invalid value, both `root` and `app`, the same `callee` twice for one scope, and more than 64
entries. An `app` that names no app, or a name that several apps share, is a located
`reference` or `ambiguity` error when the apps are resolved, like a layer's `app`. Wrappers only shape interface evidence. They
never change a rule verdict, a count or an app. The reading rules are in
[architecture-view.md](architecture-view.md#apps-and-http-interfaces-data-and-extraction-784-788).

## External package policy

A layer needs a nonempty `paths` or `external` collection; either selects that
layer. External selectors are closed objects with all three fields required:

```toml
[external_policy]
deny = [{ language = "rust", package = "legacy-db", stdlib = false }]

[layers.adapters]
order = 2
external = [{ language = "rust", package = "serde_json", stdlib = false }]

[layers.foundation]
shared = true
external = [{ language = "rust", package = "std", stdlib = true }]
```

`language` is `rust`, `java`, `python`, `javascript` or `typescript`. `package` is a literal
canonical identity, not a glob, import alias or version; `stdlib` is required and
distinguishes standard-library targets from declared dependencies of the same
spelling. Canonical dependency names are Cargo's upstream declared package name
(preserving hyphens/underscores), Maven `groupId:artifactId`, PEP 503-normalized
Python distribution name, and exact npm name or `@scope/name`. Import bindings
and version constraints are evidence, not identity. Unknown fields and invalid
identifiers are located `invalid_rules` errors. Repeats within a layer deduplicate;
two different layers selecting one external identity are a located ambiguity.
These arrays share existing configuration collection/work bounds.

`[external_policy]` accepts only optional `deny`. An absent or empty list means no
external bans. Every positively observed matching external edge emits a reserved
`$external` instance with `status:"deviates"`, even if its source has no layer.
Nonmatches emit no `$external` row; absent edges do not prove absence. Aggregate
by source module, external identity and stored production/test role with the
existing exact counts and bounded samples. Test-only instances never fail the
production CLI check. Unproved roots stay unknown, never guessed to match a ban.

`$layers` still checks direction for assigned external destinations. A ban and a
layer violation can both be reported: two policy instances, not two references.
`$external` has no exceptions. Remove or narrow a ban through a reviewed policy
edit. Neither declaration exceptions nor module-to-module exceptions accept
external package destinations.

## Roles and constraints

```toml
[roles.resource]
select = { paths = ["src/web/**"], name = "*Resource", language = ["java"] }

[roles.service]
select = { name = "*Service" }

[roles.repository]
select = { extends = "JpaRepository" }

[[rules]]
id = "layered-write-path"
for_each = "resource"
require = [{ calls = "service" }]
forbid = [{ calls = "repository" }]
```

The closed top-level keys are `version`, `layer_policy`, `external_policy`, `layers`,
`roles`, `rules` and `exceptions`. `version` is required and exactly `1`; optional collections
start empty. (#783: an ordered layer gains `app` and a shared layer gains `apps`; see
[Amendment (#783)](#amendment-783-apps-and-per-app-layers). #794: `apps` is an eighth
top-level key, a map of authored `[apps.<name>]` tables; see
[Amendment (#794)](#amendment-794-authored-apps).) A selector has at least one of `paths`, `name`, `language`, `extends`
and `implements`. Keys combine with AND; entries within `paths` or `language`
combine with OR. Languages are `java`, `rust`, `python`, `javascript`, `typescript`. Name patterns
match the measured nullable name; null never receives an invented name.

`extends` and `implements` **selectors** match direct header syntax spelling where
the grammar distinguishes it, not resolved ancestry. An unavailable or unsupported
distinction retains uncertain selection as unknown rather than silently excluding
the declaration. A header spelling match does not create a type relationship.

Each rule names an existing source role in `for_each`. Constraints are closed
single-key objects, exactly `calls`, `extends` or `implements`, naming an existing
target role. Arrays are conjunctive. Unknown keys/references, duplicate rule IDs,
invalid identities and malformed input are located errors, never ignored fields.
No annotations, recursive Boolean DSL or transitive-path syntax is supported here.

**Constraints use only fresh, selected semantic bindings (#523).** Header bases,
same-name candidates, import spelling and legacy class candidates never become
positive bindings. A document whose language has no selected producer (#483), or
no usable coverage tuple, has no bindings: its `calls` constraints return `unknown`
with `call_binding_unavailable` and its `extends`/`implements` constraints return
`unknown` with `type_relationship_unavailable`. They retain actual measured
call/header evidence.

With a selected producer, a measured call of the declaration counts when its fresh
binding is resolved, direct (or a constructor) and its internal target is a
declaration of this snapshot that the target role selects; the binding is added as
`semanticBinding` evidence. A type relationship counts when it is a fresh
`extends`/`implements` relationship of the declaration whose internal target the
role selects (`typeRelationship` evidence). Then:

- `require` **conforms** with one counted binding, and otherwise stays `unknown`:
  absence of a required edge never establishes a violation (`required_call_absent`,
  `type_relationship_absent`, or `…_incomplete` while bindings are missing).
- `forbid` **deviates** with one counted binding. A forbidden call **conforms** only
  when every measured call of the declaration has a fresh binding that is external
  or direct to a declaration outside the role, in a document the producer covers
  completely; otherwise it stays `unknown` with `call_binding_incomplete`. Forbidden
  type relationships never conform by absence.

Unbound, stale, ambiguous, unresolved and dispatched (virtual, interface, dynamic or
unknown dispatch) calls, and targets a role selects only by `extends`/`implements`
selector, are uncertain. A rule instance whose declaration is certainly selected
deviates when any constraint deviates and conforms when every constraint conforms;
otherwise, including whenever its selection is uncertain, it is unknown. A forbidden
type relationship without a counted one stays unknown with
`type_relationship_absence_unproven`.

An instance is `conforms`, `deviates` or `unknown`. A proven deviation wins over
uncertainty, but uncertainty remains in the details. Instances deduplicate by
`(rule, sourceSetId, language, path, sid)`, even if roles/selectors overlap. Layer
checking uses the reserved rule `$layers` and one instance per observed native
source to native-module or external-package destination pair. Absent or empty `layers` disables `$layers` checks;
unassigned native module and observed external identities remain informational. Ordinary declaration rules
still run and the references view remains available.

## Path patterns

Patterns are case-sensitive, checkout-relative, slash-separated and anchored.
`*` and `?` do not cross `/`; `**` spans directories. `src/store/**` selects
descendants, **not** `src/store.rs`; name the latter separately. Absolute paths,
backslashes, NUL, dot/parent components and unsupported glob syntax are rejected.
These are not ignore-file patterns: no negation or ambient ignore semantics.
Path filters on a request select **source** instances/sites only. Destination
mapping and target-role selection still use the whole selected indexed snapshot.

## What a reference means

The shared view-time projection parses **captured source**, using stored native
declarations and their document identities. A module is an indexed file identified
by its full `DocumentKey` and native file-module SID. A projected site is identified
by document version, exact byte range and relation kind; it is not a fabricated
`occ:v2` call or semantic relationship. Native links retain their real declaration
or occurrence IDs and proof; derived structural mapping is labelled as such.

Supported mapping is deliberately narrow:

- **Rust:** proved conventional module roots and lexical `crate`/`self`/`super`
  paths, explicit aliases/reexports and wildcard name exposure follow at most four
  import/reexport edges. Native final-owner mapping and real intermediate-module
  proof stay distinct. A wildcard can provide positive native mappings while
  residual unknowns remain; no native or external member inventory is invented.
  Captured Cargo declarations and reviewed `std/core/alloc` roots prove package
  targets, never semantic calls. Custom roots/`#[path]`, macro/cfg uncertainty,
  ambiguous ownership and depth beyond four remain unknown.
- **JavaScript and TypeScript:** the two languages share one resolver. Static
  relative imports/reexports/side-effect imports (also `import type`,
  `export type` and `import x = require("m")`) try the extensionless candidates
  `.ts`, `.tsx`, `.d.ts`, `.js`, `.mjs`, `.cjs` and `index.*` of a directory,
  including for dotted names such as `./x.config`. `./x.js` also names `x.ts`,
  `x.tsx` and `x.d.ts`; `./x.mjs` and `./x.cjs` name `.mts`/`.d.mts` and
  `.cts`/`.d.cts`; `./x.jsx` names `x.tsx`; an explicit TypeScript extension is
  literal. A trailing `/` selects only a directory's `index` file, never a
  sibling file. Several hits are partitioned by precedence: implementations in
  the importer's language, then implementations in the other language, then
  declaration-only `.d.ts` files; one hit resolves and several in the winning
  class are `module_path_ambiguous`. A JavaScript importer may therefore resolve
  to a TypeScript target and the reverse. The root `tsconfig.json` (else
  `jsconfig.json`; JSONC, `extends` is not followed) adds `paths` (exact key,
  else the longest `prefix*suffix` pattern, targets in order, targets leaving
  the checkout skipped), `baseUrl` and `rootDirs` for relative misses.
  Workspace packages (#773, owner-ratified 2026-11-10) come next for a bare
  specifier that `paths` and `baseUrl` leave unmatched. The member directories are
  the literal paths and simple globs (`*` within a segment, `**` across segments;
  a `!` pattern excludes) that the captured root `package.json` names in
  `workspaces` (an array of strings, or `{packages: [...]}`) and the captured root
  `pnpm-workspace.yaml` names in `packages`. That file is read with a bounded YAML
  subset: top-level `key:` mappings, a block or one-line flow sequence of plain or
  quoted strings, and `#` comments. Absolute or traversing patterns name nothing;
  more than 256 patterns, an unsupported shape, tabs, anchors, tags or a second
  document make the workspace unreadable. Each retained nested `package.json`
  (outside `node_modules`) in a member directory maps its `name` (including
  `@scope/name`) to that directory. A bare `name` resolves to the member's entry:
  `exports` when it is a single `./` string, else `module`, else `main`, else the
  directory's `index` file, by the extension rules above. `name/sub/path` resolves
  to that member-relative path, unless a single-string `exports` closes subpaths.
  Any other `exports` keeps the member `javascript_workspace_exports_unsupported`;
  two members with one name are `javascript_workspace_package_ambiguous`; an entry
  or subpath leaving the member is `javascript_workspace_entry_unsupported`; one
  naming no indexed script is `javascript_workspace_entry_not_indexed`. A
  workspace config, or a member `package.json`, that cannot be read makes bare
  specifiers that resolve nowhere else `javascript_workspace_unreadable`. A member
  name never falls back to an npm or Node label; an unreadable workspace still
  may. No `node_modules`, package manager or execution is involved, and
  scip-typescript's `--yarn-workspaces` and `--pnpm-workspaces` producer flags
  remain out of scope. A bare specifier falls back to a package only when no
  local module matched; an ambiguous local match stays `module_path_ambiguous`. A
  config that cannot be read leaves relative resolution unchanged and reports
  bare specifiers that match no package as `tsconfig_unreadable`. `export = x`
  sets an unknown name set; `.cts` ends export following. `export * from`
  forwards proved named exports, never `default`. Explicit exported names shadow star names; distinct surviving declarations
  collide, while routes to the same native declaration deduplicate. Import-binding
  reexports retain the original identity; a proved local variable alias keeps its
  own native binding identity and owner, not its initializer's. Proved forwarding
  follows at most four edges. Name expansion into `.cjs`, cycles, unsupported
  namespace/CommonJS/dynamic exports, unproved bindings and over-depth forwarding
  retain unknowns; ordinary static module edges remain available. External stars
  may label a package but provide no names and leave residual uncertainty;
  an explicit proved export settles only its matching name. Supported static
  literals use the resolver's documented decoding subset, not runtime execution.
  Bare imports can label captured npm dependencies or the exact reviewed Node
  catalog below, after `paths`/`baseUrl` fall through; the identity carries the
  importing file's language (`javascript` or `typescript`). Relative misses never
  fall back to builtin or npm labels.
- **Python:** captured `m.py` or `m/__init__.py` layouts at the checkout root or
  a declared source root require unique module ownership and proved `__init__.py`
  package prefixes. Declared source roots (#771, owner-ratified 2026-11-07) are the literal relative
  directories the captured root `pyproject.toml` names in `tool.uv.workspace.members`,
  `tool.setuptools.package-dir` key `""` and `tool.setuptools.packages.find.where`,
  plus the parent directory of each `tool.hatch.build.targets.wheel.packages` entry.
  Glob, absolute or traversing entries, malformed TOML and nested manifests declare
  nothing. A top-level module name claimed under two roots is ambiguous; deeper
  segments continue only under the root that proved their prefix, and a file
  proved under two roots has no package membership. Relative
  imports follow captured package membership; absolute imports use only those
  proved root conventions. No suffix search, namespace-package or `sys.path`
  discovery is performed. A single unconditional literal `__all__` list of
  supported string literals selects exactly its proved names, including private
  spellings. Without `__all__`, proved public top-level names are exposed.
  Dynamic, mutated or conditional `__all__` cannot fall back to that public-name
  inventory. Proved simple import aliases and forwarding retain original native
  declaration identities and follow at most four module/import edges; wildcard
  edges reach final declaration-owner files and keep real intermediate proof
  separately. Distinct competing names, cycles, a fifth edge, missing selected
  names, dynamic exports, unsupported string decoding and unproved local layout
  remain unknown. Independently proved names can map while residual uncertainty
  remains visible; partial decoding or inventory never claims complete exports.
  Explicit/local bindings settle matching names, but an external star supplies no
  names and keeps wildcard-only bindings uncertain. Reviewed stdlib labels are
  package-only; third-party distributions remain unknown without authoritative
  captured import-name mapping. This is not Python execution or runtime binding.
- **Java:** captured declared packages and public top-level class/interface/enum/record
  declarations prove ordinary single-type and on-demand imports and qualified-type
  paths. On-demand imports enumerate only the exact package, not subpackages.
  Explicit/local type bindings shadow on-demand names; distinct competing native
  types remain unknown, while independent names can still map. Static imports,
  nested/member types and unproved bindings stay unknown. The reviewed `java`
  root supplies a package-family label. `javax` requires an exact containing
  package in the [pinned OpenJDK 25 public-export catalog](java-standard-library-catalog.md).
  Both labels remain package families, not selected-JDK member availability.
  Maven/Gradle coordinates alone do not prove third-party import prefixes.

Comments and string decoys are ignored; actual import module-specifier strings
are syntax. Unsupported coverage and unresolved counts stay visible. Paths hidden
behind unproved aliases are not claimed covered. Rust/Java external-name
uncertainty does not veto an independently proved in-repo wildcard name; two
native candidates, macro/cfg uncertainty or unproved explicit bindings still do.
Python/JavaScript/TypeScript retain wildcard-only names as unknown under external-name
uncertainty unless an explicit/local binding settles them. This is **syntactic,
structurally mapped reference evidence**, not a compiler dependency graph,
resolved library membership or call binding. A positive mapped upward reference can deviate under this syntax contract;
no observed edges cannot prove no semantic dependencies.

Counts aggregate `(source module, destination module, source role)`, with bounded
deterministic sample sites and exact omitted-sample counts. Sampling does not
truncate counts or verdicts. Parse failures, cancellation and work bounds fail
atomically, not as a healthy-looking partial projection.

Python coverage follow-ups from [PR361 review](https://github.com/SquashMerge/trellis/pull/361#issuecomment-6431615903):
`from . import x` and `from pkg import sub` can map to the package initializer
instead of the child module, so child-module edges can be missed. Dynamic writes
through `setattr(sys.modules[__name__], ...)`, `vars()[...] = ...` and
`__dict__.update(...)` are not fully tracked; this projection does not claim
complete detection of dynamic namespace mutation. Tuple `__all__` remains
unknown under the ratified literal-list-only rule.

## Production and test evidence

A site's role comes from its innermost enclosing **stored** native declaration,
falling back to the stored file-module owner. Ambiguous ownership is unknown.
There is no read-time path or cfg classifier.
[Decision 0011](semantic-evidence/decisions/0011-test-production-declaration-role.md)
governs the role: Rust `cfg(test)` and inherited positive test markers are test;
`cfg(feature = "test-support")` is **production**, not a third role. A filename
such as `src/testing.rs` alone does not override the stored role.

Test references/instances are reported separately and never count as violations. Test-only policy comparisons retain their underlying
status and a `test_only` reason, including hypothetical deviations; only the
production counts determine failure.
A mixed file can therefore contribute production and test references separately.
Older baseline wording that classified feature-only test-support seams as test
must not override Decision 0011.

## Reviewed exceptions, not count baselines

An exception names an existing `rule`, an exact `target` and a nonblank `reason`.
For `$layers`, both `target` and required `destination` are **exactly**
`{language,path}` and identify a native file-module pair:

```toml
[[exceptions]]
rule = "$layers"
target = { language = "rust", path = "src/store.rs" }
destination = { language = "rust", path = "src/navigation.rs" }
reason = "Concrete reviewed justification for this observed pair"
```

Both fields are required at each endpoint. Language is `rust`, `java`, `python`,
`javascript` or `typescript`. Paths are nonempty, literal, case-sensitive repository-relative
paths, not globs, basenames or suffix matches. Wildcard markers `*` and `?` are
invalid, as are absolute paths, backslashes, NUL, colons, empty components, `.` and
`..` components. Existing path-length bounds apply. There is no case normalization
or layer-specificity matching. Extra endpoint fields, including `source_set` and
`sid`, are invalid for `$layers`; the old full-identity layer form is not accepted.
No new version, syntax tag or compatibility mode is needed.

Portable matching requires exactly one source-set ID in the **full admitted
checkout snapshot**, including document and retained-manifest identities, before
request-source filters. Multiple languages in one source set are valid. A second
source set is a located `invalid_rules` / `ambiguity` error even if its paths,
languages, roles or edges are unrelated to the exception or hidden by a filter.
The error aborts evaluation without partial instances; filters cannot choose the
scope. Within that unique scope, both native module endpoints must match their
respective literal language/path pairs. An edit that changes a module's native SID
but keeps its language/path is covered. A rename or move is not covered until the
exception is reviewed and updated. An absent endpoint simply matches no pair.

A named declaration-rule exception retains its **exactly four-field** target
`{source_set,language,path,sid}` and forbids `destination`:

```toml
[[exceptions]]
rule = "resource-service"
target = { source_set = "ACTUAL_SOURCE_SET", language = "java", path = "src/Resource.java", sid = "sid:v1:00000000000000000000000000000000" }
reason = "Concrete reviewed justification for this declaration"
```

The named rule must exist. Copy the actual emitted source-set ID and native
declaration SID, not a name or presentation label. A changed declaration SID or
source set does not match. Portable targets are invalid for declaration rules. Duplicate rule/endpoint tuples, blank reasons and other
closed-schema violations are located `invalid_rules` errors, not partial results.
Module exceptions never accept package destinations, and `$external` has no
exceptions; narrow a ban through a reviewed policy edit.

An exception keeps the underlying status, constraints, exact reference counts and
evidence, sets `excepted=true` and records its original reason. It is excluded from
deviation totals. An approved module pair allows additional sites on that same
pair, **not** a new source, destination, language or path. Production and test rows
remain separate; the role-neutral pair can decorate both, but test-only rows never
fail production checks. A null-destination unknown is not a module pair. A config
entry with no observed pair creates no instance and proves no absence.

Portability changes only exception TOML and matching. JSON targets and module
destinations retain their full native identities, including `sourceSetId` and actual SIDs, with
unchanged evidence/provenance closure. Explain requests still use the actual full
native identities from evaluated instances, never `{language,path}` in their place.
There is no scalar-count ratchet, separate mining engine, historical-count import
or automatic baseline write. A proposal may suggest exceptions only for current
positively witnessed production deviations for human review; historical #163
counts are not current facts.

## One code snapshot, separately identified configuration

References, checks, explanations, CLI and prompt use one daemon-owned admitted
snapshot with the complete `{indexGeneration,indexRevision}` pin and existing
checkout/root/finish fences. A stale pin fails; no moving-head joins or stale-pin
fallback. Projection is not persisted, writes no `index.db`, and changes neither
native producer output nor Stage 3 extraction.

The one current-file exception is root `.trellis/rules.toml`, read once per
check/explain/prompt as bounded configuration. References alone do not read it.
Its descriptor is `{path:".trellis/rules.toml",source:"checkout-config",contentHash}`.
**The code pin does not identify these rules bytes.** Editing only rules can change
the digest and outcome at an unchanged code pin. Code evidence never falls back
to live source. Missing config permits a proposal from scratch; check/explain fail
`rules_not_found`, whose MCP message names the `trellis_propose_rules` prompt (#769).
Malformed config fails all three atomically with a located error.

`trellis_rules_explain` filters the same evaluated instances by exact rule/target
and, for `$layers` or `$external`, the actual native/external destination identity; it does not interpret a rule a second way.
`trellis rules check --format json` uses that same daemon evaluator. Unexcepted
production deviations fail the command; unknown-only evaluation succeeds.
Parser and operational failures retain ordinary nonzero error exits, not an
incorrectly successful evaluation. SARIF is deferred.

## CLI JSON

```sh
trellis references --workspace /absolute/checkout --path 'src/web/**' --limit 5
trellis rules check --format json --workspace /absolute/checkout --path 'src/web/**'
```

Both are read-only daemon operations; JSON is the references default and `json`
is the only check format. Repeat `--path` for source globs. Omit it to check all
sources. Use `--pin GENERATION:REVISION` for a complete generation UUID and numeric
revision, never a bare revision. MCP uses the object pair instead. CLI references
defaults to 5 sample sites per edge; MCP references and the prompt use the same default. Both accept
1–50; neither sample limit changes exact site counts or rule results.

## Work bounds and located failures

The fixed configuration read is limited to 2 MiB, with at most 1,024 entries per
named collection and 8,192 aggregate glob patterns. The locked TOML parser keeps
its default recursion guard of 80; unbounded parsing is not enabled. Structural
accounting is iterative. Projection admits at most 2 MiB per captured source or
retained manifest and 256 MiB total admitted captured source and present
retained-manifest bytes, before source filters. The 250,000-site bound applies both
to each raw source extraction and to deduplicated selected sites across the request.
Reference samples accept at most 50 sites per edge. Snapshot admission also caps the
inventory at 100,000 documents and 512 MiB of aggregate stored input. These bounds
refuse the entire result rather than silently dropping evidence.

Syntax walks are bounded per document, to 128 levels and 1,000,000 visits, but one
pathological file doesn't refuse the result (#554), and neither does a file with a syntax
error (owner ruling 2026-10-31, #563). Walks skip what lies past the bounds and what lies
inside a parser error, and `partialFiles` names each such document in the snapshot, whatever
the source filter, because any document can feed resolution:

```text
PartialFile = {sourceSetId,language,path,
               reason:"syntax_error"|"depth_limit"|"visit_limit"|"evidence_budget"|
                      "oversized"|"not_utf8",
               range:Range,subtrees:UInt}
```

A file has one row: `oversized` and `not_utf8` win over `evidence_budget`, `evidence_budget`
over `syntax_error`, and `syntax_error` over the bounds.
`range` is the first skipped subtree or parser error, and `subtrees` counts subtrees skipped
over the depth bound, or the file's parser errors. References and declarations inside them
are missing, so absence there proves nothing, and a partial file's names stay unknown
wherever a proof would need the skipped syntax: a skipped import or declaration never proves
a competing binding absent. The field is empty when every document was read whole, and a
`limitations` entry appears when it isn't. A partial file alone doesn't change the exit
status.

A parser error can hide or misplace anything in its file, so a broken file proves less than
an over-deep one:

- In Rust, every scope of the file is opaque, and an explicit name there is unknown too. A
  file below it that its text names, but its tree doesn't declare, could have it as a second
  owner, so that file and everything below it have no proved crate.
- In JavaScript, TypeScript and Python, the file counts as dynamic: its exports are unknown.
- In Java, no type root or explicit import of the file is proved, and neither is any type of
  its package, nor any name another file's type root could lose to a type hidden there. A
  broken package declaration, or a broken file with none that still spells `package`, leaves
  the file's package unknown, which does the same for every package of its source set. That includes standard library fallbacks.
  A misspelled declaration inside the body of a broken file's first type can't be told from
  ordinary recovered text, so that file stays in the unnamed package.
- A selector's `extends` or `implements` on a type in the file is unknown, not false.

Sites that survive in the recovered tree keep the resolution their language gives them: an
intact `import` of a broken JavaScript, TypeScript or Python file still maps its literal path.

`evidence_budget` names a document published with no evidence but its file module, because
its facts exceed a [per-file budget](daemon-v1.md#current-limits) (#564). `range` is the whole file and
`subtrees` is 0. Its source is still read for what others resolve through it, but it has no
sites, scores, or declarations for rules to select, so nothing resolves to a declaration
inside it.

`oversized` and `not_utf8` name a source that wasn't read, because it's larger than
`--max-file-bytes` or isn't UTF-8 (#576). Like `evidence_budget`, it has only its file module,
`range` is the whole stored placeholder and `subtrees` is 0. Unlike it, nothing of its syntax is
known, so it proves less than a file with a syntax error: every rule above for a broken file
applies, a Rust file below it has no proved crate whatever its name, and a Java file's package
is unknown, which leaves every package of its source set unknown.

Reference work has one request-wide allowance of
`min(100_000_000, 5_000_000 + 20_000*N + 512*K)` units. Rules use a separate
allowance of `min(50_000_000, 5_000_000 + 10_000*N + 256*K)` units, shared by
layer assignment, exact glob comparisons, declaration evaluation and evidence.
`N` counts all admitted source documents, including empty documents and
unselected languages. `K` is the ceiling KiB of the aggregate captured UTF-8
source bytes plus present retained-manifest bytes, rounded once after summing.
Absent manifests add no bytes; configuration bytes do not enlarge either
allowance. Both allowances use the whole admitted snapshot before source
filters, sample limits or rule selection, and arithmetic saturates at the
absolute ceilings. Explain only prunes an already admitted evaluation; its
package-closure recomputation is bounded by the 50,000,000-unit rules ceiling.

The growth terms are capacity policy, not measured runtime guarantees. AST visit
and site bounds stay fixed: visits already reset for each local traversal, and
measurements showed no visit or site pressure. Adding unrelated files should
not enlarge a pathological document's local traversal allowance or evidence
memory bound. The five-second MCP request deadline and cancellation checks are
unchanged and independent of work accounting.

Snapshot admission reads every selected document's rows on one connection, then
decodes them on at most eight workers. Per-file parsing is unbudgeted and runs as
one pass over all documents; after a failure no later document starts. Source
extraction/owner-call binding and rule declaration work run in bounded batches
of at most eight workers. Independent language catalog builds may run
concurrently, and the JavaScript catalog computes selected documents' write and
CommonJS hazards in parallel; Rust name resolution remains ordered. Workers
count local work against the phase's remaining allowance; one ordered owner
admits their counts and rows into the request total. A batch may perform
bounded speculative work, but workers never receive independent admitted
request budgets. Actual CPU work can reach about eleven times the allowance
(three catalog jobs beside eight JavaScript hazard workers) before failure is
reported, with the unchanged five-second deadline as backstop.
Serial and parallel plans use the same phases, charge units and output order.
Failure discards the whole result.

A rule diagnostic is exactly
`{code,kind,path,range,line,column,attribution,message}`. `code` is `invalid_rules`;
`kind` is `schema`, `selector`, `constraint`, `glob`, `reference`, `ambiguity`,
`too_large`, `too_deep` or `read`. The fixed `path` is `.trellis/rules.toml`. Byte ranges
are zero-based half-open; line and column are 1-based. `attribution` is `token`,
`file` or `boundary`: a file-level/depth or size-boundary location is explicitly
labelled, never presented as an exact offending token. Ordinary I/O/configuration
read errors also carry a located file attribution. No partial instances accompany
a configuration error.

## Public JSON payloads

All objects below are closed; all listed fields are required. `UInt`, `Range`,
`DocumentKey`, language tags and the complete code pin follow the
[MCP shared types](mcp-readonly-pilot-contract.md#shared-schema-and-limits).
`Target` is exactly `{sourceSetId,language,path,sid}`; `sid` is an actual native
file-module/declaration SID, not a display label. Sources and rule targets always
use this native shape. `Destination` is the closed union of `Target` and
`{kind:"externalPackage",sourceSetId,language,package,standardLibrary}`. The full
source-set/language/package/standard-library tuple defines external equality.
No path, SID, native declaration, DocumentKey, source metric or semantic call target
is invented for a package. A config descriptor is exactly
`{path:".trellis/rules.toml",source:"checkout-config",contentHash}`.

### References data

`trellis_references` data is exactly:

```text
{referencesVersion:"references:v2",indexGeneration,indexRevision,
 evidenceTier:"syntax"|"mixed",semanticComplete:false,
 semanticProducers:{language,producerId}[],
 modules:Module[],externalPackages:ExternalPackage[],
 production:Edge[],tests:Edge[],unresolved:Unresolved[],
 counts:{scannedDocuments:UInt,mappedSites:UInt,unresolvedSites:UInt,semanticSites:UInt},
 partialFiles:PartialFile[],limitations:string[]}
Module = {identity:Target,revisionId,contentHash,provenanceId}
ExternalPackage = {identity:ExternalIdentity,ecosystem:"cargo"|"rust"|"java"|"python"|"npm"|"node",evidence:PackageEvidence[]}
ExternalIdentity = {kind:"externalPackage",sourceSetId,language,package,standardLibrary:boolean}
PackageEvidence = ManifestEvidence | CatalogEvidence
ManifestEvidence = {kind:"manifest",sourceSetId,nativeRevisionId,dependencyHash,
                    path,contentHash,importRoot,section,key,versionConstraint:string|null}
CatalogEvidence = {kind:"standardLibraryCatalog",catalog:"rust-standard-library:v1",
                   entry:"std"|"core"|"alloc"}
                | {kind:"standardLibraryCatalog",catalog:"java-standard-library:v2",
                   entry:"java"|"javax"}
                | {kind:"standardLibraryCatalog",catalog:"python-standard-library:v2",
                   entry:<one of the pinned CPython v3.12.0 stdlib roots>}
                | {kind:"standardLibraryCatalog",catalog:"node-standard-library:v1",
                   entry:"fs"|"path"}
Edge = {source:Target,target:Destination,count:UInt,kindCounts:KindCounts,sites:Site[],
        omittedSites:UInt,evidenceTier:"syntax"|"semantic"|"mixed",semanticSites:UInt}
Unresolved = {source:Target,site:Site,reason:string}
Site = {ownerSyntaxId:string|null,role:"production"|"test"|null,
        range:Range,lines:Lines,relationKind:"reference"|"import"|"call"|"type",
        spelling:string,callId:string|null,
        caller:Caller|null,callerReason:CallerReason|null,
        owner:SiteOwner|null,target:ReferenceTarget|null,targetReason:TargetReason|null,
        mapping:"structural"|"semantic",provenanceId:string|null}
SiteOwner = {syntaxId:string,name:string|null,stableLookupKey:string|null}
ReferenceTarget = {syntaxId:string,displayKey:string,document:DocumentKey,lines:Lines}
TargetReason = "structural_only"|"unresolved"|"external"|"ambiguous"
KindCounts = {call:UInt,type:UInt,import:UInt,reference:UInt}
Caller = {stableLookupKey:string,syntaxId:string,name:string,document:DocumentKey}
CallerReason = "syntax_only"|"enclosing_callable_unknown"|"stable_lookup_unavailable"|"semantic_boundary"
```

#### Semantic bindings (#523)

Where a document's language has a selected producer (the per-language default of
#483: the sole published producer covering it), the projection also reads that
producer's **fresh** evidence for the document, from a selected `complete` or
`partial` coverage tuple: call bindings of measured calls, resolved or external
use-site semantic references, and `extends`/`implements` relationships. Rows whose
proof is not `fresh`, and internal call targets not confirmed current
(`staleTarget` not `false`), are left out, so stale evidence reverts to structural
evidence. A call is represented by its lowest-provenance binding, as in the call
graph; when that one is unusable the call has no binding. Any internal target must
be a declaration of this snapshot (semantic references carry no separate
current-target flag); its module is the declaration's file.

- A structural site takes in every binding inside its range, and a call joined on a
  whole invocation that starts with it. It is bound by the binding that ends where
  it ends (its callee or final name; a call binding wins over a name), else by such
  an invocation. A resolved internal binding maps the site to its target's module;
  an external binding resolves the site without a destination unless structural
  evidence proved a package. An ambiguous or unresolved binding keeps the
  structural result.
- A measured call with a resolved internal binding that no structural site covers
  (a method call, say) becomes a `call` site, and a resolved internal `type`
  reference likewise a `type` site; one site per range, a call first. A target in
  the site's own module adds no site. Calls whose binding is external, ambiguous,
  unresolved, stale or missing add no site: they are not counted, as without an
  import.
- A site an external binding resolved is neither mapped nor unresolved, so `sites`
  can exceed `mappedSites + unresolvedSites`.
- A site mapped or resolved by a binding has `mapping:"semantic"` and its binding's
  `provenanceId`; other sites are `structural` with a null `provenanceId`.
  `counts.semanticSites` counts semantic sites with no residual. An edge's
  `semanticSites` counts its semantic sites, and its `evidenceTier` is `syntax`,
  `semantic` or `mixed` accordingly. The payload's `evidenceTier` is `mixed` once
  any site is semantic, and `semanticProducers` lists each language's selected
  producer.

Semantic bindings do not make the view complete: `semanticComplete` stays false,
unbound calls are not counted, and bindings outside the checkout name no package.
At most 2,000,000 semantic rows are admitted per snapshot (`bounded_work`), and
semantic sites count against the same site bound as structural ones.

Each external descriptor has nonempty `evidence`. Rust derives
`ecosystem:"cargo"` for declared packages and `ecosystem:"rust"` for the reviewed
Rust standard-library catalog. Java uses `ecosystem:"java"` and the reviewed
`java-standard-library:v2` catalog, with exact entries `java` and `javax`.
The `java` root policy is unchanged. The `javax` label requires exact containing-package
membership in the pinned OpenJDK 25 public-export catalog; parent packages and
invented descendants are not implied. These entries label package families, not
member existence or selected-JDK availability. Python uses `ecosystem:"python"` and the
`python-standard-library:v2` catalog: the exact 300 roots of the pinned CPython
v3.12.0 `sys.stdlib_module_names` source list. The
[Python catalog record](python-standard-library-catalog.md) gives its tag, commit,
source hashes and coverage. Upstream defines the same list on every platform,
including unavailable platform-specific and build-disabled modules; test modules
are excluded and only main package names are listed. This labels roots, not the
checkout runtime, installation, submodules or members. It never discovers or
checks the host Python version. Unlisted roots remain unknown. Local module
precedence and ambiguity are resolved before catalog
lookup; an unproved or ambiguous local root never falls back to an external
label. Python external star imports supply package edges, not member inventories;
their unknown names keep wildcard-only bindings uncertain unless a proved
explicit import or local declaration settles that name. Lookalike roots are not
catalog matches. Manifest evidence identifies the selected source
set/native revision and retained dependency hash, captured manifest path/content
hash, actual import root, declaration section/key and nullable original version
constraint. Catalog evidence identifies a reviewed code-shipped entry, not a
detected installed runtime. No precise source range is invented. Package evidence
is capture/catalog evidence, not native measured syntax provenance; actual source
sites and native intermediate-module proof remain separate.

Captured root Cargo, package.json and pyproject.toml bytes come from the selected revision's retained dependencies
through the same admitted read and transaction as source. There are no live
manifest reads, Cargo invocations, installed-library reads or parallel caches.
Malformed repository manifests stay unproved, not empty dependencies. Malformed
stored components fail with the existing typed integrity failure. Manifest parsing
shares the existing source/stored-byte and work bounds, with cancellation before
growth and atomic typed failure on exhaustion.

The first Rust slice applies root Cargo evidence only to proved conventional root
entry/module chains. Nested/workspace members, inherited dependencies without a
member binding, custom roots and unproved ownership remain unknown. Root
bindings distinguish upstream package names from Cargo keys/renames (including
hyphen normalization). Conflicting normalized bindings and local path dependencies
are not guessed external packages. Sections, optional and target qualifiers do
not prove installation/activation or change stored site roles. In particular,
`build-dependencies` and `dev-dependencies` are retained declaration evidence,
not a claim that Cargo exposes them to a production lib/bin target. Conflicting
identities across sections stay unknown; target-applicable Cargo bindings would
require a separate contract change. `std/core/alloc` are reviewed root labels,
not member/API inventories. Java's `java`/`javax` labels share this external
identity shape; the javax package gate does not change that identity. Non-JDK javax
packages require independent captured mapping evidence or remain unknown; no
third-party Maven/Gradle prefix mapping is inferred. Python
third-party distribution mappings remain unsupported.
Maven groups and Python distribution names alone do not prove import prefixes.
No Python third-party mapping table, installed-distribution lookup or additional
mapping-metadata capture is part of this slice.


JavaScript and TypeScript use `ecosystem:"npm"` only for supported declarations in the captured
root `package.json`: `dependencies`, `devDependencies`, `optionalDependencies`
and `peerDependencies`. Constraints are literal decimal `M.m.p`, `^M.m.p` or
`~M.m.p`, without leading-zero components, whitespace, suffixes or compound ranges.
The original constraint is evidence, not an installed or resolved version.
Identical keys across sections merge their evidence; unsupported competing values
block that root. Duplicate relevant sections/keys or malformed JSON make manifest
proof unknown, never last-wins. Aliases/protocols, workspace/link/file/Git/URL
values, non-string constraints and unproved declarations cannot supply npm targets.
A root `workspaces` field blocks npm inference; the root package's own `name`
never proves a dependency/self-reference. Workspace members resolve as in-repo
modules instead (#773, owner-ratified 2026-11-10; see the JavaScript and
TypeScript resolution rules); their manifests' dependency sections supply no npm
labels. Import maps, package exports maps beyond a single string and installed
node_modules are not discovered or resolved.

`pkg/sub/path` proves only declared key `pkg`; `@scope/pkg/sub` proves only
`@scope/pkg`. Empty/traversal/query/hash/backslash/scheme forms stay unknown.
No subpath/export/member existence is asserted. Dependency section names never
change native source-site production/test roles. JavaScript constructor work is
request-local, once when selected JS sources need it, charged to the existing
work budget with cancellation. There is no new capture/table/cache/live read.

Node uses `ecosystem:"node"` and `node-standard-library:v1`, exactly
`fs`, `node:fs`, `fs/promises`, `node:fs/promises` → `fs`, and
`path`, `node:path` → `path`. These labels take precedence over npm declarations.
All other `node:` forms remain unknown. The separately reviewed 56 pinned bare
builtin roots block npm labels, including their subpaths, without adding catalog
labels. This intentionally leaves false negatives such as `http/fake` and
`path/posix`; it is not Node execution semantics. Bare `test`/`sea` are not
reserved (their builtins require `node:`). The
[Node catalog record](node-standard-library-catalog.md) gives the fixed source
pin, exact spellings, root exclusion inventory and provenance. No host runtime,
`module.builtinModules`, compiler or package-manager command is consulted.

The Rust name inventory does not register foreign function-signature bindings or
expand procedural derives. Names introduced by those forms are not a complete
export or shadowing inventory; they are an unsupported coverage gap, not proof
that no competing name exists. This projection is not compiler name resolution.

`Lines` is the existing inclusive 1-based line shape or null. Target mapping
always uses the full admitted snapshot module map internally. By the
[owner's #347 ruling](https://github.com/SquashMerge/trellis/issues/347#issuecomment-6415629907),
`modules` emits only the module/proof closure of selected sources (including
sources with no sites), native destinations of emitted production or test edges,
and real native intermediate-module proof closure. `externalPackages` contains
only descriptors for emitted external destinations, never inserted into `modules`.
Every emitted module retains its native revision, content hash and provenance.
A source filter matching no documents returns empty `modules`, `production`,
`tests`, `externalPackages` and `unresolved` arrays with zero counts; this is a complete empty
structural scope, not semantic completeness. Unrelated modules do not consume
response bytes. No cap is raised and no verdict or proof is clipped. The MCP tools
page larger selected results ([paging](mcp-readonly-pilot-contract.md#architecture-references-and-reviewed-rules-129)); the CLI and
the browser detail reads ([Architecture view](architecture-view.md#summary-and-detail-reads-448)) keep these exact shapes.
Modules are ordered by full document key. Edges sort by source and target identities within each role
array; samples take call sites first, then the other sites, each in range, kind and spelling order. Unresolved sites sort by source,
range, kind and reason and are not clipped. `count` is exact and
`omittedSites = count - sites.length`. `limit` bounds samples **per edge**, not
edge counts or verdict scope. A `callId` is present only if an actual stored
call range encloses that site. Module provenance is the real native measured
syntax proof, not semantic proof for the association. `semanticComplete:false`
is invariant, including empty results.

### Check and explain data

Both tools return exactly:

```text
{rulesVersion:"rules:v2",indexGeneration,indexRevision,
 rulesFile:ConfigDescriptor,externalPackages:ExternalPackage[],
 instances:Instance[],testOnly:Instance[],
 counts:Counts,testCounts:Counts,unassigned:Destination[],unassignedFileCount:UInt,
 partialFiles:PartialFile[],limitations:string[]}
Instance = {rule:string,target:Target,destination:Destination|null,
            sourceLayer:string|null,destinationLayer:string|null,
            status:"conforms"|"deviates"|"unknown",excepted:boolean,
            exceptionReason:string|null,constraints:ConstraintResult[],
            evidence:Evidence[],unresolved:string[],referenceCount:UInt,
            kindCounts?:KindCounts}
ConstraintResult = {mode:"require"|"forbid",kind:"calls"|"extends"|"implements",
                    targetRole:string,status:"conforms"|"deviates"|"unknown",
                    evidence:Evidence[],unresolved:string[]}
Evidence = {document:DocumentKey,revisionId:string,range:Range,
            kind:"declaration"|"call"|"structuralReference"|"semanticBinding"|"typeRelationship",
            syntaxId:string|null,callId:string|null,provenanceId:string,
            caller?:Caller|null,callerReason?:CallerReason|null,
            lines?:Lines|null,relationKind?:"reference"|"import"|"call"|"type",spelling?:string,
            owner?:SiteOwner|null,target?:ReferenceTarget|null,targetReason?:TargetReason|null}
Counts = {comparable:UInt,conforms:UInt,deviates:UInt,unknown:UInt,excepted:UInt}
```

`ConstraintResult.status` follows the [constraint rules](#roles-and-constraints):
`unknown` unless a fresh selected binding settles it. A `semanticBinding` evidence
item is the call's binding (its range is the binding's anchor, `syntaxId` the bound
target and `provenanceId` the semantic proof); a `typeRelationship` item names the
relationship's target and proof. A `$layers` instance's evidence for a semantic
site is `semanticBinding` with the site's owner `syntaxId` and the binding's
provenance; a structural site stays `structuralReference` with the source module's
native provenance. The second limitation says whether any
bindings were admitted: "Calls and type relationships have no admitted bindings and
remain unknown." appears only when no document has a selected producer.
Layer instances have no constraints; declaration instances have
null layer/destination fields and `referenceCount:0`. An unresolved reference
produces a `$layers` unknown instance with null destination, aggregated by source.
Unassigned module identities are listed even when no reference exists, using
the full snapshot module map and observed unassigned external identities. A
source filter narrows this list to the modules it selects and the external
identities they observed, and never narrows layer assignment;
`unassignedFileCount` stays the checkout-wide number of unassigned files (owner
ruling 2026-10-31, #559). Empty
or absent layers disables `$layers` instances,
not this informational list, ordinary declaration rules or references.

Counts are independent for production and tests. `comparable` includes exceptions;
excepted instances are excluded from the three status totals while their status
and evidence remain intact. Test-only layer results retain their underlying policy status, including
hypothetical `deviates`; `testCounts` counts that array coherently. These are
hypothetical policy comparisons, **not violations**, and never fail the CLI.
Only production `counts.deviates` determines evaluation failure. Structural evidence carries actual
native source-document proof, not relationship proof. These new DTOs use their
explicit native document/revision/hash/provenance links rather than the existing
find/declaration/call tools' `coverage`/`provenance` array closure; that older
closure contract is unchanged for those tools. No new snapshot or semantic proof
is invented. Explain filters both arrays
by the exact identity and recomputes counts for that filtered output. Check and
explain include the external descriptors needed by returned instances and
unassigned identities; explain preserves its informational unassigned catalog.
These pre-release `references:v2` / `rules:v2` shapes replace v1 without a
compatibility mode.

Over MCP only, both payloads add the required `page:{offset,nextOffset,total}`, and
their arrays hold one page of the
[paged sequence](mcp-readonly-pilot-contract.md#architecture-references-and-reviewed-rules-129). Each
check/explain `Instance` keeps at most `limit` (default 5) `evidence` entries and adds
`omittedEvidence:UInt`, the exact number not shown. Counts, statuses and
`referenceCount` are those of the whole evaluation. The CLI and the browser's detail
reads return the complete arrays above. The
[#725 amendment](mcp-readonly-pilot-contract.md#amendment-725-rule-evidence-bases-status-filter-and-instance-pages)
supersedes this paragraph for MCP: evidence rows refer to a per-page `semanticBases`
table by `basisRef`; check accepts a `status` filter, its `limit` counts instances
per page and each check instance keeps one evidence entry. Under the
[#769 amendment](mcp-readonly-pilot-contract.md#amendment-769-rules-tools-point-to-the-next-call),
a check instance that omits evidence carries `nextCall`, the ready-made
`trellis_rules_explain` arguments that show the rest. Under the
[#714 amendment](mcp-readonly-pilot-contract.md#amendment-714-rules-check-pages-deviations-first),
MCP check pages each instance array by status, deviations first, then unknowns, then
conformances.

### Classified reference evidence (#785)

**Status: proposed (owner-delegated, run 802).**

Reference evidence is a `structuralReference` or `semanticBinding` item of a `$layers` or
`$external` instance, including the items of an unknown `$layers` row. It carries all of
`caller`, `callerReason`, `lines`, `relationKind`, `spelling`, `owner`, `target` and
`targetReason`, copied from its site. `lines` is the site's lines. Constraint evidence
carries only `caller` and `callerReason`; declaration evidence carries neither. A `$layers`
or `$external` instance carries `kindCounts`, the edge's counts or, for an unknown row,
the counts of the sites it aggregates. Declaration instances have `referenceCount:0` and no
`kindCounts`.

- **R1, relation kind** (the first match wins):
  1. A structural `import` stays `import`, including JavaScript and TypeScript `require(...)`
     and `import(...)` call sites.
  2. Semantic-only sites keep `call` or `type`.
  3. A structural `reference` becomes `call` when it is the callee of a stored measured call,
     or when the semantic binding that maps it is a call binding. The site is that call's
     callee when its range is the whole invocation, lies within the callee spelling, or runs
     from the start of the invocation to the end of the callee (a qualified callee path). A
     site that is merely inside the call, such as an argument, is not promoted.
  4. A structural `reference` mapped by a type-role semantic reference becomes `type`.
  5. Anything else is `reference`.
- **R2, owner.** `owner` is null if and only if `ownerSyntaxId` is null. `owner.name` is null
  for an anonymous declaration or a module; `owner.stableLookupKey` is null when no key is
  recoverable, and always for a file module.
- **R3, target.** `target` is non-null only for a semantic site whose binding resolves to an
  indexed declaration in this snapshot. `targetReason` is null if and only if `target` is
  non-null; otherwise it is the first of `structural_only` (the document has no selected
  semantic evidence), `external`, `ambiguous` or `unresolved`.
- **R4, kind counts.** Every site counted in an edge's `count` adds 1 to exactly one
  `kindCounts` bucket before sampling, so the four buckets add up to `count`
  (`referenceCount` on an instance).
- **R5, sampling.** An edge's sample takes its `call` sites first, then the others, each in
  the existing order. `omittedSites` is `count` minus the sites returned.

Invariants: `owner.syntaxId` equals the site's `ownerSyntaxId`, and `syntaxId` on reference
evidence. `caller` is non-null only on `call` and `import` sites. On an `import` site it
names the function that runs the import: a JavaScript or TypeScript `require(...)` or
`import(...)` whose site is its measured call.

Limits: structural type paths stay `reference` without semantic evidence, and the browser, CLI
and MCP rules samples are 5 sites per pair.

## Proposal workflow

The no-argument MCP prompt `trellis_propose_rules` (named `trellis_propose_layers` before
[#794](#amendment-794-authored-apps)) uses the launch checkout.
Its static list is workspace-independent; get uses the same evidence admission,
deadline, cancellation and final-root lifecycle as tools. It supplies server task
instructions separately from untrusted repository data: the pinned structural
reference view, limitations and production/test split, plus current exact rules
bytes/digest or explicit absence. The reference view is a bounded summary: counts,
limitations and edge and site totals, plus one production directory-to-directory
overview within a fixed byte budget, with an exact omitted count. The instructions'
workflow pages `trellis_references` and runs `trellis_rules_check` /
`trellis_rules_explain` for detail
([MCP amendment #770](mcp-readonly-pilot-contract.md#amendment-770-a-bounded-prompt-reference-view)).

Ask the agent for integer layer orders, explicit peer/shared policy choices,
placement justifications, exact reviewed baseline exceptions and a `rules.toml`
**diff for human review**. Ambiguous placement belongs in normal chat. Trellis
never applies that diff, samples a model, invokes a provider, elicits input or
requests indexing. Any later file edit uses the agent's ordinary authorized tools.
(#783: the prompt lists the detected apps and asks for layers per app. See
[MCP Amendment (#783)](mcp-readonly-pilot-contract.md#amendment-783-per-app-layers-in-the-proposal-prompt).
#794: it lists the effective apps, authored first, and explains how to author `[apps]`
tables; see [Amendment (#794)](#amendment-794-authored-apps).)

MCP request and complete response bounds still apply, including prompt messages,
errors, summary text and LF. The larger static tool-catalog allowance does not
apply to prompt evidence. Whole-result overflow fails atomically; TOML and verdicts
are never clipped to make them fit.

This document does not certify production Claude Code interoperability or
interactive command display. Those require observations against the built Trellis
binary, not schema validation alone.
Resolved library members/APIs, framework/DI/events, automatic architecture inference, mining,
SARIF and the #121 UI remain separate work.
