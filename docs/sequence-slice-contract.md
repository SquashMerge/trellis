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
code or resolution is invented by the browser. Bounds remain 200 steps, 20 participants
and depth 24. If a resolved target cannot fit, its boundary is `nodeLimit`, its target is
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
  200 steps or depth 24 in the whole view is not expanded, the view is truncated and a
  warning names it. Other dispatch, ambiguous (#466, one edge), unresolved, external and
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
