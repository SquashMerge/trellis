# Trellis 0.2.0 release notes

**Version:** `0.2.0`. **Release date:** on tag. These notes cover pull requests merged
after the [0.1.2 release](release-notes-0.1.2.md) (tag `v0.1.2`) through
[PR #937](https://github.com/SquashMerge/trellis/pull/937). This is a feature release.
Trellis can now re-run an approved producer after you edit, detects the apps in a
checkout and the HTTP interfaces between them, treats TypeScript as its own language,
and runs inside the Herdr app. The MCP contract changes: `toolVersion` goes from
**10 to 30**, with two new tools and a renamed prompt. The index schema goes from 40
to **43**, so every index is recreated on first use. See
[Compatibility](#compatibility).

## Automatic producer re-runs

- **Opt-in re-runs after edits:** Trellis can now re-run a producer itself after you
  edit, switch branches or pull. It is off by default. You turn it on per producer
  with an `[auto.<producer>]` table in `.trellis/producers.toml` at the checkout root
  (`enabled`, plus `quietSecs`, `minIntervalSecs`, `batteryGate` and `loadGate`). An
  out-of-range value is rejected as `invalidConfig`, never clamped. See
  [Automatic re-runs](quickstart.md#automatic-re-runs-optional).
  ([#899](https://github.com/SquashMerge/trellis/pull/899),
  [#904](https://github.com/SquashMerge/trellis/pull/904); design in
  [#825](https://github.com/SquashMerge/trellis/pull/825))
- **Approvals:** the file alone starts nothing. Only a manual `producer run` that
  ends `imported` writes an approval, which records that run's trust profile,
  limits, executable and environment. A changed toolchain, or a garbage-collected or
  replaced checkout, needs a new manual run (`needsApproval` with a reason).
- **When a run starts:** only after the checkout has been quiet for `quietSecs`,
  with no indexing pending, no other producer running, and at least
  `minIntervalSecs` (60 s floor, 10 min default) since the last automatic start. An
  edit during a run cancels it, and it is retried later. A manual `producer run`
  takes priority over an automatic run that hasn't reached its import.
- **Gates:** a start is skipped on battery or while the 1-minute load is at or above
  the CPU count, and checked again about a minute later. An unknown reading doesn't
  hold a run back, and a gate never stops a run that has started.
- **Status:** `trellis producer status` reports an `autoRun` object per producer
  (`off`, `idle`, `scheduled`, `running`, `deferred`, `failed`, `needsApproval` or
  `invalidConfig`, with a reason, `dueSince`, `nextEarliestStart` and `lastRunId`).
  While a run is scheduled or running, evidence that would say **needs re-import**
  says **catching up**; after a failure it says needs re-import with
  `automatic re-run failed: <code>`. Run records carry `origin: manual | auto`.
- **`semanticStatus` on evidence answers:** `trellis_call_graph`,
  `trellis_outgoing_calls`, `trellis_incoming_calls` and `trellis_declaration` carry a
  top-level `semanticStatus` list naming each language of the answer that is
  `catchingUp` or `needsReimport`, with its reason. It is informative only and
  changes no evidence or freshness (`toolVersion` 28).
  ([#899](https://github.com/SquashMerge/trellis/pull/899))

## Producers

- **Plans read project settings:** `producer plan` now derives each producer's
  settings from the project's own files and lists each with its source and file
  hashes, as part of `planSha256`:
  - rust-analyzer: the Cargo features named in `required-features` of every
    workspace member's targets (never all features);
  - scip-typescript: the root `tsconfig.json`, else `jsconfig.json`, else a generated
    config, plus the projects reached through `references`;
  - scip-python (trusted): project name and version from `pyproject.toml`, else
    `setup.cfg`;
  - scip-java (trusted): Maven, Gradle or scip-java's own build tool from the root
    build files.

  `--cargo-features`, `--ts-project`, `--python-project-name`,
  `--python-project-version` and `--java-build-tool` override them. Unparseable or
  contradictory config is the typed `producer_config_unresolved`. This replaces the
  0.1 workaround for feature-gated Rust code.
  ([#822](https://github.com/SquashMerge/trellis/pull/822),
  [#827](https://github.com/SquashMerge/trellis/pull/827),
  [#883](https://github.com/SquashMerge/trellis/pull/883))
- **`producer doctor` and `producer install`:** `trellis producer doctor [--json]`
  reports each relevant producer as `ready`, `missing`, `wrongVersion`,
  `unverifiable` or `prerequisiteMissing`, with the pinned install command. It is
  read-only and needs no daemon. `trellis producer install <producer> [--yes]` runs
  exactly that pinned command after a confirmation, then re-runs doctor. Trellis still
  never installs anything on its own.
  ([#881](https://github.com/SquashMerge/trellis/pull/881))
- **TypeScript configs shared through npm:** a root `tsconfig.json` that `extends` an
  npm package now gets that package's config from the checkout's own `node_modules`
  (bounded, read-only), instead of failing with TS6053. Without it, the run falls back
  to the generated config and says so under `omitted`.
  ([#808](https://github.com/SquashMerge/trellis/pull/808))
- **Enqueue errors settle against a known request ID:** a run or pickup chooses its
  import request ID first, so an enqueue error after commit no longer strips inputs
  that an accepted import still reads, or imports the same bytes twice.
  ([#796](https://github.com/SquashMerge/trellis/pull/796))
- **Retention survives reboots:** the retained-run catalog is bound to the root's
  inode, not `st_dev`, which APFS renumbers on reboot, so semantic imports no longer
  fail after a restart. Corrupt producer state is the typed `producer_state_corrupt`,
  and pickup backs off repeated transient failures (60 s doubling to 64 min).
  ([#812](https://github.com/SquashMerge/trellis/pull/812))

## Apps and endpoints

- **App detection:** Trellis detects the apps in a checkout from build manifests:
  Cargo binary packages, `package.json` roots, Python `pyproject.toml` projects, and
  runnable Maven and Gradle modules. Architecture shows one app at a time or All apps,
  with per-app counts.
  ([#843](https://github.com/SquashMerge/trellis/pull/843),
  [#849](https://github.com/SquashMerge/trellis/pull/849),
  [#883](https://github.com/SquashMerge/trellis/pull/883))
- **Apps in `rules.toml`:** with two or more apps, ordered layers name their `app`,
  and layer order is compared only within an app. Authored `[apps.<name>]` tables with
  `roots` override detection. The MCP prompt `trellis_propose_layers` is renamed
  `trellis_propose_rules`, with no alias, and proposes per-app layers.
  ([#792](https://github.com/SquashMerge/trellis/pull/792),
  [#862](https://github.com/SquashMerge/trellis/pull/862))
- **HTTP interfaces between apps:** server routes are matched to client call sites,
  read at view time and bounded:
  - JavaScript/TypeScript: express, fastify, Koa and Next.js routes; `fetch` and
    `axios` clients ([#866](https://github.com/SquashMerge/trellis/pull/866));
  - Rust: actix-web routes; `reqwest` clients
    ([#867](https://github.com/SquashMerge/trellis/pull/867)). A `reqwest` call
    counts only on a receiver that is a client by type or constructor: a
    `Client::new()` or `Client::builder()…build()` chain, a typed parameter or `let`,
    or a `self` field of the client type. A receiver merely named `client` no longer
    counts, so a slice's `client.get(..)` is no longer a client call. Precision is
    preferred over recall: a missed call is acceptable, a false one is not
    ([#921](https://github.com/SquashMerge/trellis/pull/921));
  - Python: Flask, FastAPI and Django routes; `requests` and `httpx` clients
    ([#877](https://github.com/SquashMerge/trellis/pull/877));
  - Java: Spring MVC and JAX-RS routes; `HttpClient`, `RestTemplate` and `WebClient`
    clients ([#876](https://github.com/SquashMerge/trellis/pull/876)).

  All apps draws the matched interfaces between columns, with each column's Shared
  band on the side away from its partner
  ([#872](https://github.com/SquashMerge/trellis/pull/872)).
- **App languages leave out hidden layers:** `apps[].languages` (Architecture and
  `trellis_workspace_describe`) no longer counts modules in an authored
  `display = "hidden"` layer, such as tooling, test harnesses and fixtures. They still
  count in `fileCount`. Trellis's own rules now make `herdr-mod/` its own app,
  `trellis-herdr-mod`, with a `trellis-herdr-mod → trellis` interface.
  ([#927](https://github.com/SquashMerge/trellis/pull/927))
- **App switcher everywhere:** Treemap, Classes and the Files tree get
  Architecture's switcher (All apps, or one app), shared with Architecture.
  ([#875](https://github.com/SquashMerge/trellis/pull/875))
- **MCP:** `trellis_workspace_describe` lists the apps. An optional `app` argument
  filters references, rules check, find, search and the call graph. The new
  `trellis_endpoints` tool lists every server route with its matched client call
  sites, plus server-only and client-only rows, paged.
  ([#868](https://github.com/SquashMerge/trellis/pull/868)) `trellis_references`
  can group by app and select sources under a directory `prefix`
  ([#861](https://github.com/SquashMerge/trellis/pull/861)), and rules check reports
  per-app counts ([#843](https://github.com/SquashMerge/trellis/pull/843)).

## Architecture and Deviations

- **Deviations tree:** rule evidence is classified by owner, target and kind, and
  the Architecture tab gets a Deviations tree with deep links into Sequence.
  ([#814](https://github.com/SquashMerge/trellis/pull/814),
  [#846](https://github.com/SquashMerge/trellis/pull/846))
- **Edges open the tree:** a production `$layers` arrow, or an upward-arrow badge,
  opens the Deviations tree on that edge. The per-edge review list and its bulk
  exception actions are retired; exceptions are per file pair from a tree line.
  ([#860](https://github.com/SquashMerge/trellis/pull/860))
- **Typed `building` answers and eager builds:** `trellis_references`,
  `trellis_rules_check`, `trellis_rules_explain` and the proposal prompt answer a
  retryable `building` with progress and `retryAfterMs` while the architecture owner
  builds, instead of `deadline_exceeded`. The owner builds each new head eagerly.
  ([#821](https://github.com/SquashMerge/trellis/pull/821))
- **Architecture during semantic catch-up:** after an edit, the architecture tools
  no longer fail `bounded_work` (and scoped references `deadline_exceeded`) until
  catch-up ends. The older-basis projection now counts only what the prior head had:
  a projected binding becomes its own reference site only under the fresh site rule,
  and each projected semantic row is admitted once. On Trellis's own checkout the
  catch-up head measures 69,333 sites (bound 250,000) and about 1.0 M semantic rows
  (bound 2 M). While the one build per head runs, reads answer the typed `building`.
  No bound changed. ([#935](https://github.com/SquashMerge/trellis/pull/935))
- **Possibly-stale evidence after catch-up:** once catch-up ends, Architecture keeps
  the semantic evidence it carried forward, labelled `freshness:"possiblyStale"`,
  until a producer re-run replaces it with `fresh` rows. Before, the head went back
  to syntax-only until the next import. `stale` rows are still left out. This
  applies to the web app, the Herdr pane, `trellis_references`,
  `trellis_rules_check` and `trellis_rules_explain` (owner ruling 2026-11-12).
  ([#937](https://github.com/SquashMerge/trellis/pull/937))
- **Edge pairs on demand:** Architecture answers no longer carry `edges[].pairs`.
  Each edge carries a `key`, and the web inspector loads that edge's pairs and
  sampled sites when you open it, from `architecture/details?kind=edge&key=…`
  (500 per page, with **Show more**). On Trellis's own checkout the default answer
  drops from about 6.8 MB to about 0.86 MB.
  ([#929](https://github.com/SquashMerge/trellis/pull/929), after
  [#919](https://github.com/SquashMerge/trellis/pull/919))
- **Deviations tree labels:** a line row shows the definition's qualified name (for
  example `DaemonState::enqueue_generated_semantic_import`), never the internal
  key. Names stay on one line, cut off with an ellipsis, with the full text in a
  tooltip. ([#924](https://github.com/SquashMerge/trellis/pull/924))
- **Layer numbers and one-app coverage:** a layer's displayed number counts only the
  shown layers of its app, and a revealed hidden layer reads **Hidden · name**.
  Rules still compare the authored `order`. In a one-app view the coverage footnote
  and the About counts cover that app's layers only.
  ([#926](https://github.com/SquashMerge/trellis/pull/926))
- **Rules tools:** rules check pages deviations first
  ([#820](https://github.com/SquashMerge/trellis/pull/820)); an instance with omitted
  evidence carries the exact `trellis_rules_explain` call to make next
  ([#782](https://github.com/SquashMerge/trellis/pull/782)); and the proposal prompt
  fits real repositories through a bounded directory overview
  ([#774](https://github.com/SquashMerge/trellis/pull/774)).
  `trellis_rules_explain` lists only the unassigned ends of the instance it explains,
  not the whole checkout's catalog, so `page.total` counts that instance's items.
  `unassignedFileCount` stays checkout-wide, and `trellis_rules_check` still lists
  the full catalog. ([#928](https://github.com/SquashMerge/trellis/pull/928))

## Sequence

- **Cross-app hops:** a client call that interface evidence matches to another
  app's route hops into an «app» lane for the handler's app, labelled with the method,
  path template, confidence and "structural match, not proof". Unresolved calls stay
  boundaries. ([#885](https://github.com/SquashMerge/trellis/pull/885))
- **Callbacks:** a callable passed to a measured call (a JS/TS arrow or function, a
  Rust closure, a Java or Python lambda) is shown as a `callback` block when its body
  holds an interaction or a drawn interface hop. Every other callback stays a
  boundary, as before. ([#891](https://github.com/SquashMerge/trellis/pull/891))
- **Frames around arrows in Interactions:** a branch or `try` that encloses an
  arrow is drawn as an `alt` frame (`opt` with no alternate), with a nested frame per
  `match` arm. A same-column call whose expanded body encloses an arrow is framed
  with its callee's name, so a helper's arrow no longer appears unframed in the
  caller's flow. Frames take no row number. Presentation only, in both renderers.
  ([#916](https://github.com/SquashMerge/trellis/pull/916))
- **Lane headers:** lanes say whether they are a «type», «module» or «class», and
  long names wrap instead of being clipped.
  ([#797](https://github.com/SquashMerge/trellis/pull/797))
- **`trellis_show`:** a new MCP tool takes the user's open browser or Herdr viewer to
  a sequence, a class or a source line, checked against the call's snapshot. It
  answers `shown`, `no_viewer`, `not_found`, `stale` or `invalid`. The browser gains
  a Following/Pinned toggle. ([#824](https://github.com/SquashMerge/trellis/pull/824))

## TypeScript and references

- **TypeScript is its own language:** `.ts`, `.tsx`, `.mts`, `.cts` and `.d.ts`
  files are indexed as `typescript` through the TypeScript grammars, with native
  constructs, class diagrams and its own language rows in MCP. The native extractor
  moves to `native-v16`. ([#800](https://github.com/SquashMerge/trellis/pull/800))
- **Call ownership:** a call is owned by its innermost executable container (module,
  namespace, type, implementation, function and so on), as native measurement
  already did. ([#804](https://github.com/SquashMerge/trellis/pull/804))
- **Workspace resolution:**
  - Cargo workspace member crates resolve across crates by path dependency, and
    `pub(crate)` paths resolve within their crate;
  - JavaScript and TypeScript bare specifiers resolve to npm and pnpm workspace
    members, with typed unknowns for what can't be followed;
  - Python import resolution honours source roots declared by the root
    `pyproject.toml` ([#776](https://github.com/SquashMerge/trellis/pull/776)).

  ([#883](https://github.com/SquashMerge/trellis/pull/883))
- **References performance:** Rust scope imports are indexed by binding name, so a
  lookup no longer rescans a scope's whole import list. On Trellis's own `src/**`,
  reference work drops from 15.4 M to 9.2 M units of a 15.5 M allowance, which leaves
  headroom under the work bound. Results and order are unchanged. ([#895](https://github.com/SquashMerge/trellis/pull/895))

## Semantic catch-up and storage

- **Shared semantic payloads:** semantic evidence is stored like native document
  versions. A validated payload is written once and shared across revisions, while
  validity stays per revision. Witnessed catch-up (S) copies unchanged documents'
  payloads by witness-keyed reuse instead of rebuilding them. Index schema 42.
  ([#873](https://github.com/SquashMerge/trellis/pull/873); Decision 0021,
  [#825](https://github.com/SquashMerge/trellis/pull/825))
- **The first edit after a capture stays incremental:** the witness charges a
  pending dependent what a full rejoin charges, so a large dependent no longer forces
  the full fallback. ([#882](https://github.com/SquashMerge/trellis/pull/882))
- **Witness limits fall back correctly:** a resource limit on the witnessed path
  now takes the full rejoin, and S never copies evidence that an unavailable S
  published. Before, an edit could lose all rust-analyzer evidence.
  ([#830](https://github.com/SquashMerge/trellis/pull/830))
- **Declaration dependencies:** a symbol declared in a changed dependency makes its
  dependent pending, so a comment edit can't leave a stale declaration behind. Index
  schema 43. ([#889](https://github.com/SquashMerge/trellis/pull/889))

## MCP

- **One closed failure schema per tool:** each tool's catalog entry has one closed
  `Failure` envelope. Failures keep their exact JSON on the wire; the catalog shrinks
  by about 20 KB ([#884](https://github.com/SquashMerge/trellis/pull/884)). The
  catalog was compacted earlier, with no change to what any schema accepts
  ([#847](https://github.com/SquashMerge/trellis/pull/847)).
- **Any local checkout:** an explicit `workspace` may select any verified local Git
  checkout, not only those sharing the launch checkout's Git directory. The
  `different_repository` reason is removed.
  ([#840](https://github.com/SquashMerge/trellis/pull/840))
- **Smaller answers:** find and search accept `compact:true`, scope rows list only
  the result languages, and compact graph nodes keep their stable lookup key
  ([#767](https://github.com/SquashMerge/trellis/pull/767)). The call graph ranks
  calls to project-named declarations ahead of other unbound name-only calls
  ([#779](https://github.com/SquashMerge/trellis/pull/779)).
- **Describe and references:** describe pages the documents behind each language's
  failed and partial semantic counts, and references group by file, directory or
  package. ([#832](https://github.com/SquashMerge/trellis/pull/832)) A grouped page
  (`groupBy:"package"` or `"directory"`) lists only the external-package
  descriptors of its own rows, so the descriptors can no longer push every group off
  the first page. ([#920](https://github.com/SquashMerge/trellis/pull/920))
- **Counts instead of per-document lists:** language rows (describe `languages`,
  find and search `scopeCoverage`, and the scope rows of the declaration and call
  tools) replace `semantic.documents` with
  `documentStatus:{olderBasis,catchingUp,needsReimport}`
  ([#931](https://github.com/SquashMerge/trellis/pull/931)). `trellis_references`,
  `trellis_rules_check` and `trellis_rules_explain` replace their checkout-wide
  `semanticDocuments` with the same counts as `semanticDocumentStatus`
  ([#933](https://github.com/SquashMerge/trellis/pull/933)). During catch-up on a
  large checkout the lists alone could exceed the 64 KiB answer cap. `partial` and
  `semanticStatus` are unchanged.
- **An over-cap answer never drops the connection:** an answer over the response cap
  is a typed `response_too_large` for that one call, now in every tool's failure
  enum, and other calls on the connection continue. Before, it ended the MCP
  connection. ([#931](https://github.com/SquashMerge/trellis/pull/931))

## CLI and daemon

- **CLI reads activate the checkout:** `status`, `symbols`, `query`, `references`,
  `rules check` and `metrics` activate the checkout as MCP and `serve` do. With no
  head yet they wait up to 90 s for the first one, printing progress, instead of
  failing `index_not_ready`. ([#844](https://github.com/SquashMerge/trellis/pull/844))
- **Reads during an import's commit are typed:** while a large producer import's
  final transaction holds the index (about 25 s on Trellis's own checkout), MCP reads
  answer the retryable `rebuild_in_progress` within their deadline, instead of
  `deadline_exceeded` from every tool. Catching-up checks no longer wait on the
  index, and the browser's checkout list answers `storage_busy` for a held checkout
  at once instead of showing "Git unavailable". Reads during the producer run itself
  were already served from the previous head.
  ([#923](https://github.com/SquashMerge/trellis/pull/923))
- **The index survives reboots:** root identity is the path and inode, not
  `st_dev`. Before, every reboot on APFS recreated the index, let GC delete closed
  checkouts' indexes and failed queued requests as `root_changed`.
  ([#815](https://github.com/SquashMerge/trellis/pull/815))
- **Search reports `building`:** while the search owner builds a new head, search
  and describe report `building` with the cached vector count, never a reasonless
  `unavailable` ([#781](https://github.com/SquashMerge/trellis/pull/781)), and
  describe counts that cache once per head, not per call
  ([#823](https://github.com/SquashMerge/trellis/pull/823)).
- **Typed failures:** an oversized ignore file is a diagnostic, not a failed index
  ([#766](https://github.com/SquashMerge/trellis/pull/766)); a capture input over its
  cap is `limit_exceeded` ([#764](https://github.com/SquashMerge/trellis/pull/764));
  and `recovery_blocked` rows record a typed cause, not paths
  ([#795](https://github.com/SquashMerge/trellis/pull/795)).
- `serve` help and docs describe the watcher, not explicit refresh.
  ([#763](https://github.com/SquashMerge/trellis/pull/763))

## Web app

- **Recovers by itself after a daemon stall:** after a client deadline, a failed
  fetch, a typed transient answer or an untyped 502–504, the page refreshes status and
  the visible view itself, first after 2 s and then backing off to 60 s. **Catching
  up** clears when the head says it has, and an agent's `trellis_show` that a stall
  interrupted is shown once the daemon answers. A typed path refusal
  (`source_forbidden`, `symlink_forbidden`, `directory_forbidden`) no longer
  disconnects the page or forgets the remembered token.
  ([#918](https://github.com/SquashMerge/trellis/pull/918))
- **Dogfooding fixes:** after an agent show and Back, keyboard focus follows the
  selected tab, and the connect footer now reads "index follows file changes ·
  producer re-runs are opt-in". ([#926](https://github.com/SquashMerge/trellis/pull/926))

## Herdr app

- **Trellis in Herdr:** a Herdr app mod under `herdr-mod/` shows Architecture and
  Sequence beside the coding agents. It follows the focused agent's checkout (with a
  pin) and reaches a daemon on another Herdr machine. A new bounded `trellis api` CLI
  reads allowlisted browser routes through the daemon socket for it.
  ([#819](https://github.com/SquashMerge/trellis/pull/819))
- **Parity with the web app:** the mod shares the browser's Architecture display,
  app helpers and Deviations model. It gains Color by and Aggregation, the app
  switcher, interface cards and cross-app lines, the Deviations tree and the
  inspector. ([#887](https://github.com/SquashMerge/trellis/pull/887))
- **Architecture on a remote machine:** the pane asks for Architecture without edge
  pairs, and on a remote machine calls `trellis api --gzip`, which prints one line of
  base64 gzip of the usual output with the same exit code. On Trellis's own checkout
  the remote answer drops from 6.8 MB, over Herdr's 8 MiB exec cap, to about 164 KB.
  An exec failure that isn't a connection failure is now `exec_failed` with its
  message, not `machine_unreachable`, and a `trellis` without `--gzip` is
  `trellis_outdated`. ([#919](https://github.com/SquashMerge/trellis/pull/919))

## Development / test reliability

- **Plain `cargo test` works:** in-process tests now carry their own fixture state
  and semantic test hooks instead of process globals, so a plain multi-threaded
  `cargo test` no longer hangs or fails where nextest's process-per-test hid it.
  ([#892](https://github.com/SquashMerge/trellis/pull/892),
  [#898](https://github.com/SquashMerge/trellis/pull/898))
- **Nightly hang backstop:** the 256-case generated-history properties get a
  30-minute hang backstop, so the default timeout no longer acts as a speed limit.
  ([#890](https://github.com/SquashMerge/trellis/pull/890))
- **Individual test fixes:** architecture build-count baselines are taken before
  rules can start a build ([#871](https://github.com/SquashMerge/trellis/pull/871));
  the outgoing corrupt-boundary fixture builds Architecture on demand
  ([#879](https://github.com/SquashMerge/trellis/pull/879)); and the fixture phase
  barrier moved into the shared test module
  ([#793](https://github.com/SquashMerge/trellis/pull/793)); and the automatic re-run
  loop tests wait for the stored outcome, not queue removal
  ([#922](https://github.com/SquashMerge/trellis/pull/922)).
- **CI without apt:** the Linux browser job runs `playwright install-deps` only when
  a Chromium system library is actually missing
  ([#896](https://github.com/SquashMerge/trellis/pull/896)), and the mold and
  cargo-nextest downloads are cached and retried
  ([#831](https://github.com/SquashMerge/trellis/pull/831)).
- Dependency updates ([#833](https://github.com/SquashMerge/trellis/pull/833),
  [#834](https://github.com/SquashMerge/trellis/pull/834),
  [#835](https://github.com/SquashMerge/trellis/pull/835)).

## Compatibility

Trellis is pre-release; there is no backward compatibility.

- **MCP `toolVersion` 10 → 30.** Clients that pin the catalog must refresh it.
  - New tools: `trellis_show` and `trellis_endpoints` (13 tools, from 11).
  - The prompt `trellis_propose_layers` is now `trellis_propose_rules`, with no alias.
  - The workspace-selection reason `different_repository` is removed.
  - New output fields: `semanticStatus` on four evidence tools, apps in describe and
    the rules check, `nextCall` on rules instances, `building` answers from the
    architecture tools, and app kinds `cargo`, `npm`, `python`, `maven` and `gradle`.
  - New optional inputs: `app`, `compact`, `groupBy`, `prefix`, `depth` and
    `coverageDocuments`.
  - Each tool's failure schema is one closed envelope; failure bytes on the wire are
    unchanged.
  - 29: each language row's `semantic.documents` list is replaced by the required
    counts `documentStatus:{olderBasis,catchingUp,needsReimport}`, and
    `response_too_large` is in every tool's failure `code` enum.
  - 30: `trellis_references`, `trellis_rules_check` and `trellis_rules_explain`
    replace `semanticDocuments` with `semanticDocumentStatus`, the same counts.
  - Without a catalog change: `trellis_rules_explain` scopes `unassigned` to its
    instance, grouped references pages list only their rows' `externalPackages`, and
    the architecture tools accept `possiblyStale` call and type bindings after
    catch-up.

  Each change is an amendment in the
  [MCP contract](mcp-readonly-pilot-contract.md).
- **Daemon HTTP and CLI:** Architecture edges no longer carry `pairs` and gain a
  `key`; a new `architecture/details` `edge` kind pages an edge's pairs. `pairs=false`
  is still accepted and changes nothing, and `pairs=true` is `400 invalid_request`.
  `trellis api` gains `--gzip`. See [daemon-v1](daemon-v1.md).
- **Index schema 40 → 43, native extractor `native-v15` → `native-v16`.** Indexes are
  disposable derived state: an older index is recreated, not migrated, and GC removes
  older-schema indexes. Expect a full re-index of each checkout on first use.
- **Durable and configuration state:**
  - `.trellis/producers.toml` is new. It is optional, and without it nothing changes.
  - The producer state directory gains `approvals/<producer>.json`, and `run.json`
    gains `origin` ([local topology T09](local-topology.md#automatic-producer-re-runs-660)).
  - The retained-run catalog is rebound to the root inode in place. A catalog of
    another root is reset, since it is disposable.
  - `requests.db` keeps its schema. Queued rows that differ from the root only in
    device are adopted.
  - `.trellis/rules.toml` gains `[apps.<name>]` and a layer `app` key. With two or
    more detected apps, every ordered layer must name its app, so an existing rules
    file for a multi-app checkout can fail `invalid_rules` until you add them
    ([rules.md Amendment #783](rules.md#amendment-783-apps-and-per-app-layers)).
- **Contract amendments:** contract-v1 (call ownership, TypeScript, interface
  evidence, shared payloads, automatic re-runs), Decision 0020 (TypeScript), Decision
  0021 (shared semantic payloads), local topology T02–T06 and T09, the
  [sequence slice contract](sequence-slice-contract.md) (#907 frames) and
  [rules.md](rules.md) (#912 explain scope, #936 possibly-stale bindings).

## Upgrade notes

The usual [upgrade recovery](quickstart.md#upgrading-trellis) applies. The index is
rebuilt, and Trellis retries each producer's last successful `producer run` batch.
Retained batches from 0.1.x may be refused (for example JavaScript batches captured
before TypeScript became its own language), which leaves **needs re-import**: run the
producer again. If you opt in to automatic re-runs, run each producer once by hand
after upgrading to write its approval.

The Herdr pane now passes `--gzip` to `trellis api` on a remote machine, so update
`trellis` on each remote machine along with the mod (`node build.mjs --install` in
`herdr-mod/`).

## Known issues

- Open test-only flakes, which don't affect the shipped binary:
  [#900](https://github.com/SquashMerge/trellis/issues/900),
  [#901](https://github.com/SquashMerge/trellis/issues/901),
  [#902](https://github.com/SquashMerge/trellis/issues/902) and
  [#903](https://github.com/SquashMerge/trellis/issues/903).

## Known limitations

See [known limitations](known-limitations.md) before relying on the results. Changed
since 0.1.2:

- **Automatic re-runs** have their own [limits](known-limitations.md#automatic-re-runs):
  each run is a full producer run, manifest-only changes don't trigger one, manual
  overrides such as `--cargo-features` aren't reused, and the web app doesn't show the
  scheduled or running **catching up** label.
- **Project settings** a plan doesn't read: scip-typescript's yarn and pnpm workspace
  flags, `rust-analyzer.toml` and a Python `.venv`. See
  [known limitations](known-limitations.md#project-settings-a-producer-plan-does-not-read).
- **Interfaces are structural matches, not proof.** Route and client extraction
  covers the frameworks listed above. A call it can't read stays unresolved. A
  `reqwest` client typed through nested modules, generics or another file isn't
  recognised.
- **Reads pause while a large import commits.** The final transaction of a large
  producer import holds every reader out for tens of seconds; MCP answers the
  retryable `rebuild_in_progress`. See
  [known limitations](known-limitations.md).
  ([#923](https://github.com/SquashMerge/trellis/pull/923))

Still true from 0.1.2:

- **Performance gate:** the combined native-index and semantic-import
  [performance gate (#61)](https://github.com/SquashMerge/trellis/issues/61) remains
  open, and large repositories can take minutes to index and import. Semantic catch-up
  after an edit is not bounded.
- **Incoming callers by name:** an unbound caller, or one with a stale or `dispatch`
  binding, is still listed by name match.
  ([#730](https://github.com/SquashMerge/trellis/pull/730))
- Classes draws no proven relationship edges. Remote SSH and design handoff are not
  included. Producer imports do not imply complete coverage or runtime dispatch
  resolution.
