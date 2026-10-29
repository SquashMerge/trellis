# Source and member navigation

User wants source view -> classes/methods (context menu) and class members -> declared types/methods.
Use cached revision-bound evidence, not bare-name grep guesses. No repository execution, dependencies,
providers, reindex/schema change or changes to inspected repositories.

POST /api/navigation request (deny unknown fields):
- Source selector: {expectedRevision, path, line, semanticProducerId?} (1-based whole source line).
  `semanticProducerId` is an optional explicit published producer ID; absent/null is syntax-only,
  and an empty/unknown producer is an invalid request. No automatic producer selection or merge.
- Member selector: {expectedRevision, classId, memberName, startByte, endByte}.
Selectors are exclusive. Member selector MUST match a recorded ClassMember by name+exact byte range;
never trust typeHint/name as resolution. Source selector must validate path/line against cached file.
This slice is LINE-BASED navigation, not compiler go-to-definition at arbitrary cursor words.

Response: {revision, targets: [{symbol: Symbol, action: "sequence"|"class", reason: "declaration"|
"enclosing"|"type"|"call", matchKind: string}], warnings: string[], truncated: bool, requireIndex: bool}.
Targets use original measured declaration IDs/path/ranges. Declared functions/methods can sequence;
classes only if supported cachedclassprojection. Source line candidates: declarations on line,
nearest enclosing method/class, class type refs intersecting line, and measured resolved call targets
on line when present. Java unresolved bare-name or plain `this` calls may additionally offer
`sameClassCandidate` methods after bounded cached-AST proof of the exact invocation, lexical caller
and named class owner. Targets must be actual method declarations in that exact class; constructors
and arbitrary/qualified receivers are excluded. All matching overloads stay choices, not resolution.
This narrow proof supports callers inside ordinary method declarations only; constructor callers,
field/static/instance initializers, local/anonymous classes and lambda scopes remain unsupported. No global name matching for unresolved
calls and no Graph/call/sequence resolution mutation. Member selector returns own
measured method + declared field/parameter/return type refs scoped to owner and member range;
NOT arbitrary relationships elsewhere in class. Multiple candidates stay labelled choices, not
silently resolved. Unmatched/builtin types have explicit no-indexed-target status.

Backend owner: src/navigation.rs (new), src/lib.rs, navigation methods in src/store.rs,
API route/handler in src/http.rs, tests/navigation_http.rs (new) ONLY. Coordinate root HTTP asset route.
One read transaction/revision guard. Cached only, oldclassprojection requireIndex guidance when relevant.
Bound request input, query rows, candidates, target count, warnings, and RESPONSE bytes (<=512KiB).
Evidence-record input is separately bounded at 256KiB. Java field association may parse ONE cached
source file with a SEPARATE 2MiB source-text budget and 250ms parser deadline; source text is never
included in the response. This is explicitly approved. SQL-side narrowing precedes large payload loads;
never full-deserialize huge stored class definitions. Associate exact measured Java variable-declarator
range/name to its parent declaration type range; never guess from typeHint or neighboring text.
Include auth/host/origin, stale revision, cache-only, crossfile types, ambiguous/unmatched types,
UTF8 line boundaries, overloaded methods/shared field ranges, exact member validation, limits tests.

Source UI owner: web/navigation.js (new), web/navigation.css (new), tests/navigation-ui.test.cjs(new).
window.TrellisNavigation.init({request,currentRevision,currentSession,openClass,selectMethod,showMenu}).
request(path,{method,body}) same interface asClasses. showMenu(event,actions) delegates existingaccessible
classcontextmenu. Public open(event,selector,{isCurrent?}={}) captures anchor/coords synchronously,
prevents native menu then fetches targets; callbacks must rechecksession/revision/request/scope.
Public attachSource(container,{path,revision,startLine,isCurrent}) binds delegated click/contextmenu/
ShiftF10/ContextMenu on existing .source-line rows with .line-number text; do NOT replace source content.
Return/reset/dispose invalidatespendingnavigation. A visible Navigate action and keyboard line movement
must work withoutthousandstabstops; preserve selection/copy/scroll and highlightedsource ranges.
Inline callable actions read one pinned `/api/symbols` page (limit 200) when source is attached;
continuation requires explicit **More source actions**. Never fetch another source file for actions.
Line-menu `/api/navigation` reads remain explicit, not triggered by line selection. Include the source
serial guard and reset on source changes; late actions cannot run after another selection, logout or
index-pin adoption. Inline controls must not pollute source text or native copy.
No source fetch justtoopenmenu. Menu candidates distinctreason+certainty+qualifiedidentity whenavailable.
No guessing arbitrarywordcall targets. Empty/loading/errors visible and recoverable; safe text nodes.
Root integrates assets and init/attach/reset in app.js/index.html/style hooks, CI and docs.

