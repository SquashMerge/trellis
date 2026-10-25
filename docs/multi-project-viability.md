# Multi-project viability

> Historical validation/analysis. Saved views, notes and Jev/ACP providers described below were retired by the [owner ruling 2026-10-24, #305](https://github.com/SquashMerge/trellis/issues/305#issuecomment-6394175295). Their old files remain in place and ignored. Historical observations are not current feature or startup instructions.

> **Topology decision (2026-09-23).** The facts below about the current code still hold. For agents, the
> chosen topology is not the one-daemon registry recommended here but one index per checkout reached through
> a stdio `trellis mcp` server; see the [local topology](local-topology.md). A registry may still back a
> future browser project picker.

## Decision summary

**Yes, one multi-project SQLite database is viable. It is not needed to get one Trellis window with a project/space selector.** The smallest safe approach is one daemon with a small project registry, routing to the existing per-workspace stores. Keep independent project context, index state, notes and provider allowances. Add cross-project search or links only when needed.

A set of Herdr spaces, such as `project-a`, `project-b` and `trellis` (illustrative names), is a useful interaction model: switch context without closing other work. It is not evidence that Trellis needs shared storage, nor that Herdr must be integrated. This review did not inspect or control Herdr or inspect other projects.

Separate three product decisions:

1. **Navigation:** one UI can switch among projects while background jobs continue.
2. **Storage:** facts can live in separate files or project-keyed tables in one file.
3. **Relations:** a link between two projects needs explicit identity, provenance and freshness rules. Sharing a database does not discover or prove that link.

This is a static architecture review, not an implementation or benchmark. Only this report was written. No migration, indexing, repository command, provider call or test was run. The report contains architecture facts and code references, not inspected application source excerpts or credentials. File/symbol references describe the reviewed version; UI work may change line locations.

## Current implementation: the important boundaries

| Area | Observed contract and exact code reference |
| --- | --- |
| Workspace ownership | `src/main.rs`, `WorkspaceArgs::resolve`, hashes the canonical absolute root into the default state directory. `src/store.rs`, `Store::open`, binds that directory to exactly one canonical root. A different root is rejected. Git is not required (`src/indexer.rs`, `index_workspace`, uses `require_git(false)`). |
| Actual database layout | There are already **two primary databases per workspace**, not one: rebuildable `cache.db` and durable `workspace.db`. Optional Jev and ACP ledgers are separate again. `src/store.rs`: `CACHE_SCHEMA`, `CLASS_SCHEMA`, `WORKSPACE_SCHEMA`, `Store::cache`, `Store::workspace`. |
| Schema version | `src/store.rs`, `DATABASE_SCHEMA_VERSION`, is **3**. It migrates older versions and refuses future or nonempty unversioned schemas. Graph JSON remains version 1 in `src/model.rs`. The v2 statement and some “not implemented” statements in `docs/daemon-v1.md` are historical, not current limits. |
| Database identities | Files use relative `path` primary keys; graph/class objects use bare `id`; revision and class metadata are singleton rows. Durable views/annotations also use bare `id`. None has a project key. |
| Symbol identity | `src/indexer.rs`, `index_workspace`, `Extractor::symbols`, `Extractor::declarations`, `Extractor::region`, `Extractor::walk`, generate relative-path module IDs, hash/path/position-based local IDs and preserved global SCIP identities. Native adapters in `src/indexer_rust.rs`, `src/indexer_java.rs`, `src/indexer_python.rs` also use relative-path modules and syntax identities. Identical checkouts can collide, even when source hashes are included. |
| Publication | `src/store.rs`, `Store::publish`, validates the graph, prepares the class projection, takes `BEGIN IMMEDIATE`, checks expected revision, then replaces **all** cache graph/projection rows in one transaction. A separate durable `revision_clock` allocation prevents revision reuse after cache deletion. Allocation and cache publication are not one cross-file atomic commit; gaps are intentional. |
| Reads and snapshots | `Store::symbols_at`, `entity_at`, `query_view`, `classes_at` and related reads pin the current cache revision in transactions. `source_at` returns stored source, not an arbitrary disk read. Only the current graph is retained. `planning::prepare` uses repeated guarded reads and final revision validation, not one long transaction across all packet reads. |
| Durable data | `Store::put_view`, `views`, `put_annotation`, `annotations` preserve saved queries, pins, hidden IDs and notes independently of the cache. Missing nodes become orphans, not cascade-deleted notes. Durable reads and cache orphan checks span separate databases, so these are not one atomic durable-plus-cache snapshot. |
| Daemon scope | `src/http.rs`, `DaemonState`, owns one store, index configuration, browser root, explicit Rust source roots, dependency catalog, job collection, packet cache and optional providers. `/api/...` routes have no project identity today. |
| Jobs | `src/http.rs`, `start_index`, admits one workspace index job per daemon state, captures its revision baseline, publishes with compare-and-swap, and retains at most 100 in-memory jobs. `cancel_job` requests cancellation; a committed revision cannot be undone. Jobs do not resume after restart. |
| Catalog lifecycle | `DaemonState::start_dependency_index`, `publish_dependency_index`, `catalog_snapshot` manage a separate in-memory, generation- and revision-checked dependency catalog. Refresh bursts coalesce. Startup and successful workspace indexing request catalog builds. `src/main.rs`, `Command::Serve`, does not automatically index the workspace but does start this catalog work. |
| Evidence and provider scope | `src/planning.rs`, `QuestionPacket`, `packet_id`, `prepare`, has no project field. Packet hashes cover request, revision, context, source and warnings, not workspace identity. `src/http.rs`, `PacketCache`, caches at most 8 packets / 8 MiB, each at most 1 MiB. `cached_packet`, `question_answer`, `question_run` check current revision before/after provider work. |
| Provider accounting | `src/live_jev.rs`, `LiveJev::open`, `check_binding`, `reserve`, binds a private durable ledger to workspace and immutable cap, reserving before requests and retaining failed/incomplete reservations. `src/acp.rs`, `Acp::open`, `read_status`, `reserve`, does the same for a separate attempt allowance. Sharing a directory across projects is rejected. |
| Agent sessions | `runtime/acp/runner.mjs`, `OPTIONS` and `runSession`, create a fresh, nonpersistent, tools-disabled ACP session in scratch space for an evidence answer. `src/acp.rs`, `Acp::run`, allows one in-flight request per ACP instance. This is **not** a persistent project coding-agent manager. The richer tools/session model in `SPEC.md` is planned architecture. |
| Browser session | `web/app.js`, `api`, `operationGuard`, `refreshStatus`, logout handler and `showSource`, use one connection epoch and one selected workspace. The source cache key is revision + path, without a project key. Revision/workspace change clears several states; logout advances the epoch. `web/classes.js` also guards by session and revision. |

Tests read as evidence, not executed here: `tests/identity.rs` covers local-ID orphaning and failed publication; `tests/store.rs` covers store invariants; `tests/privacy.rs` covers private storage. Existing single-workspace tests are not multi-project isolation tests.

## Option comparison

| Option | Viability and benefit | Cost and limits |
| --- | --- | --- |
| **A. One project-keyed SQLite database** | Technically sound for a local application. SQL joins, centralized backup and a same-database read transaction can support consistent multi-project reads. | **High complexity.** Every key, query, foreign key, publication, migration, cache reset and API context needs review. All writes share one SQLite writer. Combining durable data and disposable cache also changes the recovery contract. Not a configuration-only change. |
| **B. Global project registry over existing database pairs** | **Recommended.** One daemon/UI, selected-project navigation and independent context, while reusing the tested store boundary. A private registry holds IDs, labels and approved root/state configuration, not graph/source duplication. | **Medium complexity.** Requires request routing, lifecycle/scheduler controls, complete UI context guards and authorization decisions. Separate DBs do not offer one atomic cross-project snapshot. Registry metadata itself is small/low-complexity. |
| **C. Attached databases or application-level federation** | Useful later for bounded cross-project search/reporting without migrating facts. Application-level fan-out can reuse each store’s guarded APIs. | **Medium additional complexity.** Results need a project/revision vector. `ATTACH` has a finite build-dependent attachment limit, requires trusted filenames and schema compatibility, and complicates pooling. It does not solve identity or authorization. Not needed for switching. |

These are relative complexity estimates, not delivery dates. No workload measurements were made. A selector alone is a small UI control; making every asynchronous operation safe across project changes is the substantive work.

### What option A would actually require

- Use workspace-scoped composite keys for files, nodes, calls, regions, classes, class relations, views and annotations. Scope singleton metadata and revision allocation too. Every foreign key must include scope, so one project's caller cannot silently bind to another project's node.
- Retain existing local symbol IDs as opaque values where possible. Wrap them in an external identity such as `(workspaceId, symbolId)`, instead of rewriting only some strings. Queries, pins, hidden IDs, candidates, callbacks, region owners, class targets and JSON payloads all need consistent handling. A prefix on SQL primary keys alone is insufficient.
- Replace wholesale deletion in `Store::publish` with project-scoped replacement. No write can erase another project's graph or catalog. Use a per-workspace revision clock, not a global revision that makes unrelated changes invalidate all projects.
- Distinguish a **single shared cache DB plus separate durable DB** from literally **one physical file for everything**. The former preserves cache disposability but still has multiple files and cross-file allocation. The latter must replace “delete cache.db” with a selective cache reset that cannot touch notes or accounting. A durable generation/revision namespace must survive resets and restore operations; never reuse an old snapshot identity.
- Make migrations copy-safe, verify row counts and references, preserve orphaned notes, and retain export/rollback paths. SQLite does not provide automatic row-level authorization; project filtering and access checks remain application responsibilities.
- Keep index parsing outside the write transaction, as today. WAL lets readers continue during publication, but **only one writer can write a database at once**. A large project publish can delay another project's publish and, if colocated, note edits. Current connections use a five-second busy timeout (`src/store.rs`, `connect`); that is not a fairness scheduler. Monitor write duration, lock waits, WAL growth, disk space and checkpoints before choosing consolidation.

Option B permits independent SQLite writers in different project files. It still needs global CPU, memory, disk and provider concurrency limits; separate files do not make unbounded parallel indexing safe. Neither option makes a live filesystem scan an atomic checkout snapshot. Captured files can represent different instants during concurrent edits. Atomic **publication** is not atomic **source capture**.

## Identity: project, workspace, branch and space

Recommended initial model:

- **Project:** stable opaque ID and editable display label, such as “mimir.” Do not use a basename, remote URL or branch name as a unique key.
- **Workspace:** stable opaque ID for one registered canonical source root and its existing state pair. Start with one workspace per project. Add multiple checkouts/worktrees under a project only when required. The database tenant key should be this workspace ID if a project can contain several checkouts.
- **Space:** initially the selected project/workspace UI context, not a new storage owner. Several UI tabs/agents may target the same workspace without creating duplicate stores.
- **Branch:** optional observed metadata, not required identity or authority. Switching branches in place currently changes the next indexed snapshot, not the workspace. Two worktrees need separate workspace IDs even if their repository origin and branch labels match. Trellis itself need not be a Git repository.

Canonical-root hashing already deduplicates path aliases but does not survive moves as a stable identity. Do not silently rebind an old store when a directory moves or a different checkout occupies its path. Provide explicit relocation verification or register a new workspace and import durable data. Current `Store::open` and provider ledger bindings intentionally refuse root changes. Nested registrations and overlapping browse roots need an explicit policy rather than name-based merging.

A snapshot reference should be `(workspaceId, revision)`; after destructive restore/reinitialization, also use a store generation or issue a new workspace ID. A multi-project result should state a vector of those references. A list of current revisions is not evidence that all roots were indexed at the same instant.

## Safe navigation and independent contexts

For one daemon, separate process-global concerns (listener, authentication, registry, bounded scheduler) from per-workspace concerns (store, index options, roots, jobs, catalog, evidence cache, provider policy). Route each request to a context captured at admission. **Never mutate a daemon-global “current project” that determines in-flight work.** Two browser tabs or agents may select different projects.

A proposed route shape is `/api/projects/{projectId}/workspaces/{workspaceId}/...`, or the shorter `/api/workspaces/{workspaceId}/...` with registry lookup. This is a proposal, not an implemented API. Return scope on responses. Unknown/removed IDs must fail closed, not fall back to the last project or accept an arbitrary filesystem root.

On switch:

1. Advance a UI context generation; invalidate stale success, error and completion callbacks.
2. Stop old polling and cancel client reads where useful. A browser abort does not imply a server job or provider request stopped.
3. Clear or partition source, search, tree, class diagram, dependency/source candidates, questions, answers, inspector actions, views, notes and provider status by workspace/revision. Keep only bounded per-project navigation preferences if desired.
4. Restore project-specific navigation and load current status without indexing or calling a model merely because the user switched.
5. Keep background jobs attached to their original workspace. Show badges/progress there and provide explicitly scoped cancellation. Pause/removal must drain or reject active work, not discard its accounting.

Current `sourceCache` in `web/app.js` is a plain Map with no explicit byte/LRU cap. Session clearing is not a multi-project resource policy. A long-lived daemon needs per-workspace **and global** cache limits, idle-context eviction and limits on simultaneously open directory handles. Evict derived catalogs/packets, not durable notes or ledgers. Catalog eviction/reload can cause local work; it must not silently index workspaces or send sources externally.

Independent persistent agents are optional later work. Give each an immutable session/workspace binding, distinct conversation state, allowed operations, approved source scope and attributed provider usage. A future tool request must carry, or derive from a server-validated capability, workspace identity and expected revision. `query_graph`, source lookup, diagram edits, saved views and cancellation must never rely on the UI's current selection. An agent needs explicit authorization to cross project boundaries; a prompt instruction alone is not isolation. Existing ACP is an evidence-only answer call, so adding persistent editing agents is a separate capability expansion.

## Isolation, budgets and source roots

**One UI is not a security boundary.** Today `src/http.rs`, `guard`, checks loopback Host/Origin and one bearer token for all API routes. It has no project ACL. A shared daemon token could intentionally grant the local owner all registered projects, but that is a broader access grant than a per-workspace daemon token. Restricted agents need scoped credentials/allowlists and separate registry-list visibility. One process and one OS user still do not provide hostile-tenant isolation.

Keep existing no-store/CSP/origin checks and private database/token permissions (`src/auth.rs`; `src/store.rs`, `secure_state_dir`, `secure_database_file`). `docs/browser-token.md` and `web/app.js` now allow opt-in token storage for the exact origin; the older daemon document's memory-only statement is outdated. A consolidated origin must not accidentally reuse a narrow old token as an all-project credential.

Keep the three source authorities separate:

- Cached workspace source: `Store::source_at`, revision guarded.
- Live directory browsing / explicitly configured source roots: `src/file_tree.rs`, `SourceDir`, pins roots and uses descriptor-relative no-follow traversal on Unix. `valid_path` rejects traversal and unsafe components. `src/rust_sources.rs`, `Root::snapshot`, provides separate hash-identified candidates, not graph facts.
- Dependency sources: `src/http.rs`, `dependency_source`, resolves an admitted catalog source reference, reads under its retained root and checks its hash. `src/dependency_rust.rs` bounds discovery; unsupported or outside-root dependencies remain blocked/unknown.

A broader browse root is not an indexed project and must not become an agent read capability. Register roots through trusted configuration or an explicit authorized registration flow. Do not infer roots from links, manifests, returned model text or arbitrary request paths. Preserve the indexer's documented limitation against hostile concurrent ancestor replacement (`src/indexer.rs` module contract); multi-project routing is not a new sandbox.

Jev caps and ACP attempt allowances remain independent and project-scoped by default. Their ledger checks bind configuration, but `QuestionPacket` itself has no workspace identity today; provider functions rely on correct daemon routing. Namespace packets and verify scope at the provider boundary as defense in depth. Never share one `Arc<Acp>` or `Arc<LiveJev>` across projects by accident. Switching, deleting a cache or restarting must not reset allowances or refund incomplete attempts.

A global aggregate spend/attempt view is useful, but **aggregation is not a shared budget**. If a shared cap is later requested, design a single transactional reservation authority that enforces global and project limits together. Summing separate ledgers before a call races under concurrency. Preserve provider-specific estimates versus actual billing caveats. Provider audit ledgers retain source-bearing request/response evidence, so deletion and backup must include them explicitly. No provider call should occur on project selection, search or preview.

## Cross-project search and real relations

Start search within the selected project. `Store::symbols_at` performs bounded literal substring matching, not FTS; its limit caps returned rows, not necessarily scan cost. A shared database does not automatically improve that query. Index project-keyed lookup columns before considering FTS and measure real workloads first.

For optional all-project search, fan out with bounded concurrency, timeout and a total result budget. Return project/workspace labels, revision, partial/unavailable status and deterministic merge ordering. Do not scan every source tree or rebuild indexes as a side effect. Packet preparation may contain full source files; cross-project previews need explicit source-scope consent and a combined size limit, not separate per-project limits that multiply without bound.

Resource guards already include 100,000 source files / 256 MiB captured source per workspace (`src/indexer.rs`, `index_workspace`), plus dependency limits such as 2,000 files, 64 MiB source and 50,000 symbols (`src/dependency_rust.rs`). These are separate bounds, not one measured process-memory ceiling. Full rebuilds materialize graphs; dependencies and serialized payloads add memory. Do not eagerly activate every registered project.

Cross-project relations should be a distinct record type with scoped endpoints, evidence kind, source revisions/hashes and status. A user-authored navigation link is not a measured call. Matching SCIP strings or package names across projects is not proof of a call, API route or runtime dependency. Current `validate_graph` expects internal targets in one graph. `src/dependency_links.rs`, `annotate`, adds conservative library hints; it is not a general cross-project resolver. Keep unknown, ambiguous, stale, removed and unauthorized targets explicit. Never guess a local path, navigate to an arbitrary URL, expand provider scope or convert an unresolved edge to internal merely to make a link work.

With option B, federation can expose an honest revision vector without claiming a global atomic snapshot. SQLite `ATTACH` can query several trusted files, but WAL does not guarantee atomic multi-file commits; do not promise cross-project publication consistency from `ATTACH`. Prefer independent read-only queries initially. A single file can support one atomic read transaction across current project rows, but those rows may still originate from independent index runs and mixed-time source scans.

## Backup, removal and a reversible path

Distinguish **unregister**, **delete rebuildable cache**, **delete durable project data**, and **delete provider audit/accounting data**. None means deleting source roots. Default removal should unregister or archive, with explicit confirmation for destructive choices. Do not silently reset an allowance by removing/re-registering a project. Retained provider accounting and source-evidence retention need a stated policy.

With option B, back up the registry, each durable workspace DB and the configured provider ledgers using SQLite backup mechanisms or with writers stopped. A collection of independent backups is not one atomic global backup. Preserve canonical-root bindings and revision clocks on restore; if snapshot identity may be reused, change its generation. `docs/daemon-v1.md` correctly warns not to unlink live SQLite databases or casually discard WAL sidecars. Cache loss should remain recoverable without losing notes.

If measurements later justify A:

1. Freeze writers for the affected projects and back up all durable state and ledgers.
2. Create a **new** versioned consolidated store beside the originals; never point several existing `Store` instances at one old state directory.
3. Import with explicit workspace keys. Preserve local symbol IDs, record IDs, orphaned references and revision high-water marks. Verify duplicates are separated, references stay in scope, and exports match. Cache data can be rebuilt later with authorization, not as an implicit migration step.
4. Validate isolation, rollback, cancellation and recovery offline. Cut over one registry entry at a time; keep originals unchanged and make only one store writable per workspace.
5. Before new writes, rollback can simply restore routing. After new durable writes, rollback requires a verified scoped export/replay or restoration that explicitly accounts for those writes. Do not claim that flipping a pointer then is lossless. Avoid live dual writes unless a separate consistency protocol is justified.

Provider ledgers can remain separate in this migration. Merging them is not required for shared graph storage and must not enlarge or reset authorization.

## Recommended phases and acceptance gates

1. **Registry + selected-project navigation — medium overall complexity.** One daemon, existing store pairs, trusted explicit roots, per-request scope, complete UI switch invalidation and bounded activation. Keep providers disabled unless configured separately per workspace. No Herdr integration required.
   - Gate: two projects with the same paths, symbol IDs, view IDs and revision number cannot cross-read/write. Two tabs can select independently. Switch during a read, index, class action or delayed provider response cannot paint the wrong project.
2. **Lifecycle and context management — medium complexity.** Fair bounded index/catalog scheduling, global/per-project resource accounting, scoped background status, safe unregister/backup. Optional bounded navigation memory per project. Persistent agent sessions remain an explicit additional feature.
   - Gate: A's cancellation/removal cannot affect B; restart does not claim old jobs resumed; failed reservations remain charged; no work starts merely from selection beyond disclosed local context activation.
3. **Cross-project search and explicit links — medium additional complexity.** Federated bounded reads and truthful revision vectors first. Cross-project inference or editing requires separate source-sharing authorization and project-aware tools.
   - Gate: partial results, unavailable projects, unknown links and stale targets are visible; unauthorized projects never enter result sets or evidence packets.
4. **Optional physical consolidation — high complexity.** Only after measurements identify a real benefit in transaction needs, backup operations or search. Use the reversible import path above; retain per-workspace identities even if files merge.

### Decisions to confirm

- Is the immediate goal switching among the four projects, or also searching/reasoning across them? Recommend switching first.
- Should “project” group multiple worktrees, or is one registered root sufficient now? Recommend root-scoped workspaces with optional grouping later.
- Should one local owner token see every project? Do any agents need narrower visibility?
- Does “independent agents” mean the existing one-shot ACP explanation, or persistent editing sessions? The latter is not current functionality.
- Must jobs continue in inactive projects? Recommend yes, under a global concurrency limit, with scoped cancellation.
- Are provider allowances per workspace, per logical project, or deliberately shared? Recommend preserving current per-workspace ledgers first.
- On removal, what durable notes and provider evidence must be retained? Who may authorize irreversible deletion?
- What measured requirement would justify one physical database? File-count preference alone does not outweigh the current cache/durable separation.

**Recommendation:** adopt B first. It delivers the desired “spaces” experience without making storage consolidation or persistent coding agents a prerequisite. Keep A possible through explicit identities and context-scoped APIs. Treat C and actual cross-project relations as later capabilities, not hidden consequences of a selector. Actual Herdr integration is optional future work and would need a separate, explicit request and interface review.
