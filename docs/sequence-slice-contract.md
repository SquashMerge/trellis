# File / method / sequence vertical slice

## Current resolved sequence contract (#378)

The sequence is a static possible-path view, not an observed runtime trace. Its syntax
adapters preserve measured call IDs, source ranges, evaluation order, guards, alternates,
hidden entries and nested boundaries. Semantic enrichment never flattens or reorders them.

`SequenceView.revision` and `POST /api/sequence.expectedRevision` are `IndexPin` values,
not integer revisions. All evidence and declaration targets are read inside the same
selected SQLite transaction. No provider, filesystem read or index request is performed
by enrichment. Callee bodies are expanded only as described under #522 below.

The store uses, at that selected snapshot, the sole published semantic producer whose
profile covers the seed's language (owner ruling 2026-10-30, #483, amending #392). Zero
producers preserves the syntax-only DTO (with `semantic.reason`, #522). Multiple producers for the seed's language
preserves syntax and adds `multiple semantic producers for <language>; showing syntax`; it
never picks or merges producers. Published producers that do not cover the seed language
also preserve the plain syntax-only DTO, including receiver hints, without semantic labels
or declarations.

A measured call step may contain `callEvidence`:

- `boundaryReason`: the call-graph engine's `none`, `external`, `unresolved`, `ambiguous`,
  `dispatch`, `stale`, `nodeLimit`, or `missingEvidence`.
- Optional `resolution`, `dispatch`, `freshness`, and `provenanceId`: the selected semantic
  binding/provenance fields, not the older graph-model resolution enum.
- Optional `externalPackage` `{manager,name,version}` (#450): only for `external`, the package
  field of the selected binding's SCIP symbol (`version` null for `.`); absent for local or
  malformed symbols. It is a producer claim for in-context library lookup, never a target,
  lane or resolved library declaration.
- Optional `dispatchTarget` `{id,name,kind,path}` (#557): only for a `dispatch` boundary with
  an exact internal binding, the declaration it names (a trait, interface or prototype
  method), not a runtime body. It takes no lane and never expands.
- `target`: present only for a fresh exact resolved internal direct/constructor target
  located in this snapshot. It is the exact participant ID, not lexical receiver text.

Resolved participants have `declaration: Symbol`; the seed has the same witness in an
enriched view. Navigation uses that declaration with `view.revision`. Ambiguous candidates,
external symbols, dispatch possibilities, stale targets and missing evidence never create
a target lane or declaration action. The legacy step `resolution` stays absent; it is not
a competing source of semantic truth. Syntax participants are hints, not declarations.

Every measured occurrence is classified by the shared engine **before grouping**. Graph
answers retain their existing grouping and navigation behavior. Groupable repeated calls
therefore retain distinct sequence IDs, source positions and evidence. Chain previews
represent only their first measured call, not a guessed terminal target.

`semanticWarnings` carries the engine's structured warnings (omitted when empty); their
messages also appear in `warnings`. Existing syntax warnings are preserved. No warning
code or resolution is invented by the browser. Bounds remain 200 shown steps, 20 participants
and depth 24, with a separate 2000-step work bound (#536, below). If a resolved target cannot fit, its boundary is `nodeLimit`, its target is
absent, and the view is truncated with an explicit participant-limit warning.

### Participants, expansion and incidental calls (#522)

This amends "no callee-body expansion" above, per #522 (owner's 2026-10-31 live review).
Enrichment still performs no provider call, filesystem read or index request: every body
it adds is read from the same selected snapshot.

- `SequenceView.semantic` is `{tier, producerId?, reason?}`. `tier=semantic` names the sole
  producer whose fresh bindings enriched the view. `tier=syntax` keeps the syntax-only DTO
  (its source-hint lanes included) and gives the reason: the producer note (zero producers,
  none covering the language, or several), the seed document's coverage not selected or not
  `complete`/`partial`, or no measured call of the seed having a fresh selected binding
  (stale or missing evidence). One fresh binding is enough to enrich; the seed's other calls
  then keep their own `stale` or `missingEvidence` boundaries. A seed with no measured calls has nothing
  to fall back from: it reports `tier=semantic` with the producer, and none of its steps
  carries call evidence.
- Each exact direct/constructor target (contract-v1 dispatch, #470) is a participant. The
  first call to each function/method target has the target's own measured body expanded as
  that call step's `children`. Expanded steps carry `lane` (the participant whose body they
  belong to; absent for the seed's own steps) and IDs prefixed by the expanding step's ID.
  They are enriched by the same engine rules, recursively. A target already on the expansion
  path (recursion) or already expanded is drawn as an arrow only. A body that would exceed
  the shown-step, work or depth bound in the whole view (#536, below) is not expanded
  (all or nothing), the view is truncated and a warning names it. Other dispatch, ambiguous (#466, one edge), unresolved, external and
  stale calls stay boundaries.
- A syntax heuristic never hides a call with a fresh exact internal target (for example a
  repository-defined JavaScript `console.log`); it and its expanded body stay visible.
- A standalone call step whose boundary reason is `external` (the standard library or any
  other package) is incidental. `incidentalSteps` counts them; unless `showAll`, they are
  `hidden` and counted in `hiddenSteps`. A chain group whose calls are all hidden is hidden.
- `callEvidence.semanticBasis` is the selected binding's captured basis, as MCP reports it.
- Depth 24 bounds the step tree to 24 levels, as the browser renderers already did: a step
  on the 24th level that still nests becomes one `boundary` ("Depth limit: behavior
  omitted.") at its own range. The Rust adapter bounds its syntax
  recursion separately, at 96 levels, so deep syntax that adds no steps keeps its calls.

### Visible and work step bounds (#536)

Owner ruling 2026-10-31 (#536, on #531): hidden incidental steps do not consume the
200-step bound. This amends the #522 bullets above, where hidden steps counted.

- **Visible bound: 200 shown steps.** A step counts only if it is shown; a `hidden` step
  (an incidental call, a hidden logging wrapper) and anything it nests do not. The seed's
  incidental calls are hidden first, then the seed is kept in pre-order (a step, its
  `children`, then its `alternate`) up to its 200th shown step; everything after that,
  hidden steps included, is omitted. A callee body is hidden the same way before it is
  admitted, and is expanded only if its shown steps fit in what is left.
- **Work bound: 2000 steps, hidden ones included.** It bounds computation, so hostile
  input stays bounded however many of its steps are hidden. A language adapter builds at
  most 2000 steps of one body, and a callee body is not expanded when the steps the view
  keeps (the seed's, after its visible cut, and every admitted body), its own included,
  would exceed 2000. Each target's body is built at most once, so a refused body costs no
  more than one build. A Rust chain group is a step too: grouping is presentational and
  stops, with a warning and nothing measured lost, before a body would pass 2000 steps.
  Depth 24 and 20 participants are unchanged.
- **Show all respects the visible bound.** With `showAll` nothing is incidental-hidden, so
  every step counts: the seed stops at its 200th step and a callee body that does not fit
  is not expanded, exactly as above.
- **Typed truncation.** `truncatedBy` lists, in this order, the bounds that truncated the
  view: `visibleSteps`, `workSteps`, `depth`, `participants`. It is omitted when none did
  (`truncated` may still be set by an adapter's other limits, which keep their warnings).
  The visible bound's warning is "Sequence step limit reached (200 shown steps); later
  behavior is not shown."; a refused body names itself ("Sequence step limit reached; the
  body of X is not expanded.", "Sequence work limit reached; …").
- A syntax-only view (any `semantic.tier=syntax`) applies the same visible and work
  bounds; only the adapter's own hidden steps are hidden there.
- `incidentalSteps` and `hiddenSteps` count the steps the view keeps.

### Folded early returns and the seed reserve (#542)

Owner ruling 2026-10-31 (#542, after #541's measurement of `coordinate`). It amends the
two sections above.

- **Folded `?`.** In Rust, a `?` whose direct operand (through parentheses) is a measured
  call no longer draws its own "?: continue only on success; otherwise early return"
  branch. The branch, with its early return as `alternate`, moves to that call step's
  `folded` list. The call then means "may return early", and the browser appends that
  marker to its row (a chain group shows it when one of its calls carries it, collapsed or
  not). A `?` on anything else keeps its branch: a value, a field or tuple index (even of a
  call's result, `make().f?`), an await. The operand call is the one evaluated last, also
  when it sits under its own arguments' continuation guards (`outer(x?, y())?`).
- **Folded guards.** An "only if the preceding path continues normally" guard folds into
  the nearest call before it that carries a folded `?`, provided only plain calls (which
  cannot leave the path themselves) come between them. The guard also moves to that call's
  `folded` list, and its children (the guarded rest of the block or expression) follow the
  call at the same level instead of nesting. Any other guard, such as one after an `if`
  that returns, a `break` or an opaque boundary, is unchanged.
- **Lossless.** Every folded step keeps its ID, label, kind, range and alternate. The step
  inspector lists them as "Folded control flow" disclosures with their early return. Folded
  steps are not shown rows, so they do not count toward the visible bound. They do count
  toward the 2000-step work bound.
- **Incidental.** A call that may return early is control flow, so it is never incidental,
  even when it is bound external: it stays shown.
- **Seed reserve: 150 + 50.** When one of the seed's first 150 shown steps is a call that
  can expand, the seed's own shown steps stop at 150 (`SEED_STEPS`). A call can expand when
  its fresh exact internal target is a function or method other than the seed itself, the
  same structural test expansion applies; a class constructor target keeps its lane but
  reserves nothing. The other 50 of the 200 shown steps (more when the seed is shorter) are
  left for the bodies its first calls expand, admitted all or nothing in call order as
  before. A seed with no such call there keeps all 200, and
  so does a syntax-only view. When the seed is cut at 150 the warning is "Sequence step limit
  reached (150 shown steps of the method, 50 reserved for callee bodies); later behavior is
  not shown.", typed `visibleSteps`.
- **Show all** applies the same split: nothing is incidental-hidden, the seed stops at 150
  shown steps, and bodies share the remaining 50.

### Trivial accessors and logging calls (#543)

Owner rulings 2026-10-31 (#543). They amend the #522, #536 and #542 sections above.

- **Incidental reasons.** Each incidental step carries `incidental`, one of:
  - `external` (#536);
  - `logging`;
  - `accessor`.

  All three are hidden unless `showAll`, counted in `incidentalSteps` (and in `hiddenSteps`
  while hidden) and shown under the one "Show/Hide N incidental calls" toggle. The
  inspector labels them "hidden: external call", "hidden: logging (naming heuristic)" and
  "hidden: trivial accessor" (with "(shown by Show all)" when shown). An incidental call is
  never expanded, even when it has a resolved target.
- **Lanes.** A hidden accessor or logging step takes no participant lane and has no
  `target`; its `callEvidence` keeps the binding (`boundaryReason: none`), and the browser
  accepts that only for a hidden incidental step. With Show all the step is shown, so it
  gets its lane and target as any resolved call, within the 20-lane cap.
- **What the toggle counts.** `incidentalSteps` and `hiddenSteps` count steps, not calls:
  the receiver and argument steps hidden with a logging call or accessor (an attribute
  lookup, a branch in an argument) count with it, so one call can count as several.
- **Logging (all languages, syntax and semantic views).** A call whose callee is
  `<receiver>.<level>(…)` is hidden under the naming rule, where:
  - `<receiver>` is `log`, `logger`, `LOG`, `LOGGER`, `_log`, `_logger`, `self.log`,
    `self.logger`, `this.log`, `this.logger` or `console`;
  - `<level>` is `trace`, `debug`, `info`, `warn`, `warning`, `error`, `fatal`, `critical`
    or `log`.

  So are the Rust macros `trace!`, `debug!`, `info!`, `warn!` and `error!`, bare or under
  `log::`/`tracing::`. In more detail:
  - The steps evaluating the call's receiver and arguments (the steps just before it inside
    its source range) are hidden with it.
  - A Rust logging macro is a boundary that continues, so it no longer guards the
    statements after it.
  - A warning says the rule is a naming rule.
  - This subsumes the JavaScript `console.log`/`console.debug` rule.
  - **The owner exception to #522's rule:** a logging call stays hidden even when fresh
    exact evidence resolves it to a local wrapper. Show all reveals it.
- **Trivial accessors (semantic views only).** A call with a fresh exact binding is
  incidental when its target declaration's own body, read from the selected snapshot, is
  one statement that either returns a field or assigns a plain name to a field:
  - **Returns a field:** `self.f`/`this.f`; in Rust also `&self.f`, `&mut self.f` and
    `self.f.clone()`; in Java also a bare name that the native declarations list as a field
    of the enclosing class and no parameter, varargs included, shadows (an identity
    method such as `T id(T x) { return x; }` never qualifies). Python skips a docstring
    that is a plain string literal; an f-string executes, so it is behavior.
  - **Assigns a field:** `this.f = v`/`self.f = v`; in Java also `f = v` under the same
    field rule.

  The rule is the same for Java, Rust, Python and JavaScript. The steps evaluating its
  receiver and arguments are hidden with it, except a call that can expand (by the same
  structural test as the reserve, in the seed and in every expanded body). An unresolved
  `getX()` is never an accessor: the name alone never qualifies.
- A call that may return early (#542) is never incidental, and an operand subtree that
  holds one is not hidden with the call it feeds. Operand subtrees are hidden whole: every
  step they nest is marked and counted.
- Rust `::log::`/`::tracing::` macro paths and JavaScript optional calls (`console?.log`)
  match the same rule. Constructors (Java constructors, a JavaScript `constructor`, Python
  `__init__`) are never accessors, and a setter assigns with plain `=` only.

The browser draws an expanded step's arrows and notes from its `lane`, shows the tier and
reason in the sequence status line, and offers a "Show/Hide N incidental calls" toggle
(the same `showAll` request as Options → Show all steps).

Typed integrity, busy and pin failures abort the entire response, never returning a
partially enriched view. Lanes/arrows prove static declaration targets only: no runtime
receiver identity, timing, activations or return edges are inferred.

## Original syntax slice notes

The notes below record the initial syntax-only slice; current revision and semantic DTO
rules above supersede its historical integer-revision and targetless descriptions.


Primary interaction: indexed file tree -> expand file INLINE to methods -> click method ->
sequence diagram alongside tree. This uses no provider calls. Keep existing inspector/question
features accessible but secondary. Preserve selected root and explicit expansion.

Shared language-neutral behavior DTO (src/behavior.rs; no tree-sitter here):
SequenceView {revision:u64,seed:Symbol,participants:Vec<Participant>,steps:Vec<SequenceStep>,warnings:Vec<String>,hiddenSteps:usize,truncated:bool}
Participant {id:String,label:String,kind:String} kind method/internal/boundary; seed id is symbol.id.
SequenceStep {id:String,kind:String,label:String,path:String,range:SourceRange,callId:Option<String>,target:Option<String>,resolution:Option<Resolution>,children:Vec<SequenceStep>,alternate:Vec<SequenceStep>,hidden:bool}
Kinds: call,branch,loop,try,return,throw,await,boundary,note,effect (strings wire; use enuminternallyifdesired).
Target is participant id for call arrows. Branch children true body, alternate false/else; try children try body,
alternate catch/finally grouped labeled nodes. No fabricated caller/return edges. Bare return/throw are notes.
All real call steps keep original callId/resolution/source range. Unresolved targets boundaryparticipants;
do not present same receiver text as provenobjectidentity. Calls on selectedmethod receiver mayuse self.
Do NOT use lexical start-byte order as evaluation order for nestedcalls: arguments/receiver evaluatedbeforeouter.
Represent &&/||/??/conditional evaluation as guarded branches, not unconditionalcalls. Function/callback/class
bodies not executed by declaration: explicit boundary, no flattening. Async marks await boundaries, nottimeline.
Loops are fragments not unrolled. Return/throw terminate sameblock; remainingcode markedunreachable/omitted
withwarning, never shownexecutingafterreturn. Exceptionpaths notassertedwhenunknown.
Unsupportedcomplexcontrol => source-linked boundary/warning; never quietlyfabricatebehavior. Evidence is
static possible paths, NOT observed runtime order. Bounds max200steps/20participants/depth24 failsofttruncated
warning; sourcequotes/labels bounded. Default showAll=false collapses only confidentlyincidental logging;
retain validations,writes,external/unresolvedeffects,branchtests,returns/throws. Hidden counts+Showall control.
This is deterministic heuristic selectivity, NOT claimed semantic consequentialness. Source-links mandatory.

JS adapter src/behavior_js.rs builds the neutral DTO from cached SourceFile, selectedSymbol, measuredcalls.
Root exports modules. Keepgraphschema/storagebackwardscompatible: derivebehaviorfromcachedsourceatrequesttime
inthisfirstslice; graphmodeldownstream/renderingneverimportsJSgrammar. Signature agreed:
behavior::build_sequence(revision:u64, seed:&Symbol, file:&SourceFile, calls:&[CallSite], show_all:bool)->Result<SequenceView>
This dispatcheslanguage to behavior_js::build(...sameargs)->Result<SequenceView>. Unsupportedlanguage error.

Catalog/API workerowns src/store.rs src/http.rs tests/browse_http.rs (notexistingtests unlessneeded).
GET /api/files?revision=N&offset=0&limit=200 -> {revision,items:[{path,language,methodCount}],nextOffset:number|null}.
Optional `q` (1–256 bytes, no control characters) keeps only paths containing it, ASCII case-insensitive, across the whole pinned catalog before paging (#447).
GET /api/methods?revision=N&path=... -> {revision,items:[{symbol:Symbol,consequential:bool,reason:String}],truncated:bool}.
Methods = indexed Function/Method includingnestedmethods labeled bynames/path ranges; no class/moduleasmethod.
Conservativeheuristic only hide accessor/trivialreturnliteral/identifier methods ifdemonstrablytrivial; otherwise
include. If trivialanalysisnotavailable keepall exceptaccessors labeledheuristic. Neverhidepurechecksjust0calls.
POST /api/sequence {seed:String,expectedRevision:u64,showAll:bool(defaultfalse)} -> SequenceView.
Useonlycurrentcachedsnapshot, revisionchecks, rejectunknownfields/invalidshape, sourcepathmustindexed,
no sourceworkspacefilesystemreads orcommands, auth/host/origin/bodyguardsunchanged. missing404 stale409 bad422.
Storeworkerchooseshelpers butmustsnapshotconsistency. BuildsequenceCPUonspawn_blocking/dbwrapper.
No autoindex/modelcalls. Puremethodclick never callsquestions/ACP/Jev.

UIworker owns web/app.js web/index.html web/style.css tests/browse-ui.test.cjs (existingquestionUITestsreadonly).
Implement actual SVG sequence diagram (noCDN/externalfonts/network renderer). Lifelines, directionalcallarrows,
selfcalls, branch/loop/tryfragments visible; preservehierarchy, don'tflattenalternativesintostraighttimeline.
Renderer module web/sequence.js plainbrowserJS window.TrellisSequence={render(container,view,onSource)};
root/backendworker serves /sequence.js CSPself. UIworker ownsrenderer too.
Safe DOMtextContent/createElementNS no userHTML/mermaid stringinjection. Every step keyboard-clickable source
location through existing showSource helper; horizontaloverflowcontaineddiagramnotpage. Sourceaccessiblelist
orbuttons equivalent; smallscreensusable. Explicit showAllsteps toggles serverrequest. Fileexpandloadsinline
methods and preservesotherbranches; filter searches the whole pinned catalog server-side (`q`, #447), showallmethodstoggle. 200filepages
Loadmore control. Concurrencygenerationsperfile+diagram+session+revision prevents stale successANDfailure,
logout/indexrevision clearcatalog/diagram/state. Showclearloading/empty/unsupported states; labelstaticpossible
paths and uncalibrated heuristics. Preserveexisting rawquery/question features andtests. No inferencecalls.
