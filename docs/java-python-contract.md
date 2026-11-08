# Java and Python parsing slice

Status: implemented and locally validated. See [support](java-python-support.md) and
[public validation summary](java-python-validation.json). Deployment-specific evidence is not included.

Support Java/Python parsing and the file → method → static sequence workflow.
Modify Trellis only. Any inspected repository is read-only input: never run Gradle/Maven/Python imports/package scripts,
install its dependencies, invoke providers, write source, change Git, or run its tests.
Existing Trellis development builds/tests are permitted. Root owns Cargo grammar additions.

Ownership:
- java-parser: src/indexer_java.rs, src/behavior_java.rs, tests/java_indexer.rs, tests/java_behavior.rs
- python-parser: src/indexer_python.rs, src/behavior_python.rs, tests/python_indexer.rs, tests/python_behavior.rs
- language-integration: src/file_tree.rs, src/store.rs (only language filters if needed),
  tests/java_python_http.rs and its own synthetic fixtures. No other owners' files.
- root: Cargo.toml/lock, src/indexer.rs, src/lib.rs, src/behavior.rs dispatch, docs/CLI/deployment.
- repository-inventory: read-only inventory and representative method selection, report privately.
Coordinate directly when needed; no cross-owner edits. Avoid global cargo fmt while others edit;
rustfmt only owned files, root does final fmt. Reply explicit results and limitations.

Interfaces:
indexer_java::extract(g:&mut Graph,file:&SourceFile,cancel:&CancelFlag)->anyhow::Result<()>
indexer_python::extract same. pub(crate), like indexer_rust.
behavior_java::build(revision:u64,seed:&Symbol,file:&SourceFile,calls:&[CallSite],show_all:bool)
 ->anyhow::Result<SequenceView>; behavior_python::build same.
Language strings java/python, extensions .java/.py. TypeScript is outside this contract; no alternate Jython syntax.
Use tree_sitter_java::LANGUAGE and tree_sitter_python::LANGUAGE with tree-sitter0.25 APIs.
Root wires module registration, discovery and dispatch; do not create placeholder shared adapters.
IndexOptions/index_workspace + Store publication should drive tests (like Rust tests).

Extraction: classes/interfaces/enums/records/constructors/methods, Python classes/functions/async/lambdas;
lexical parents/owners, measured call IDs + UTF8 byte/line/column ranges, regions/recovered syntax,
cancellation with parser progress_callback and traversal checks. IDs MUST include start AND end
byte (nested chained calls can share start). Unique closure ownership; no callable bodies attributed
to enclosing function. Native SymbolKind and SourceFile only, avoid schema changes.
Semantic provenance unavailable; never borrow JavaScript SCIP evidence, guess dispatch/types or bind
bare names globally. Signatures without bodies stay navigable but have explicit no-body behavior.
Annotations/decorators/defaults must not be silently conflated with function invocation body.

Behavior: shared immutable SequenceView/steps with measured calls source-linked by actual ranges/IDs.
Static possible paths, not runtime traces. Evaluation order receiver/arguments before invocation,
assignment evaluation per language, conditions/short-circuit/loops/try/return/raise/throw/suspension.
Explicit guard/alternate/exit nodes, no unconditional suffix after an abrupt exit or conditional return.
Opaque boundary if unsupported instead of linearizing or inventing calls. No function/lambda/class
body descent at definition/construction; no implicit generated methods/dispatch/return values.
Java: constructor calls vs methods, annotations/Lombok/reflection/generics/virtual calls not resolved;
modern switch/patterns/resources/synchronized etc conservatively bounded if not implemented.
Python: decorators/defaults and class execution special; comprehensions/generators/async/with/tryelse/
finally/match/dynamic attributes and invocation via descriptors require faithful logic OR explicit
boundary. Do not treat a comprehension body as unconditionally executed; no fake constructor type
claims for capitalized calls. All third-party/unresolved targets terminal, including Show all.
Use named unresolved receiver/callee hints, preserving uncertainty and separating them from types.
Use bounds comparable to existing200steps/20participants/depth24; resource/parse bounds explicit.
Avoid unbounded allocations just to find a seed. No importance hiding or fluent grouping required.

Tests: synthetic representative syntax, mixed languages, nested ownership, chained shared-start IDs,
UTF8, recovery, cancellation, Store::publish, evaluation ordering, controls/early exits,
unsupported boundaries, bounded traversal, original ranges/call IDs preserved, source-only scopes.
Do not copy private source into public fixtures. Use synthetic fixtures for public CLI/browser checks.
Automatic Maven/Gradle/pip library catalog and semantic resolution are NOT implemented by this slice.
