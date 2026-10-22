# Decision 0001: prospective semantic-evidence core contract v1

- **Status:** proposed for ratification by the independent integrated review
- **Date:** 2026-09-23
- **Scope:** documentation-only Stage 1A contract
- **Decision target:** `../contract-v1.md`

## Context

Trellis needs a language-neutral description of measured syntax, semantic evidence, durable declaration identity, and bounded rooted graph answers before later implementation and corpus work can be judged consistently. Current Java, Rust, Python, and JavaScript importers and IDs differ. Current production behavior is useful implementation context, but it is not proof of this contract.

The central safety problem is avoiding stronger claims than the evidence supports. Syntax, a symbol binding, a reference, a measured invocation, and a possible dispatch target are different facts. Freshness and coverage are also independent. The answer is a static, source-ordered view over a pinned snapshot, never an execution trace or runtime-complete call graph.

## Supplied owner decisions

The approved issue/brief supplied these binding inputs; this decision record does not present them as choices made by the document author:

1. The v1 record families, exact closed field inventory, required-field rule, shared scalar/path/range definitions, and atomically rejected malformed-artifact boundary.
2. Tuple-scoped per-producer coverage, immutable revisions, captured semantic bases, separate coverage/freshness, historical failed-refresh handling, and `staleTarget` semantics.
3. Exact measured stable-ID spelling, language-specific lookup normalization, all seven reference roles, Java alias as the sole non-applicable role, and the later 40-reference-record floor per language.
4. Canonical JSON profile, NUL-terminated digest domains, declaration and revision-local occurrence inputs, Java signature discriminator, sibling ordinal namespace, projected headers, sibling-group hashes/counts, and continuity requirements.
5. Exact static-expansion eligibility: only fresh, exact, unambiguous internal direct/constructor bindings with a nonstale target.
6. One pinned FIFO breadth-first graph algorithm, local source order, exact defaults (`depth=2,maxNodes=150,maxCalls=500`) and limits, frontier arithmetic/order, and no runtime completeness claim.
7. The documentation-only surface: the contract and this record first, then exactly 64 descriptor vectors plus a deliberately narrow raw-digest checker in the next slice. No production, CI, API, persistence, or configuration change.

## Prospective ratification decisions

Subject to independent review, we ratify `contract-v1.md` as the complete prospective v1 algorithm and interpretation baseline:

- All logical records are closed. Every field is required; nullable fields carry explicit nulls. Unknown fields are rejected by a future v1 consumer.
- Canonical bytes and all identity inputs are fixed by the contract. Lookup normalization cannot enter ID bytes. Header hashes cover only the declared projection, not raw source headers.
- The PR review chose opaque 128-bit syntax and occurrence handles (the first 16 bytes of their full domain-separated SHA-256 digests) to reduce repeated result size. Full 256-bit digests remain in integrity fields and vector digest rows. Detected collisions (same typed 16-byte handle, different full digest) invalidate the affected artifact under the later, narrowly scoped [Decision 0015 hash-based identity amendment](0015-native-hash-based-identity.md); presentation-only display keys are never identity or identity-hash inputs.
- Durable attachment never searches by name or header for a replacement. Duplicate-header survival needs independently proven group continuity; equal hashes and counts alone do not prove it.
- Per-producer coverage may be validly partial. Malformed artifacts are rejected atomically. Old evidence survives only as labelled history.
- Graph answers select one producer or syntax-only evidence. They expose every measured call as one ordered edge when within call budget, retain static semantic boundaries, and use ordered frontiers only for depth/node/call limits.
- The worked truth tables, anchor audits, graph arrays, failure table, and finite checklist are normative interpretation aids. If a later vector conflicts, the prose must be amended explicitly and affected derivations repeated; the checker cannot silently choose an alternative algorithm.

This is **prospective ratification**, not a statement that production implements the records or algorithms today. Final issue acceptance still requires the later vectors/checker slice and a distinct integrated reviewer.

## Rejected alternatives

