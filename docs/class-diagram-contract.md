# Class diagram slice

Status: implemented, reviewed and deployed. See [usage](class-diagrams.md) and
[public validation summary](class-diagram-validation.json). Deployment-specific evidence is not included.

User: add working class diagrams with right-click menu that pulls up related classes.
Inspected workspaces are read-only inputs. Do not write their source, execute their commands,
imports or builds, install dependencies, or invoke providers. No Git initialization/commit.
Existing tokens and workspace states must remain intact. Retired saved records and provider budgets remain in place and ignored.

Initial meaningful support: Java/Python declared classes/types, direct members, inheritance and explicit
declared field/parameter/return type references. No inferred receiver types, runtime dispatch, ownership,
composition/multiplicity, generated methods or Maven/pip resolution. Candidate links are syntax matches,
not compiler resolution. Ambiguous/unmatched names remain terminal hints, hidden by default but revealable.
JS/Rust class/type extraction was outside this initial slice; #452 adds it (below).

JavaScript (#452 slice 1, owner direction 2026-10-29): a named ES `class_declaration` at module level,
directly or as an `export`/`export default` declaration, with a unique measured `type` symbol, is a
ClassDefinition (`language:"javascript"`, `declarationKind:"class"`, qualified name = path without
`.js`/`.mjs`/`.cjs`, `/`→`.`, then the name). Members are direct `field_definition`s and
`method_definition`s (static, getters/setters, private `#names` included) in source order; static
blocks are not members. A method's `symbolId` is its unique measured method symbol, so `#private`
methods have none. `extends` with an identifier or dotted member expression records one `extends`
relation; other heritage expressions record none. No field/parameter/return relations: plain JS has
no type annotations and JSDoc is out of scope. Class expressions, prototype assignment, factories,
object-literal "classes" and function-body classes are not extracted. Per-file limits, composition and
caps are those of Decision 0004, unchanged. As for Java and Python, every relation stays a terminal
`unmatched` hint (Decision 0004 "No resolution is added"; #64 no lexical class edges; publication refuses
a targeted relation). `GET /api/classes` returns `supportedLanguages` (`crate::classes::SUPPORTED_LANGUAGES`)
so clients can gate class views by language.

Rust (#452 slice 2): `struct_item`, `enum_item`, `union_item` and `trait_item` with a unique measured
`type` symbol are ClassDefinitions (`language:"rust"`, `declarationKind` struct/enum/union/trait).
Qualified names are `crate-key::module::Name` from the conventional Cargo layout (`src/a/b.rs` and
`src/a/b/mod.rs` are `a::b`; `src/lib.rs`/`main.rs` the crate root; the root crate has an empty key,
another crate's key is its directory, and `src/bin/*` targets and files outside `src/` are keyed by
path), plus inline `mod` names. Members: struct/union named fields (tuple fields `0`, `1`, …) with the
declared type as hint; enum variants with the payload text as hint; trait methods (signatures and
defaults). Relations: `field`, `parameter`, `returns` from field and signature types, `extends` from
supertrait bounds, `derives` from `#[derive]` names, all terminal `unmatched` hints; generic parameters
(type and const) and `Self` are excluded. Function bodies are not read.

Per-file extraction also yields `FileExtraction.impls`: one `ImplItem {target, records, text, value}` per
inherent-impl method (with its own path, range and measured method symbol) and its signature relations,
and one `implements` relation per `impl Trait for Type`. `target` is the qualified name that the self
type resolves to through that file's own `mod`/`use` scopes only: declared types of the same module,
`use` bindings (`as`, lists, `self`), `crate`/`self`/`super` paths, and `use super::*`-style globs into a
module of the same file. Unbound names, other globs, re-exports, aliases and extern paths do not resolve;
resolution steps are capped per file (10,000), so glob graphs cannot blow up; a self type that is a generic parameter, a reference, tuple or trait object is a blanket/non-path impl
and adds the per-file notice `"{path}: blanket or non-path impl not attributed to a class"`. Negative
impls declare nothing; macro-generated items and derived impls are not attributed. `F` therefore still
depends only on its own file, and impl items charge the same per-file and detail budgets.

Composition (the global step) charges a file's impl items after that file's own class items, in source
order, under the Decision 0004 detail rules; a refused item marks its target class `truncated`. It then attaches each admitted item to the one admitted Rust class whose qualified name equals
`target`: methods are appended (class member limit 256; excess marks the class truncated), relations get
that class as owner and id `class-ref:{owner}:{path}:{start}:{end}:{kind}:{typeName}` (the path keeps ids
unique within the owner's projection when several files implement it). No match or several
(cfg duplicates) leave it unattributed. Relations are ordered by owner document, then `(path, start_byte,
id)`, and stored with the owner's class projection. Because another document's class rows depend on an
impl, a local publication that changes, removes or recovers a Rust document recomposes the revision from
all stored `F` instead of reusing per-document projections. A method seed whose lexical ancestry has no
class (a Rust method's parent is its impl block) resolves to the one Rust class recording that method.

Raw identifiers are one identity: `r#type` and `type` bind, resolve and qualify identically (declared
names, `mod`/`use` aliases, generic parameters and paths), while recorded names and ranges keep their
source spelling. Type and const parameters follow the Decision 0004 limits: at most 256 per declaration
and per scope (outer impl or trait parameters plus a method's own; `Self` is not counted). An over-limit
type or trait is skipped with `Class type parameter limit reached (256/declaration)` and an incomplete
registry; an over-limit impl is not attributed (`{path}: impl type parameter limit reached …`); an
over-limit method keeps its member but omits its signature hints and truncates its class (`Method type
parameter detail limit reached (256); method references omitted`).

The member navigation selector accepts an optional `memberPath` (the recorded member's `path`). Name and
byte range are unique only within one file, and a Rust class can record members from several files, so
the browser sends it. Navigation reads the member's own document; a selector matching more than one
recorded member, or none, is refused.

Backend core owner: src/classes.rs + tests/classes.rs only. Public serde camelCase DTOs:
ClassMember {name:String, type_hint:Option<String>, symbol_id:Option<String>, path:String, range:SourceRange}
ClassDefinition {symbol:Symbol, qualified_name:String, language:String, declaration_kind:String,
 fields:Vec<ClassMember>, methods:Vec<ClassMember>, truncated:bool}
ClassRelation {id:String, owner:String, target:Option<String>, type_name:String, kind:String,
 path:String, range:SourceRange, candidate_ids:Vec<String>, match_kind:String}
Catalog {classes:Vec<ClassDefinition>, relations:Vec<ClassRelation>, warnings:Vec<String>, truncated:bool}
Catalog::build(files:&[SourceFile],nodes:&[Symbol],cancel:&CancelFlag)->anyhow::Result<Catalog>.
Stable IDs bound to measured class Symbol IDs and cached-source ranges. Relation kind extends/implements/
field/parameter/returns; matchKind syntaxCandidate/unmatched/ambiguous. Unique *scoped* matching only:
Java package/import/lexical context; Python explicit module/relative import/lexical context. No global bare
name guessing. Generic type parameters and shadowing must not falsely match workspace classes. Cap per-file
AST visits/depth, declarations/members/references and global text/records; cancellation checks. Limits/recovery
explicit. Two-step build ([Decision 0004](semantic-evidence/decisions/0004-compositional-class-catalog.md)):
per-file extraction uses only per-file limits; global caps, warnings and truncation are applied when composing
per-file results in path order. Catalog::build is that composition over every file. No function-body traversal for class relationship discovery. Preserve nested lexical classes safely.

Store/HTTP owner: src/store.rs, src/http.rs, src/class_diagram.rs, tests/class_diagram_http.rs.
Persist Catalog projection atomically with existing index publication, building before transaction. Graph,
question/provider packets unchanged. Add schema3 migration (preserve schema1/2 data and durable workspace data).
Store catalog must support old snapshot as empty with explicit Index workspace notice, not rescan live files.
GET /api/classes?path=&q=&offset=0&limit=100&revision=N -> {revision,items:ClassDefinition[],nextOffset,truncated,warnings}.
POST /api/class-diagram {seed:String,expectedRevision:u64,expanded:Vec<String>=[],includeUnmatched:bool=false}
-> {revision,seed:String,nodes:ClassDiagramNode[],edges:ClassRelation[],warnings:Vec<String>,truncated:bool}.
ClassDiagramNode {id:String,class:Option<ClassDefinition>,label:String,kind:String,expandable:bool}.
Actual class nodes id=class.symbol.id, kind=class; unmatched type hints kind=unmatched/ambiguous, class=null,
terminal. Edges owner->target; for hint target use placeholder node.id in DIAGRAM response only, no changing
stored relation certainty. Requests from class or method ID resolve nearest enclosing class, else readable400.
Seed plus ONE-HOP incoming/outgoing declared references; expanded is additional class seeds (max12), retain
all earlier expansions; max24nodes/64edges. Actual linked classes before unmatched hints. Explicit truncation.
Revision read transaction guard, seed/session/workspace safety, bounded search (literal LIKE escapes), strict
request validation/auth/routes/error mapping. Serve /classes.js and /classes.css (root wires final assets).
Coordinate on core DTOs. No opaque snapshots outside cache or graph export changes.

Frontend owner: web/classes.js, web/classes.css, tests/classes-ui.test.cjs only. Vanilla self-contained
window.TrellisClasses controller with init({request,readSource,selectMethod,currentRevision,currentSession}),
open({seed?,path?}), reset(). root adds UI IDs classes-panel,classes-state,classes-query,classes-search,
classes-results,classes-diagram,classes-unmatched. request(path,options) existing authenticated JSON helper;
root wraps current session invalidation. open uses GET class lookup for path or seed then POST diagram;
class tab must also allow global search. Context menu on actual class nodes: Show related classes (one-hop
expansion), Read class source, Focus this class; member method click -> selectMethod(symbol). Explicit source
via readSource({path,range},revision). No source fetch on plain selection, no provider requests ever.
Native right-click and Shift+F10/ContextMenu; visible menu button per node for discoverability/touch. Fixed/
top-layer menu viewport-clamped, Arrow/Home/End/Escape, focus return, dismiss outside/scroll/session/revision.
All source is text, no innerHTML/eval; immutable DTOs. Distinguish syntax candidates with dashed edges/labels.
Maintain charcoal/flame-orange, local fonts, 13px labels, no invented cardinalities. Contained scroll on
mobile/large graphs. Bounded deterministic layout and stable node identities during expansion. Loading,
empty, unindexed,error,partial and unsupported-language states; stale responses ignored. No root app/shell edits.

Root owns src/lib.rs wiring, frontend app.js/shell.js/index.html/style.css integration, docs, gates/deployment.
Root adds Classes tab, file/method context actions via exported controller/context support; no existing
sequence/tree/provider behavior lost. No global fmt until all owners frozen. Tests all synthetic/public;
Keep private-workspace validation artifacts outside the public repository. Reply explicit API, tests, limitations and freeze.
