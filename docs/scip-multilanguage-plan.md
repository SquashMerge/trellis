# SCIP beyond JavaScript: staged import and generation plan

> **Update (2026-09-23).** Semantic evidence must now also survive real-time native refresh: overlays
> keyed by document content hash, and conservative possibly-stale labels computed at read time. The
> identity recommendations below are overridden by an owner decision: every language, JavaScript
> included, uses stable syntax IDs for declarations, with SCIP symbols as separate bindings and no
> backward compatibility (see SPEC §5). See the
> [local topology](local-topology.md#semantic-evidence-after-the-syntax-tier-release).

## Decision and scope

**Yes. Implement a shared SCIP importer with language-specific syntax joins. Roll out Java first, then Rust, then Python.** Keep generation of SCIP artifacts a separate, opt-in capability. The first implementation should import supplied artifacts and run only synthetic importer tests. It does not need a compiler, language server, build tool, provider, or network.

This is a design, not an implemented or validated feature. This task changed only this document. It read repository files and a bounded set of public upstream source/docs over HTTP. It did not install tools, download executable artifacts/dependencies, run indexers or tests, execute inspected application code, or contact providers. No application pilot is authorized. The directory is not a Git checkout; no commit or PR is claimed.

SCIP is a language-agnostic **symbol/definition/reference interchange format**. It is not a universal resolver and its occurrences are not a call graph. Trellis must continue to measure explicit calls, ownership, ranges, callback boundaries and control regions with each syntax adapter. A SCIP reference joined to a measured callee can identify a **declared target**. It does not establish runtime dispatch, injection wiring, reflection, proxy behavior, callback execution, or an executed method body. Relationship/implementation data is navigation evidence, not proof of a runtime receiver.

Related contracts: [Java semantic indexing](java-semantic-indexing-plan.md), [Java/Python support](java-python-support.md), [Java/Python contract](java-python-contract.md), [Rust slice](rust-slice-contract.md), [daemon contract](daemon-v1.md), and [SPEC](../SPEC.md). The earlier Java plan recommends a direct javac helper as a possible no-download path. That helper would produce a Trellis overlay, **not SCIP**, unless a producer were separately implemented. This plan proposes SCIP import as the shared path, not a silent replacement of an agreed LSP/compiler engine. Confirm the generation choice separately.

## What the code actually does

Line references describe files inspected for this plan. They are navigation aids, not stable API identifiers. There is **no `src/scip.rs`** in the inspected tree. A future shared module with that name would be new; current SCIP code lives in `src/indexer.rs`.

| Location | Observed behavior | Consequence |
| --- | --- | --- |
| `Cargo.toml:16-20` | Existing `protobuf = "3"`, `scip = "0.10"` dependencies | Reuse the binary parser and generated types; no new language-specific parser library is needed to decode SCIP. |
| `src/main.rs:59-64,175-176`; `src/indexer.rs:39-55` | One optional artifact and flat hash manifest; CLI says no semantic indexer is executed | Keep these legacy inputs working. A future artifact-set manifest is a separate versioned contract, not automatic generation. |
| `src/indexer.rs:120-256` | Discovers `.js/.mjs/.cjs/.rs/.java/.py`; hashes source and a fixed root config/lock list | A SCIP producer supporting TypeScript or Kotlin does not add `.ts/.tsx/.kt` extraction to Trellis. Nested modules/configs and toolchain inputs need new attestation. |
| `src/indexer.rs:258-301` | Decodes `Index`, compares union of manifest/current hash keys, imports documents only if Fresh | Shared format ingestion exists. It does not validate producer identity, complete input universe, project-root relationship, document language or artifact hash. Duplicate paths overwrite earlier documents in a map. |
| `src/indexer.rs:307-355` | Rust/Java/Python extract and `continue`; only JS gets SCIP documents. Recovered JS trees get no semantic evidence | Removing this branch is not an implementation. The other adapters have no declaration/callee anchor joins. Freshness does not mean every file has a matching SCIP document. |
| `src/indexer.rs:491-529` | Matches exact JS token spans against definition bit `1` and deprecated occurrence `range`; maps UTF-8/UTF-32 explicitly and everything else to UTF-16; scopes locals with path/hash | Reuse the idea, not its implicit defaults. Unknown encoding is not UTF-16. New typed ranges, producer profiles, document-local namespaces and validation need explicit handling. |
| `src/indexer.rs:531-598,734-794` | JS declaration names may come from variable/pair owners; member calls use property token; a unique definition can replace node ID with a SCIP symbol | These are JS AST rules, not generic declaration/call mapping. Keep current JS identity behavior during the new-language rollout. |
| `src/indexer.rs:375-394` | Any indexed non-module/non-accessor candidate can become Internal; unmatched symbols ending `().` can become External | Not a generic callable classifier: classes pass the internal test, overload disambiguators need not end `().`, and missing source joins are not necessarily external. |
| `src/indexer_java.rs:7-11,74-83,98-145,189-235` | Source/hash/full-span/kind IDs; method/constructor/lambda nodes; explicit invocations; unresolved targets | Preserve IDs and measured spans. Expose adapter anchors for method names, explicit constructors and selected creation syntax; do not synthesize implicit constructors/generated methods. |
| `src/indexer_rust.rs:8-12,96-105,214-244`; `src/indexer_python.rs:7-11,68-77,170-203` | Same full-span identity pattern; syntax calls and inline callbacks, no SCIP | Rust paths/turbofish/method syntax and Python attribute/call syntax need their own tested anchors. No name-only resolution. |
| `src/model.rs:5,35-82,104-115` | Graph schema 1; Fresh/Stale/Unavailable and four resolution states; no dispatch meaning or producer coverage | Add explicit overlay/binding/status metadata before exposing new semantic claims. Keep freshness distinct from coverage and binding quality. |
| `src/store.rs:18,27-35,878-985,1454-1564` | DB schema 3, JSON payloads and atomic graph publication; query automatically traverses Internal targets | Store metadata with its revision. Declaration binding cannot safely use this traversal unchanged. Query warnings currently contain truncation, not graph diagnostics. |
| `src/behavior_java.rs:423-468`; `src/behavior_python.rs:622-641`; `src/behavior_rust.rs:283-309` | Exact caller/path/full-span joins. Java/Python participants remain source-hint boundaries; Rust recognizes Internal | Preserve joins. Update labels/evidence deliberately; do not leave a compiler-bound declaration mislabeled as unresolved or imply its body executed. |
| `src/planning.rs:112-119,184-201` | Packet identity covers context, sources and warnings | Binding meaning, coverage and dispatch caveats must reach and be hashed in same-revision packets. Diagnostics saved elsewhere are insufficient. |

### Existing tests are a baseline, not multi-language semantic validation

- `tests/indexer.rs:153-212`: synthetic JS UTF-16 occurrence join and source/config/add/delete/missing-artifact invalidation.
- `tests/indexer.rs:282-365`: external/ambiguous/non-call references, lock drift and parser recovery.
- `tests/indexer.rs:369-383`: direct symlink artifact/config rejection.
- `tests/indexer.rs:422-485`: Java/Python deliberately **never borrow** Fresh JavaScript SCIP evidence. Preserve this negative guarantee; add positive tests with explicit language/producer profiles rather than weakening it.
- `tests/indexer.rs:489-516`: root Java/Python config drift; this is not transitive build attestation.
- `tests/java_indexer.rs`, `tests/rust_indexer.rs`, `tests/python_indexer.rs`: language syntax behavior remains the baseline.
- `tests/identity.rs:12-63`: real syntax rename leaves notes orphaned; no automatic identity reconciliation.

No tests were run for this documentation-only task. Conditional local artifact tests in `tests/indexer.rs:238-268` do not establish producer compatibility when artifacts are absent.

## Import architecture and acceptance rules

### 1. Shared normalized evidence, language-owned anchors

Introduce a pure importer/validator (proposed `src/scip.rs`) after lexical extraction, before resolution counts and atomic publication. Decode once. Build bounded document, occurrence and semantic-symbol lookup tables. Avoid rescanning every occurrence and every source prefix for every token as the current JS helper does. Index normalized `(path, startByte, endByte, definition/reference)` keys once.

Each adapter emits ephemeral declaration and call anchors alongside the existing graph, without changing graph node/call spans. An anchor records document hash, supported AST kind, exact callee/name span, full measured span and owner. Name tokens and full declarations are different ranges. A SCIP occurrence must match the tested token anchor; optional enclosing ranges may cross-check a known producer convention, never replace Trellis's measured range. An enclosing range can include comments, annotations or decorators that tree-sitter's declaration range excludes. Reject unsupported joins; do not choose the nearest declaration/line or fuzzy name.

Resolve in two passes so cross-file forward references work: map definition symbols to exact existing nodes, then join references on explicit measured calls. Do not turn imports, method references, function values, getters, macros or arbitrary identifier occurrences into calls. A callable variable/reference does not prove a function body without separately supported binding evidence. No points-to fallback. Ambiguous definitions, multiple distinct targets at one token or conflicting artifacts stay ambiguous/unresolved, with no chosen first candidate. Deduplicate identical occurrences before counting candidates; multiple overload declarations/stubs are not automatically an invocation ambiguity and require producer-specific treatment.

### 2. Coordinates and schema evolution

- SCIP positions are zero-based, half-open. Trellis ranges retain UTF-8 byte offsets and one-based lines/byte columns (`src/indexer.rs:436-445`). Convert against the **same cached source bytes**, not a later filesystem read.
- Validate 3/4-element legacy ranges and typed single-/multi-line ranges; reject negative, reversed, out-of-file and split-code-point/surrogate positions. Test CRLF, supplementary Unicode, combining marks, tabs, EOF and empty ranges. A legal zero-length SCIP occurrence is not automatically a supported call anchor.
- `Document.position_encoding`, not `metadata.text_document_encoding` or producer implementation language, controls columns. Support UTF-8/16/32 explicitly. Allow unspecified encoding only in a pinned legacy producer profile with a tested convention. Unknown enum values fail closed.
- **Current upstream schema deprecates `Occurrence.range` in favor of `typed_range`, with typed values taking precedence.** The installed `scip-0.10.0/src/generated/scip.rs` contains both fields, but `src/indexer.rs:512-515` reads only `range`. Implement precedence and tests, including conflicting typed/legacy representations and typed enclosing ranges. No binding upgrade is assumed necessary for these fields; pin compatibility against the actual locked crate and selected producer.
- Java raw Unicode escapes, constructor/type tokens, annotations, Rust macro expansions and Python decorators can yield producer/source-span differences. Unsupported/generated spans remain unmapped. SCIP support does not fix tree-sitter grammar gaps.

### 3. Document, local and symbol validation

Require canonical relative slash paths, no absolute/drive/UNC paths, traversal, dot/empty components or symlink documents. Resolve only against approved cached source paths. Do not follow `metadata.project_root` to read files. Record an explicit root relocation policy for artifacts generated on another machine; normalized relative paths plus attested hashes must identify the snapshot. Validate document language against adapter and producer profile. Report unindexed/generated/skipped documents rather than fetching them. Reject duplicate conflicting document paths instead of last-wins insertion.

Preserve raw global SCIP strings as semantic identifiers. Parse the descriptor grammar or use tested symbol-kind evidence, not suffix-only matching or display text. `SymbolInformation.kind` and external symbol tables may be absent; lack of metadata is not evidence of callability. Constructors, overloaded methods, type calls and callable objects need language/profile-specific rules. A symbol outside Trellis's nodes may be an excluded source, failed join, synthetic declaration or genuine external dependency. Classify External only with sufficient evidence of an out-of-graph declaration/origin; otherwise retain unmapped boundary evidence.

SCIP `local N` is document-scoped. Scope it by artifact/input identity, canonical document path and source hash in the new importer. This prevents collisions across files or producers and stale reuse across content changes. Do not expose it as a workspace-global symbol or import cross-file local references. Global symbols duplicated across artifacts/source sets also require explicit conflict policy. Importing an index containing no usable document is not semantic coverage.

Bound bytes, decoded counts, individual symbol/path lengths, nesting, ranges, diagnostics and indexing work. Existing 256 MiB file parsing limits do not bound every decoded object or quadratic join. No hover/documentation/producer arguments should be copied wholesale to public diagnostics or providers. Report sanitized reason codes and counts.

### 4. Artifact-set manifest and freshness

Keep the legacy flat `path -> SHA-256` manifest for the existing JS route. Its hashes are a **trusted user assertion**, not evidence that the SCIP producer saw those inputs. Hash equality does not establish compiler success, classpath fidelity or resolution completeness. Never relabel this as attested multi-language semantics.

Define a versioned manifest for new imports with:

- Artifact content hash, producer name/version/digest, SCIP/schema/profile version, invocation/config digest and declared language/source-set coverage.
- Snapshot-relative source path/hash pairs, additions/removals and explicit excluded/generated inputs. Include each nested module's relevant config, not only root files.
- Toolchain/source-level/options and input-universe identity. Java: ordered classpath/module path, source sets, JDK, preview and generated-source provenance. Rust: workspace members, edition, toolchain, target, features/cfg, dependency identity, build/proc-macro policy. Python: interpreter/target version, config/extraPaths/import roots, environment/stub/package identities and producer project namespace/version.
- Source/config dependency closure or an explicit unknown/partial statement, completion/error status and trust origin (user-supplied assertion versus separately controlled generation). Metadata inside SCIP is descriptive, not authenticated provenance.

Do not execute tools to fill missing manifest fields during import. Do not scan all cached jars/packages to guess configuration. Artifact hashes bind a supplied artifact to a manifest but cannot prove generation history. Changed or missing required inputs invalidate that artifact's accepted evidence. A global manifest can conservatively invalidate its whole declared universe; independently attested source sets may have separate states. Do not mix incompatible Java main/test classpaths or Rust feature configurations into one pretend-complete graph.

Expose per-artifact/language/document requested, matched, skipped, unsupported and ambiguous counts, plus freshness and coverage separately. A Fresh JS artifact must not make Java Fresh. Fresh Java evidence may still cover only a small selected source set. Initially preserve legacy aggregate `semantic_state` meaning and add explicit multi-language status; revise aggregation only with a versioned consumer contract. On stale/missing/invalid artifacts, retain the lexical graph and reason codes, not previous targets attached to new source. Cancellation still aborts publication. Navigation reads only the cached revision and never claims to revalidate live files.

### 5. Declared binding is not runtime dispatch: release blocker

`Store::query_view` currently pushes every Internal target into its breadth-first queue (`src/store.rs:1485-1511`). A Java interface/base declaration can be a uniquely correct compile-time target while an override executes at runtime. Blindly promoting it to Internal can contaminate multi-hop question evidence with the wrong body.

Add optional binding evidence with `meaning: declaredTarget`, semantic symbol/alias, artifact/input digest, join quality and dispatch category (`static`, `special`, `virtual`, `interface`, `dynamic`, `unknown`). Names are proposed, not implemented schema. SCIP reference roles alone do not supply JVM dispatch opcodes or runtime identity. Derive a dispatch category only from supported syntax/declaration facts with tests; otherwise use unknown. A static/declaration graph remains a static graph, not an execution trace.

First ship declaration navigation separately from automatic body expansion. Until this gate exists, keep new references in a binding overlay, not traversal-enabled Internal targets. After the gate, allow Internal to mean a uniquely mapped indexed declaration, but do not enqueue virtual/interface/dynamic/unknown bodies as runtime evidence. The initial implementation may stop all new semantic body expansion; enable direct static/special traversal only in a later tested slice. Do not infer concrete implementations from SCIP `is_implementation` relationships. Apply equivalent rules to Rust trait-object/generic calls and Python dynamic dispatch.

Update Java/Python/Rust behavior labels, navigation, graph API types, and packet warnings together. Keep unresolved, ambiguous, external and declaration-only boundaries visible. Bring bounded semantic warnings into `query_view` from the same database snapshot, since persisted diagnostics are not currently forwarded there. No provider call is needed to determine or validate bindings.

## Identity and compatibility decision

**Keep existing measured graph IDs for Java/Rust/Python; store SCIP identities as aliases/evidence, not replacement primary IDs.** Their IDs include path, content hash, start/end byte and AST kind. This preserves current caller/owner/region/callback links, class/member joins, source selections and behavior call references for identical sources, whether semantics is enabled, disabled, stale or unavailable. It does not make syntax IDs stable across edits; existing orphan behavior remains honest.

JS differs today: a unique SCIP declaration can become the node ID; parser fallback uses `syntax:path:hash:start:kind`. Do not copy that substitution into the other adapters, and do not silently rewrite JS IDs during shared-code extraction. Preserve current JS semantic/syntax ID formatting and local scoping in compatibility mode. Freeze normalized JS golden graphs before refactoring. Fixes rejecting previously unsafe/malformed evidence may change those specific outputs, but must be explicit regression cases rather than a new identity policy.

Use optional serde-defaulted metadata and an explicit artifact/overlay schema version. Cache JSON storage may permit additive fields without SQL columns, but old cache reopening, graph validators, frontend serialization and future-version rejection all need tests. Graph schema is 1 and database schema is 3 today; finalize whether an additive change is safe before landing. If not, bump the affected cache/API schema and rebuild derived data explicitly. Never reset `workspace.db` to make a migration pass.

A future uniform SCIP-primary identity model is separate work: persisted scoped alias tables, unique reconciliation rules, collision handling, old-to-new semantic ID review. Saved views/pins/hidden IDs/notes are retired; their files remain ignored. No automatic rename/move/package-version reconciliation is proposed here. The SPEC identity section should distinguish semantic identity from the current measured primary IDs rather than promise a universal direct-SCIP primary key.

## Producers and generation permissions

These are compatible **producer candidates**, not verified installations or permission to run them. Source URLs below were read at mutable upstream branch heads; no release binary, pinned artifact hash, complete transitive dependency closure or local producer output was verified. Pin a release and re-check its actual emitted SCIP schema, symbol/range conventions and toolchain support before acceptance. Do not generalize from an indexer list to complete language coverage.

| Rollout | Producer and verified upstream claim | Inputs and execution boundary |
| --- | --- | --- |
| Existing JS | `scip-typescript`; README advertises TypeScript/JavaScript | Retain legacy import. Running it needs approved Node/tool/package configuration. It does not add TS extraction to Trellis. |
| **1. Java** | `scip-java` / `scip-javac`: javac plugin emits per-file SCIP shards, then aggregation creates an index. Current getting-started docs list Java 17/21/25, launcher JDK 17+, and required `jdk.compiler` exports | Import user-supplied artifacts first. Generation needs pinned plugin/aggregator/JDK and accurate source level, classpath/module path/source sets. Docs' default Gradle command is `clean compileTestJava compileTestKotlin compileTestKotlinJvm`; Maven is `--batch-mode clean verify -DskipTests`. These execute build logic, can clean outputs, resolve dependencies and run processors/generators. `-DskipTests` is not a no-execution safety boundary. |
| **2. Rust** | `rust-analyzer scip`; inspected source emits UTF-8 document position encoding and external symbol metadata | Pin rust-analyzer/toolchain/source/target/features/cfg and Cargo workspace/config inputs. Current SCIP loader sets `load_out_dirs_from_check: true` and uses a sysroot proc-macro server. Its SCIP subcommand exposes `--config-path`, not the `--disable-build-scripts`/`--disable-proc-macros` options shown for some other subcommands. Verify the selected version's effective controls; do not invent flags or call it parse-only. Cargo/build scripts/proc macros can execute code or access network. |
| **3. Python** | `scip-python`, Sourcegraph's Pyright fork. Current default-branch README says Python 3.10+, Node 16+, environment discovery via pip; `--environment` skips pip discovery | Pin the actual supported runtime, producer/Pyright version, project identity, interpreter/import roots, environment and stubs. Supply an explicit environment manifest where supported. Skipping pip discovery does not prove a sandbox or correct import paths. No `pip install`, environment activation hook, setup/build backend or application import is authorized. Dynamic/decorated/metaclass behavior remains partial. |

The Java producer's compile-and-aggregate route is different from the direct parse/analyze-only javac helper in the earlier Java plan. It intentionally executes an approved compiler plugin and may emit class outputs. If later approved for synthetic generation, run only the pinned producer plugin on Trellis-owned fixtures in a private output directory, with processors disabled (`-proc:none`), no arbitrary plugins/agents, no inherited JVM injection, no build tool and no application runtime execution. Verify that restricted producer mode actually works; do not advertise it as already tested. Keep compilation and generated artifacts outside the application tree.

Permission gates, independently requested:

1. **Importer implementation and synthetic import tests:** pure Rust parsing of locally constructed/supplied fixture artifacts; no compiler/indexer process or provider.
2. **Tool acquisition:** named pinned tool releases and dependency closure, hashes/licenses/origins, Trellis-owned cache. Not application dependency downloads.
3. **Synthetic artifact generation:** reviewed exact commands, toolchain, subprocesses, outputs, time/memory limits, cancellation and no-network policy. Producer plugins are trusted tool execution, not a promise of zero code execution.
4. **Application snapshot analysis:** separate approval of explicit cached source snapshots and input manifests. No inspected-application execution, source upload, build-system invocation, processors or generators under importer approval.
5. **Build-backed coverage:** separate permission for each build/dependency/network/processor/proc-macro/generator action. Offline flags alone do not forbid build script execution. Existing outputs require provenance, not mere presence.

Do not run `scip-java index`, `rust-analyzer scip`, `scip-python index`, wrappers, Docker, package managers, or upstream upload commands as part of this plan. No providers are needed for import or generation. Safe execution requires enforced process/filesystem/network limits; flags are not a sandbox.

## Milestones and fixture acceptance

| Milestone | Deliverable | Exit gate |
| --- | --- | --- |
| M0: contract | Finalize manifest/alias/binding/status schemas, import trust policy, producer profiles and dispatch-aware traversal design | Review identity and cached warning behavior; record generation permissions as absent. No claim of new language support. |
| M1: generic importer | Bounded pure decoder/normalizer/validator, artifact-set plumbing, source hash binding and lexical fallback | Synthetic cross-encoding/range/path/manifest tests pass; existing JS compatibility graphs and default lexical suites remain stable. No generation. |
| M2: Java import | Java anchors, cross-file definition aliases, overload/constructor rules, declared-target navigation, cached coverage | Java fixture table passes; unsafe dispatch never expands. No runtime resolution claims. |
| M3: producer interoperability | Approved pinned Java synthetic producer corpus with recorded command/tool/input/artifact hashes | Real producer output passes the same validator; negative and partial cases remain explicit. No private application run implied. |
| M4: Rust import | Rust-specific paths/method anchors, cfg/source-set provenance, trait/macro boundaries | Import fixtures pass; actual pinned rust-analyzer corpus only after separate synthetic execution approval. |
| M5: Python import | Function/attribute anchors, explicit environment profile, stub/overload/decorator rules | Import fixtures pass; actual pinned scip-python corpus only after separate synthetic execution approval. |
| M6: optional project pilot | Separately authorized read-only snapshot coverage report | Report denominators, gaps and exact input universe; do not relax joins to improve percentages. Full generation/build integration is another feature. |

Keep small, source-controlled synthetic sources and binary SCIP fixtures with a human-readable expected occurrence/alias table and provenance README. Pure tests can construct protobuf messages using the existing crate. Hand-authored artifacts test the importer, **not producer fidelity**; separately labeled pinned producer artifacts are required for interoperability. Do not commit private application artifacts, absolute paths or compiler stderr.

| Fixture/acceptance area | Required cases | Pass condition |
| --- | --- | --- |
| Java positive | Same-/cross-file static calls, imports/static imports, overloaded methods with nonempty disambiguators, generic declaration normalization, explicit constructors and `super`/`this` where supported | Exactly one validated declaration alias per supported call; targets use unchanged measured IDs. Unsupported constructor/type-only occurrences stay boundaries. |
| Java dispatch | Interface declaration with two implementations; base override; unknown receiver; method reference/lambda passed as value | Declaration is navigable; no guessed implementation and no automatic virtual/interface/unknown body traversal; reference is not a new call. |
| Java failure/coverage | Missing classpath/generated members, invalid source, recovered or ambiguous producer output, unindexed synthetic/default constructor, multi-module main/test conflict | No promoted guessed/recovered target; bounded reason and selected/skipped/error denominator remain visible. An index alone is not proof of a successful complete compile. |
| Rust positive/boundaries | Free and qualified paths, `use` aliases, UFCS/turbofish, inherent methods; generic trait calls, `dyn Trait`, macros, cfg-disabled code, generated `OUT_DIR` sources | Exact supported anchors resolve; cfg/feature universe recorded; trait dispatch and unmapped macro/generated spans remain explicit, not invented calls. |
| Python positive/boundaries | Cross-file imports/aliases, functions, attribute methods with usable evidence; stubs/overloads, decorators, properties, callable instances, monkey patching and unknown imports | Supported declared references join uniquely; no property-to-call, capitalization-to-constructor or dynamic runtime-target inference. |
| Coordinates/schema | UTF-8/16/32 and profiled unspecified encoding; astral text before calls, combining marks, CRLF, tabs, EOF, multiline, nested shared-start calls, typed-only and typed/legacy conflict | Exact byte spans and precedence; unsupported encoding, malformed ranges and split code units fail closed. IDs/full spans never widen to fit SCIP. |
| Local/ambiguity | Repeated `local 0` across two files and artifacts; duplicate identical references; two distinct symbols on one token; duplicate global definitions; missing callable metadata | Correct scoping/deduplication; no first-wins target, suffix guess or cross-file local join. |
| Document validation | Duplicate paths, wrong language/profile, traversal/absolute paths, symlinks, unindexed/generated files, relocated root, mismatched embedded text | Reject conflicting/unsafe input; never read producer root or fetch missing documents. If nonempty document text is used, validate it against snapshot bytes. |
| Stale/provenance | Edit/add/remove source; nested config/classpath order/jar/tool/target/features/stub change; artifact replaced; missing manifest or required input; JS Fresh plus absent Java | Invalidated overlay cannot leak old targets; per-language states remain honest. Flat legacy manifest is never promoted to stronger attestation. |
| Identity/cache | Semantics on/off/stale, restart, old JSON missing optional fields, future schema, revision conflict/cancel, class/member/callback/behavior joins | Same measured IDs/owners/ranges/ordinals for identical Java/Rust/Python sources; JS compatibility explicit; atomic publication; workspace state preserved. |
| Query/UI/packet | Depth-2 virtual declaration; declaration-only navigation; cached warnings after reopen; Java/Python source-hint labels; Rust Internal participant | No wrong-body traversal; binding caveats survive and affect packet hash; API reads launch no compiler or filesystem search. |
| Resource/privacy | Oversized/malformed protobuf, huge symbol/occurrence counts, duplicate-heavy input, timeout/cancel, source-bearing diagnostics, tool invocation tripwires | Bounded work/output; lexical fallback or explicit abort; no application/provider/tool execution from import and no source/path leaks. |

After implementation, run the project's native Rust test interface, including `cargo test --locked --test indexer --test java_indexer --test rust_indexer --test python_indexer --test identity --test store --test planning --test java_behavior --test rust_behavior --test python_behavior`, then affected navigation/HTTP/UI suites and the complete suite. New importer tests should not require installed language toolchains or network. Native test execution and producer execution are distinct approvals; neither was performed here.

A coverage report must state: selected/discovered files, files with usable SCIP, measured explicit calls, supported callee anchors, unique internal declarations, genuine external declarations, ambiguous/unmapped/stale/rejected calls, and dispatch-limited calls. Report against the entire selected syntax graph and separately against supported anchors. Never report only the successful joins as the denominator or claim universal/full runtime resolution.

## Primary sources and verification boundaries

Bounded HTTP reads only. Branch heads can change; these are not release pins. Upstream documentation may be stale or inconsistent. In particular, the Java getting-started page's introductory `index` note says Gradle/Maven, while its later section documents Bazel support; this plan relies only on the inspected Gradle/Maven/manual plugin claims and requires release verification for actual use.

1. SCIP schema: <https://github.com/scip-code/scip/blob/main/scip.proto> (read through <https://raw.githubusercontent.com/sourcegraph/scip/main/scip.proto>). `Document`, position encodings, symbol grammar, relationships, occurrence roles, typed-range precedence and enclosing-range semantics. The definition role distinguishes references from definitions, not calls from other references.
2. SCIP producer directory: <https://github.com/scip-code/scip/blob/main/README.md> (read through <https://raw.githubusercontent.com/sourcegraph/scip/main/README.md>). Establishes protocol purpose and producer candidates only, not each producer's support matrix.
3. Java producer: <https://github.com/scip-code/scip-java/blob/main/docs/getting-started.md>, <https://github.com/scip-code/scip-java/blob/main/docs/manual-configuration.md>, <https://github.com/scip-code/scip-java/blob/main/docs/design.md> (read equivalent `raw.githubusercontent.com/sourcegraph/scip-java/main/...` paths). Confirms compiler plugin/shards/aggregation, documented build commands and current JDK support claims. No exact release was verified or installed.
4. Rust producer implementation: <https://github.com/rust-lang/rust-analyzer/blob/master/crates/rust-analyzer/src/cli/scip.rs> and <https://github.com/rust-lang/rust-analyzer/blob/master/crates/rust-analyzer/src/cli/flags.rs>. Inspected `Scip::run`, document encoding and actual SCIP subcommand options. No local rust-analyzer availability, effective sandbox configuration or feature coverage was tested.
5. Python producer: <https://github.com/sourcegraph/scip-python/blob/scip/README.md>. Repository metadata at <https://api.github.com/repos/sourcegraph/scip-python> identified `scip` as default branch. `main/README.md` was absent; a `master` copy was also read, but claims here use the default-branch README. No Python/Node release compatibility or environment-discovery safety was executed/verified.
6. JS producer: <https://github.com/sourcegraph/scip-typescript/blob/main/README.md>. Establishes JS/TS producer scope; existing Trellis extraction remains JS/MJS/CJS only.

The installed Cargo source for `scip-0.10.0` was read locally to verify typed-range fields exist; no project dependency check, import, build or download was needed. This supports a concrete importer implementation path, not a claim that any producer artifact already passes Trellis's planned validator.