| Alternative | Reason rejected |
|---|---|
| Reuse current production IDs or semantic symbol strings as v1 syntax IDs | Existing schemes include different path/content/range inputs and vary by language. Semantic symbols are not measured syntax identities. |
| Normalize identifier spelling before hashing | It destroys measured identity and conflates lookup with identity. Language lookup rules differ and collisions can remain ambiguous. |
| Use Git commit or source bytes alone for freshness | Dirty content, manifests, dependency/config/toolchain, and producer changes can alter semantics with unchanged caller bytes. |
| Merge producers or pick the first contradictory exact binding | It hides provenance/conflicts. Requests select one producer; contradictory exact evidence becomes ambiguous. |
| Heuristic range/name/enclosing-definition joins | They can install the wrong semantic fact. Only exact converted spans in the exact bytes/revision are eligible. |
| Treat references, callback values, declaration bindings, or possible-dispatch candidates as calls | Each asserts a different fact and would fabricate executable edges. |
| Expand a virtual/interface/dynamic target when only one candidate is listed | `possibleDispatchComplete` is always false and does not establish a statically selected body. |
| DFS, evaluation order, or global-file sorting | The owner chose deterministic FIFO BFS with per-owner local source order. It is a source view, not execution order. |
| Drop cycle/repeated/boundary calls or stop after a node-limit refusal | It would hide measured facts and make budgets/order unstable. Such calls remain visible and count toward maxCalls. |
| Mark exact numerical caps truncated even with no work left | Truncation means omitted work represented by a frontier, not merely equality with a number. |
| Revision-wide orphaning of all anchors | It loses body-edit durability. Unique headers and proven-unchanged duplicate groups may survive safely. |
| Infer continuity from equal group hashes/counts | Indistinguishable replacement/reorder can preserve those values. Independent membership/order evidence is required. |
| Claim the projected header hash covers the raw source header | Bodies, trivia, defaults, and annotation/decorator argument expressions are intentionally absent. |
| Make the later checker canonicalize descriptors or validate expected IDs/anchors | A second implementation could bless its own mistake. The checker is intentionally raw-byte/shape/digest only; hand review owns semantics. |
| Deploy a JSON Schema, endpoint, cursor, migration, or production rewrite now | It exceeds Stage 1A and would prematurely freeze a wire/storage implementation. |

## Compatibility and non-migration boundary

This change creates only normative prose. It does **not** migrate or claim compatibility for existing production IDs, importer APIs, MCP DTOs, storage rows, projections, traversal behavior, auth rules, or source watchers. It adds no endpoint, schema, changelog, grant, feature flag, runtime consumer, or CI job. Future adapters must translate producer positions against exact bytes and must stay inside explicitly admitted source sets; reading evidence never authorizes producer/build execution.

Existing Java/Rust/Python IDs that include path/content/ranges and JavaScript imported identities may differ from prospective `sid:v1`. Existing `Internal` traversal does not prove direct-dispatch safety. Any rollout requires a separately approved compatibility and migration plan.

## Deferred proof owners

| Proof / implementation | Owner / follow-up | What remains deferred |
|---|---|---|
| Record validation, joins, freshness, dispatch, BFS/frontiers | #10 and later implementation stages | Runtime code and executable behavioral tests. |
| Trustworthy sibling-group edit classification | #10 and #27–#30 corpora | Evidence that membership/order is unchanged; vectors only state assumptions. |
| Topology and persistence | #23 | Storage model, migrations, grants, read projections. |
| MCP wire format and presentation-only `displayKey` result field | #24 (implemented by #17 after ratification) | DTO/schema/error transport, field nullability, ordering, and compatibility; no display key in identity-typed requests. |
| Corpus consistency checker | #26 | Cross-record semantic validation. |
| Source/token witnesses and language/compiler semantics | #27–#30 | Parser witnesses, binding quality, role-floor proof, and edit evidence. |
| Traceability | #9 | Requirement-to-evidence links only. |
| Exactly 64 descriptor vectors and bounded checker | Slice 2 of this issue | Literal hashes/anchors, README, raw-digest checker, negative checker trials. |
| Final prose/vector/checker agreement | Independent integrated reviewer | Re-audit after both slices; green digest output is insufficient. |

No deferred owner may reinterpret a closed algorithm choice silently. A required change needs an explicit contract amendment and repeated affected derivations.

## Risks and consequences

- Prospective IDs can change after rename, path/source-set move, container-key change, or sibling ordinal shift. Descendant identities can change with ancestor keys. Body edits alone do not.
- Normalized lookup collisions can be ambiguous even when measured IDs differ.
- Conservative source-set-wide freshness may withhold usable facts; v1 intentionally rejects lookup-dependency precision optimization.
- Projected-header equality is weaker than source equivalence. Duplicate groups require trusted continuity evidence, and indistinguishable physical replacement may be undetectable.
- Static direct calls remain an under-approximation of runtime behavior. Boundary edges and `partial=false` never imply runtime completeness.
- The later raw-digest checker can accept a same-shaped but semantically wrong expected ID. Independent byte-by-byte derivation and integrated review are mandatory.

## Ratification gate

Before acceptance, a reviewer independent from this slice author must complete the finite checklist in `contract-v1.md`, audit every field and every worked example against the approved brief, and close ambiguities in canonical bytes, ordinal groups, projected headers, continuity, and anchor outcomes. Slice 2 may then author vectors against that exact baseline. Final ratification occurs only after a distinct integrated audit of prose, decision, all 64 cases, and checker limits.