Class UI owner: web/classes.js web/classes.css tests/classes-ui.test.cjs ONLY.
Keep direct method-name click -> sequence viaexisting selectMethod exactmeasuredmember. Add member
contextmenu/touch action and clickabletypehint -> navigation lookup. Call optionalapi.navigateMember
(event,{classId,memberName,startByte,endByte},{isCurrent}) wiredbyroot toNavigation.open. Predicatecaptures
snapshot/generation/displayTicket AND currentlyvisiblepanel. No arbitraryglobaltypehintsearch. Reuse
sharednavigationmenu; don't duplicate async/auth handling. Fields/methodsremainfolded bydefault,
avoidnestedbuttons, keep originalclassnode contextmenu fromhandlingmember event too. Exact callbacks,
source callcount0 untilintentionalread, asyncstale, keyboard/touch/Unicode/safe-text tests.

Root handles integration/review/native+UI gates/staging browser/deployment. Preserve existing
workspace snapshots; providers stay disabled and closed budgets stay untouched.
Keep private validation artifacts outside the public repository. All public fixtures are synthetic.


## Source-to-diagram continuity (#392)

`selectMethod(symbol, revision)`, `openClass(symbol, revision)` and
`inspectSymbol(symbol, revision)` receive the measured symbol and the source view's exact full pin.
Only declaration/enclosing targets with `matchKind: "measured"` or call targets with
`matchKind: "resolved"` become source diagram choices. Syntax candidates remain non-actions.
Call graph stays disabled until a browser view exists. Inspect uses the existing inspector and does
not fetch source; its explicit source action retains the same guards.

The app owns source return and routing. Before a source-to-diagram transition, it replaces the current
entry with an opaque source-return key and pushes one diagram entry. In-memory entries reference
admitted source and diagram data; browser history stores no source bytes, token or DTO. Back restores
the section, selected Files symbol/basis, source range and active line, focus and scroll using the
normal renderer. Already admitted declaration metadata is retained with the entry, including explicit
continuation pages, so restore needs no new metadata request and never waits on one for position.
Pending metadata remains independent of focus/scroll restoration. Entries are invalidated by session, checkout or full-pin changes. Missing state is a
visible refusal. No new source read or source-text persistence is needed for a retained entry.

`#sequence?sid=…` waits for authentication and explicit checkout selection. Generated links add
`rootKey`, `indexGeneration` and `indexRevision`; mismatches are refused. The sequence endpoint's
returned seed must match the requested ID and full pin; no workspace scan or lexical lookup resolves
a URL. Back, new routes, checkout/session changes and pin adoption fence all pending route results.

Library candidate identities are not workspace declaration IDs. External source therefore has an
explicit unavailable hint and no workspace navigation attachment. Working-tree-only source has no
indexed actions. Positive external navigation needs a separate indexed identity contract, not a
same-name or same-path guess.


### Explicit resolved call targets

With an explicit semantic producer, source-line calls use the same selected read transaction and
full revision pin as the declaration projection. Each native call's ID and measured range join its
published binding through the #40 `select_binding` / `boundary_reason` rules. Only fresh, resolved,
direct/constructor Internal targets with an exact selected document and measured callable syntax ID
become `reason: "call", matchKind: "resolved"` choices. These choices also carry native `callId` and
byte `range`; repeated sites remain distinct. Unresolved, ambiguous, stale, external, dispatch-only,
failed/omitted/unrequested coverage and non-callable targets do not gain callee actions. The measured
enclosing method remains available. A missing required in-scope coverage row is typed corruption,
not a successful partial navigation result. Existing navigation row, evidence and response budgets also cover these reads.
Browser producer selection remains a separate owner decision; this optional API never picks one.
