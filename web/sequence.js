"use strict";
// Language-neutral DTO renderer. Geometry never infers calls or control flow.
function renderSequence(container, view, readSource, expandedGroups = new Set(), options = {}) {
  return drawSequence(container, view, readSource, expandedGroups, options, {id:null});
}

function sourceWitness(item) {
  const r = item?.range;
  return typeof item?.path === "string" && !!item.path.trim() && !!r &&
    Number.isSafeInteger(r.startLine) && r.startLine > 0 &&
    Number.isSafeInteger(r.endLine) && r.endLine >= r.startLine &&
    (r.startColumn == null || (Number.isSafeInteger(r.startColumn) && r.startColumn > 0)) &&
    (r.endColumn == null || (Number.isSafeInteger(r.endColumn) && r.endColumn > 0)) &&
    (r.startLine !== r.endLine || r.startColumn == null || r.endColumn == null || r.endColumn >= r.startColumn) &&
    (r.startByte == null || (Number.isSafeInteger(r.startByte) && r.startByte >= 0)) &&
    (r.endByte == null || (Number.isSafeInteger(r.endByte) && r.endByte > (r.startByte ?? -1)));
}
function sequenceTarget(view, step) {
  const evidence = step?.callEvidence, pin = view?.revision;
  if (!["call", "boundary"].includes(step?.kind) || !step.callId || !sourceWitness(step) ||
      typeof pin?.indexGeneration !== "string" || !pin.indexGeneration || !Number.isSafeInteger(pin.indexRevision) ||
      evidence?.boundaryReason !== "none" || evidence.resolution !== "resolved" ||
      !(evidence.freshness === "fresh" && !evidence.projection ||
        evidence.freshness === "possiblyStale" && evidence.projection?.semanticStatus === "olderBasis" &&
        evidence.projection.currentPin?.indexGeneration === pin.indexGeneration &&
        evidence.projection.currentPin?.indexRevision === pin.indexRevision) ||
      !["direct", "constructor"].includes(evidence.dispatch) || !evidence.provenanceId || typeof step.target !== "string") return null;
  const participant = view.participants?.find(item => item.id === step.target);
  const declaration = participant?.declaration;
  return declaration?.id === step.target && sourceWitness(declaration) ? participant : null;
}
function sequenceEvidenceLabel(step, view) {
  if (!["call", "boundary"].includes(step?.kind) || !step.callId) return "Measured source syntax";
  const evidence = step.callEvidence;
  if (!evidence) return "measured syntax; target unverified";
  if (evidence.projection?.semanticStatus === "catchingUp") return "Semantic catching up - native syntax only";
  if (evidence.projection?.semanticStatus === "needsReimport") return "Semantic needs re-import - native syntax only";
  if (evidence.projection?.semanticStatus === "olderBasis") {
    const boundary = evidence.boundaryReason === "none" ? "Resolved at older basis" : "Older basis boundary";
    return `${boundary} - ${evidence.freshness || "possibly stale"}`;
  }
  if (evidence.boundaryReason === "none" && step.hidden && step.incidental && !step.target) return "Resolved declaration - hidden incidental call, no lane";
  if (evidence.boundaryReason === "none" && view && !sequenceTarget(view, step)) return "Target unverified - invalid declaration evidence";
  const labels = {none:"Resolved declaration", external:"External boundary", unresolved:"Unresolved target",
    ambiguous:"Ambiguous target", dispatch:"Dispatch boundary", stale:"Stale evidence", nodeLimit:"Target omitted - participant limit",
    missingEvidence:"Unbound - missing semantic evidence", catchingUp:"Semantic catching up - native syntax only",
    needsReimport:"Semantic needs re-import - native syntax only"};
  return labels[evidence.boundaryReason] || "Target unverified";
}
function sequenceTargetActionLabel(declaration) {
  return ["function", "method"].includes(declaration.kind) ? "Open callee sequence" : "Read declaration source";
}
// The evidence tier of a view (#522): the producer whose fresh bindings built its lanes,
// or why it shows syntax only. Server text, never inferred here.
function sequenceEvidenceSummary(view) {
  const semantic = view?.semantic;
  const documents = semantic?.documents || [];
  const pending = documents.some(item => item.projection?.semanticStatus === "catchingUp");
  const reimport = documents.some(item => item.projection?.semanticStatus === "needsReimport");
  const older = documents.some(item => item.projection?.semanticStatus === "olderBasis");
  if (pending || reimport || older) return [pending && "Semantic catching up", reimport && "Semantic needs re-import", older && "Older semantic basis", semantic?.producerId].filter(Boolean).join(" · ");
  if (semantic?.tier === "semantic" && typeof semantic.producerId === "string" && semantic.producerId)
    return `Semantic bindings · ${semantic.producerId}`;
  return typeof semantic?.reason === "string" && semantic.reason ? `Syntax only · ${semantic.reason}` : "Syntax only";
}
// The captured basis of a call's selected binding, as MCP reports it (#522).
function sequenceBasisLabel(evidence) {
  const projection = evidence?.projection;
  const pin = projection?.semanticBasisPin;
  const projected = projection ? `${{olderBasis:"older basis", needsReimport:"needs re-import"}[projection.semanticStatus] || "catching up"}${pin ? ` ${pin.indexGeneration}:${pin.indexRevision}` : ""} · native ${projection.basisNativeRevisionId}` : "";
  const basis = evidence?.semanticBasis;
  if (!basis || typeof basis.producerId !== "string") return projected;
  return [projected, `basis ${basis.producerId}${typeof basis.producerVersion === "string" ? " " + basis.producerVersion : ""}`].filter(Boolean).join(" · ");
}
// A call that may return early carries its folded `?` and guards (#542); a chain group
// shows it when one of its calls does. An `if` or `match` carries the guard after it (#558):
// its own frame shows how the path ends. Server fields, never inferred from labels.
function sequenceEarlyReturn(step) {
  const folded = item => Array.isArray(item?.folded) && item.folded.length > 0;
  // A branch keeps its folded steps when the depth bound redraws it as a boundary: no call ID.
  if (folded(step) && !step.callId) return " · may exit early";
  return folded(step) || step?.kind === "group" && (step.children || []).some(folded) ? " · may return early" : "";
}
function sequenceEntry(step) {
  const children = step?.children || [];
  return step?.kind === "group" && !step.alternate?.length && children.length &&
    children.every(child => child.kind === "call" && !child.hidden && !child.children?.length && !child.alternate?.length && sourceWitness(child) && child.path === step.path) &&
    sourceWitness(step) ? children[0] : null;
}
// Exact DTO prose is abbreviated for display only. These mappings never
// equate guards, alter their source ranges, or change the control tree.
const compactLabels = new Map([
  ["only if the preceding path continues normally", "continues"],
  ["?: continue only on success; otherwise early return", "success / early return"],
  ["?: early return on residual; conversion not expanded", "early return"],
  ["only if iteration continues normally", "iteration continues"],
  ["arm body only if guard is true", "guard is true"],
  ["RHS only if left is true", "left is true"],
  ["RHS only if left is false", "left is false"],
  ["RHS only if left is truthy", "left is truthy"],
  ["RHS only if left is falsy", "left is falsy"],
  ["RHS only if left is nullish", "left is nullish"],
  ["match: mutually exclusive arms; first matching pattern with true guard", "match · first matching arm"],
  ["body only if condition is true / next item exists (loop: each iteration)", "condition true / next item"],
  ["catch: only if an exception reaches this handler", "catch · exception reaches handler"],
  ["for_expression: possible iterations, not unrolled; exit/termination unknown", "for · possible iterations"],
  ["while_expression: possible iterations, not unrolled; exit/termination unknown", "while · possible iterations"],
  ["loop_expression: possible iterations, not unrolled; exit/termination unknown", "loop · termination unknown"],
  ["possible iterations, not unrolled; protocol and termination unresolved", "loop · protocol / termination unresolved"],
  ["Definition boundary: decorators/defaults/annotations and binding effects not expanded; callable body does not execute here.", "definition · effects not expanded"],
  ["Class definition boundary: bases, decorators, metaclass and class-body execution not expanded; method bodies do not execute here.", "class definition · effects not expanded"],
  ["Nested callable boundary: body does not execute at definition; captures are not expanded.", "nested callable · captures not expanded"],
  ["Lambda boundary: body does not execute at definition; captures are not expanded.", "lambda · captures not expanded"],
  ["Nested declaration boundary: bodies are not executed here; implicit initialization is not expanded.", "nested declaration · initialization not expanded"],
  ["Nested function/callback/class boundary: bodies are not executed here; class definition effects (base, computed keys, static initialization) are not expanded.", "nested definition · effects not expanded"],
  ["Attribute lookup: descriptor/__getattribute__ effects and type unresolved.", "attribute lookup · effects/type unresolved"],
  ["Subscription: implicit __getitem__ effects unresolved.", "subscription · effects unresolved"],
  ["Comprehension/generator boundary: iteration, filters and deferred execution not expanded.", "comprehension / generator · not expanded"],
  ["Context-manager/match boundary: implicit protocol and conditional execution not expanded.", "context manager / match · not expanded"],
  ["Async iteration boundary: protocol and suspension not expanded.", "async iteration · protocol not expanded"],
  ["Argument unpacking boundary: iteration/mapping protocol not expanded.", "argument unpacking · protocol not expanded"],
  ["Formatted string boundary: interpolation and formatting protocol not expanded.", "formatted string · protocol not expanded"],
  ["Yield boundary: generator suspension, send/throw/close and yield-from protocol not expanded.", "yield · protocol not expanded"],
  ["Try boundary: exception selection, else and finally execution/transfers not expanded.", "try · paths not expanded"],
  ["Assignment target boundary: unpacking/setter effects not expanded.", "assignment target · effects not expanded"],
  ["Call unpacking boundary: positional/keyword evaluation and iteration effects not expanded.", "call unpacking · effects not expanded"],
  ["Augmented assignment boundary: target read/write and overloaded operator effects not expanded.", "augmented assignment · effects not expanded"],
  ["Chained assignment boundary: target write sequence not expanded.", "chained assignment · writes not expanded"],
  ["Annotation-only target boundary: target address effects not expanded; local annotation is not evaluated.", "annotation target · effects not expanded"],
  ["Chained comparison boundary: later operands are conditional; not expanded.", "chained comparison · conditional / not expanded"],
  ["Loop-else boundary: break versus exhaustion paths not expanded.", "loop else · paths not expanded"],
  ["Async block boundary: future construction does not execute its body; captures are not expanded.", "async block · captures not expanded"],
  ["Opaque macro/unsafe/const boundary: expansion, effects and transfers unknown.", "macro / unsafe / const · effects unknown"],
  ["Unsupported control transfer: remainder of this path omitted.", "unsupported transfer · path omitted"],
  ["Unsupported complex control: possible behavior remains unknown; body not expanded.", "unsupported control · behavior unknown"],
  ["Destructuring binding: computed keys/defaults/implicit reads are not expanded (RHS evaluated first).", "destructuring binding · reads not expanded"],
  ["Destructuring assignment: computed keys/defaults/writes are not expanded (RHS evaluated first).", "destructuring assignment · writes not expanded"],
  ["Loop destructuring binding: per-iteration reads/defaults are not expanded.", "loop destructuring · reads not expanded"],
  ["Parameter defaults/destructuring may execute before the body; behavior not expanded.", "parameter defaults / destructuring · not expanded"],
  ["await: possible suspension/resumption; await protocol unresolved", "await · protocol unresolved"],
  ["await: suspension/resumption boundary, timing unknown", "await · timing unknown"],
  ["iteration target write: implicit setters/unpacking unresolved", "iteration write · setters/unpacking unresolved"]
]);
const analysisKinds = new Set(["effect", "return", "throw", "exit", "await", "note", "boundary", "definition"]);
function compactAnalysis(step) {
  const label = step.label || step.kind;
  if (compactLabels.has(label)) return compactLabels.get(label);
  // Abbreviate DTO evidence categories, not source expressions. Never parse an
  // assignment target or infer a return value from the original source label.
  if (step.kind === "effect") {
    if (label.startsWith("bind name: ")) return `bind ${label.slice("bind name: ".length)}`;
    if (label.startsWith("write: ")) return label.endsWith(" (implicit setters/unpacking unresolved)")
      ? "write · setters/unpacking unresolved" : "write";
    return "effect · inspect details";
  }
  if (step.kind === "return" || step.kind === "throw") return step.kind;
  if (step.kind === "definition") return label.startsWith("define ") ? label : "definition · inspect details";
  return label;
}
// Compact rows stay near one screen wide; the full label is in the inspector (#558).
const SEQUENCE_LABEL_LIMIT = 72;
// The row of an arrow, in both renderers (#558): its number and the exact target's name. The
// lane names the type. The call's source text and evidence are in the inspector and Show all.
function sequenceArrowLabel(ordinal, target, collapsed, early, limit = SEQUENCE_LABEL_LIMIT) {
  const marker = `[${String(ordinal).padStart(3, "0")}]`, name = target.declaration.name || target.label;
  const suffix = `${collapsed ? ` +${collapsed} calls collapsed (entry preview)` : ""}${early}`;
  const room = Math.max(12, limit - marker.length - 1 - suffix.length);
  return `${marker} ${name.length > room ? `${name.slice(0, room - 1)}…` : name}${suffix}`;
}
// The short text of a step (#558). The exact label stays in the DTO and the inspector.
function sequenceCompactLabel(step) {
  const label = step.label || step.kind;
  return analysisKinds.has(step.kind) ? compactAnalysis(step) : compactLabels.get(label) || label;
}
// The column a participant is drawn in (#558): its type or module. A syntax hint, which has
// none, stays its own column. The participant keeps the exact identity.
function sequenceColumn(participant) {
  const column = participant?.column;
  return typeof column?.id === "string" && column.id && typeof column.label === "string" && column.label
    ? column : {id:participant?.id, label:participant?.label};
}
// What a column is, in a word (#716), so a type `Session` and a module `session` never look
// alike. The server names a module's column `module:…` and a Rust type's `type:…`
// (src/behavior_columns.rs); its other columns are classes. A participant without a column
// (a syntax hint, or the seed of a syntax-only view) is neither, so it has no kind.
function sequenceColumnKind(participant) {
  const column = participant?.column, id = column?.id;
  if (typeof id !== "string" || !id || typeof column.label !== "string" || !column.label) return "";
  return id.startsWith("module:") ? "module" : id.startsWith("type:") ? "type" : "class";
}
// A lane's name as lines (#716): a long name breaks after `_`, `::`, `.`, `/` or `-`, or
// before a capital, and never inside a word, so lanes stay narrow and no name is clipped.
const SEQUENCE_LANE_LINE = 10;
function sequenceLaneLines(name, limit = SEQUENCE_LANE_LINE) {
  const lines = [];
  for (const part of String(name).split(/(?<=_|::|[./-])(?=.)|(?<=[a-z0-9])(?=[A-Z])/)) {
    if (lines.length && (lines[lines.length - 1] + part).length <= limit) lines[lines.length - 1] += part;
    else lines.push(part);
  }
  return lines;
}
// A lane's header, top to bottom (#716): its kind as a UML stereotype, then its name.
function sequenceLaneHeader(participant, name, limit) {
  const kind = sequenceColumnKind(participant);
  return [...(kind ? [`«${kind}»`] : []), ...sequenceLaneLines(name, limit)];
}
// Rust's standard library crates, as a producer names their package. Other languages' producers
// do not name one, so their external calls stay "external".
const STANDARD_LIBRARIES = new Set(["std", "core", "alloc"]);
// Calls-first is a display projection of admitted evidence, never a new execution tree.
// Membership keeps source controls and both adapters bound to the exact original objects.
const deferredSequenceBoundaries = new Set([
  "Nested callable boundary: body does not execute at definition; captures are not expanded.",
  "Lambda boundary: body does not execute at definition; captures are not expanded."
]);
function sequenceUncertainty(step) {
  if (step.kind === "effect") return step.label === "iteration target write: implicit setters/unpacking unresolved" ||
    step.label?.startsWith("write: ") && step.label.endsWith(" (implicit setters/unpacking unresolved)");
  if (step.kind === "try") return step.label === "try; exception behavior unknown";
  if (step.kind !== "loop") return false;
  return [
    "possible iterations, not unrolled; protocol and termination unresolved",
    "enhanced for: possible iterations; iterable evaluated once; implicit iterator/array protocol not expanded",
    "loop: possible iterations, not unrolled; termination and transfers unknown",
    "for_expression: possible iterations, not unrolled; exit/termination unknown",
    "while_expression: possible iterations, not unrolled; exit/termination unknown",
    "loop_expression: possible iterations, not unrolled; exit/termination unknown"
  ].includes(step.label);
}
function sequenceDisplay(view, options = {}) {
  const visible = new Set(), exits = new Set();
  const visit = steps => {
    let shows = false, transfers = false;
    for (const step of steps || []) {
      if (step.hidden) continue;
      const children = visit(step.children), alternate = visit(step.alternate);
      const transfer = ["return", "throw", "exit"].includes(step.kind) || children.transfers || alternate.transfers;
      const structural = ["branch", "loop", "try"].includes(step.kind);
      const context = step.kind === "await" || sequenceUncertainty(step) ||
        step.kind === "boundary" && !deferredSequenceBoundaries.has(step.label);
      const show = options.showDetails || step.kind === "call" || !!step.callId || context ||
        children.shows || alternate.shows || structural && transfer;
      if (show) visible.add(step);
      if (structural && transfer) exits.add(step);
      shows ||= show; transfers ||= transfer;
    }
    return {shows, transfers};
  };
  visit(view?.steps);
  const earlyReturn = step => sequenceEarlyReturn(step) || (!options.showDetails && exits.has(step) ? " · may exit early" : "");
  // A view with no shown call keeps calls-first: its context markers and empty state (#594).
  const calls = [...visible].some(step => step.kind === "call" || step.callId);
  return calls && sequenceInteractive(view, options) ? sequenceInteractions(view, options, earlyReturn) : {visible, earlyReturn};
}
// The Interactions view (#636) is the default for an enriched view. A syntax-only view has
// no columns (#558), so it keeps the calls-first display, as does Show all steps.
function sequenceInteractive(view, options = {}) {
  return !!options.interactions && !options.showDetails && view?.semantic?.tier === "semantic";
}
// The column a step runs in: its lane's, or the seed's for the seed's own steps.
function sequenceStepColumn(view, step) {
  const participants = view?.participants || [];
  const owner = step?.lane != null && participants.find(p => p.id === step.lane);
  return sequenceColumn(owner || participants.find(p => p.id === view?.seed?.id) || {id:view?.seed?.id, label:view?.seed?.name});
}
// Interactions (#636, owner review on #638) are a display projection of the same admitted DTO,
// as calls-first is. They show only interactions between entities:
// - an arrow is a measured call whose exact target is drawn in another column than its own; it
//   keeps source order, its callee's body (nested cross-column chains) and a return after it;
// - a loop that encloses an arrow is a UML loop frame around its arrows, with no row of its own
//   and no row number; it says " · may exit early" when its own frame draws an exit;
// - every other step draws nothing: internal steps, nested callables, calls without a target and
//   any branch, try, chain or same-column call around an arrow (`through`: its arrows draw in
//   its place). `hiddenSteps` counts the shown DTO steps that draw nothing; Show all has them.
// - at most SEQUENCE_INTERACTION_LIMIT arrows are drawn, in source order (#663). The server
//   sends Interactions candidates before its visible step bound, which counts internal steps;
//   this bound counts arrows only, and `omittedInteractions` counts the arrows past it.
const SEQUENCE_INTERACTION_LIMIT = 200;
function sequenceInteractions(view, options, earlyReturn) {
  const visible = new Set(), through = new Set(), loops = new Set(), returns = new Set(), holds = new Set();
  const nested = step => [...(step.children || []), ...(step.alternate || [])];
  const crosses = step => {
    if (step.kind !== "call") return false;
    const target = sequenceTarget(view, step);
    return !!target && sequenceColumn(target).id !== sequenceStepColumn(view, step).id;
  };
  let arrows = 0;
  const scan = step => {
    if (step.hidden) return false;
    let any = crosses(step) && ++arrows <= SEQUENCE_INTERACTION_LIMIT;
    if (any) returns.add(step);
    for (const child of nested(step)) any = scan(child) || any;
    if (any) holds.add(step);
    return any;
  };
  for (const step of view?.steps || []) scan(step);
  let hiddenSteps = 0;
  // An omitted arrow is an interaction, not an internal step.
  const fold = step => { if (step.hidden) return; if (!crosses(step)) hiddenSteps++; for (const child of nested(step)) fold(child); };
  const place = steps => {
    for (const step of steps || []) {
      if (step.hidden) continue;
      if (returns.has(step)) visible.add(step);
      else if (step.kind === "loop" && holds.has(step)) { visible.add(step); loops.add(step); }
      else if (holds.has(step)) { through.add(step); hiddenSteps++; }
      else { fold(step); continue; }
      place(step.children); place(step.alternate);
    }
  };
  place(view?.steps);
  // A loop may exit early when its frame draws an exit (calls-first's mark) or a step in it
  // carries a folded `?` or guard (#542, #558).
  const exits = step => !step.hidden && (!!sequenceEarlyReturn(step) || nested(step).some(exits));
  // The frame's tag says "loop"; its guard says only what qualifies it, if anything.
  const loopLabel = step => earlyReturn(step) || exits(step) ? "may exit early" : "";
  const omittedInteractions = Math.max(0, arrows - SEQUENCE_INTERACTION_LIMIT);
  return {interactions:true, visible, through, loops, returns, hiddenSteps, omittedInteractions, loopLabel, earlyReturn};
}
// What to say when the admitted view has no displayed calls (#594).
// Counts come from the server; the reasons are the steps' own.
function sequenceEmptySummary(view, options = {}) {
  const steps = view?.steps || [];
  if (options.showDetails) return "";
  const display = sequenceDisplay(view, options);
  if (display.interactions) {
    // Interactions (#636): no call crosses a column. A call without an exact target proves
    // nothing about where it goes, so it is counted, never said to stay.
    if (display.returns.size) return "";
    const calls = [...sequenceDisplay(view, {...options, interactions:false}).visible].filter(step => step.kind === "call" || step.callId);
    const untargeted = calls.filter(step => !sequenceTarget(view, step)).length, column = sequenceStepColumn(view, null).label;
    if (!untargeted) return `All calls stay within ${column}.`;
    if (untargeted === calls.length) return `${untargeted === 1 ? "The one call has no exact target" : `None of the ${untargeted} calls has an exact target`}, so no arrow is drawn between columns.`;
    return `All resolved calls stay within ${column}; ${untargeted} ${untargeted === 1 ? "call has" : "calls have"} no exact target.`;
  } else if ([...display.visible].some(step => step.kind === "call" || step.callId)) return "";
  else if (steps.some(step => !step.hidden)) return "No measured calls in the admitted steps. Non-call behavior may be hidden; use Show all steps to inspect it.";
  if (!steps.length) return "No measured calls in the admitted steps. This does not mean no behavior; inspect source or Show all steps.";
  const hidden = [];
  const walk = items => { for (const step of items) { if (step.incidental) hidden.push(step); walk(step.children || []); walk(step.alternate || []); } };
  walk(steps);
  const external = step => step.incidental === "external";
  const standard = step => {
    const pkg = step.callEvidence?.externalPackage;
    return external(step) && pkg?.manager === "cargo" && STANDARD_LIBRARIES.has(pkg.name);
  };
  const kind = hidden.length && hidden.every(standard) ? "standard-library" : hidden.length && hidden.every(external) ? "external" : "incidental";
  return `Only ${kind} calls (${view.hiddenSteps} hidden).`;
}
// Replaces a diagram with its empty state. `onShow`, when given, is a real control.
function renderSequenceEmpty(container, summary, onShow, preserveDiagram = false) {
  const note = document.createElement("p");
  note.className = "sequence-empty"; note.textContent = `${summary} `;
  if (onShow) {
    const show = document.createElement("button");
    show.type = "button"; show.textContent = "Show all steps";
    show.addEventListener("click", onShow);
    note.append(show);
  }
  if (preserveDiagram) container.prepend(note); else container.replaceChildren(note);
}
const sequenceRenders = new WeakMap();
function beginSequenceRender(container) {
  const owner = {}; sequenceRenders.set(container, owner);
  return () => sequenceRenders.get(container) === owner;
}
function drawSequence(container, view, readSource, expandedGroups, options, selection) {
  const current = options.isCurrent, active = beginSequenceRender(container);
  options = {...options, isCurrent:() => active() && (!current || current())};
  const groupControls = new Map(), rowControls = [], displayProjection = sequenceDisplay(view, options);
  const ns = "http://www.w3.org/2000/svg";
  const make = (tag, attrs = {}, text) => {
    const n = document.createElementNS(ns, tag);
    for (const [key, value] of Object.entries(attrs)) n.setAttribute(key, String(value));
    if (text !== undefined) n.textContent = text;
    return n;
  };
  container.replaceChildren();
  const allParticipants = view.participants || [];
  const participantById = new Map(allParticipants.map(p => [p.id, p]));
  // A collapsed chain previews its first measured call, never a synthesized
  // call to an inferred common receiver. Later calls can return other types.
  const entryCall = sequenceEntry;
  const visibleTargets = new Set([view.seed.id]), targetProofs = new Map();
  const pending = (view.steps || []).map(step => ({step, depth:0})).reverse();
  let visibleDepth = 0;
  const nestedPending = (step, depth) => [...(step.alternate || []).slice().reverse(), ...(step.children || []).slice().reverse()].map(step => ({step, depth}));
  while (pending.length) {
    const {step, depth} = pending.pop();
    if (!displayProjection.visible.has(step)) {
      if (displayProjection.through?.has(step)) pending.push(...nestedPending(step, depth));
      continue;
    }
    visibleDepth = Math.max(visibleDepth, depth);
    if (step.kind === "group" && !expandedGroups.has(step.id)) {
      const entry = entryCall(step);
      const target = sequenceTarget(view, entry);
      if (target) { visibleTargets.add(target.id); targetProofs.set(target.id, entry); }
      continue;
    }
    const target = sequenceTarget(view, step);
    if (target) { visibleTargets.add(target.id); targetProofs.set(target.id, step); }
    pending.push(...nestedPending(step, depth + 1));
  }
  // Hidden chain members and replaced candidate hints must not leave ghost lanes.
  // One column per type or module (#558). A column with one method keeps its open action.
  const columns = new Map();
  for (const id of visibleTargets) {
    const participant = participantById.get(id);
    if (!participant) continue;
    const column = sequenceColumn(participant), known = columns.get(column.id);
    if (known) known.members.push(participant);
    else columns.set(column.id, {...participant, id:column.id, label:column.label, members:[participant]});
  }
  const participants = [...columns.values()];
  // Keep a readable guard column even for deeply nested input. The canvas scrolls.
  const width = Math.max(720, participants.length * 210 + 80, visibleDepth * 10 + 400);
  const svg = make("svg", {viewBox:`0 0 ${width} 200`, width, role:"group", "aria-label":"Static possible paths; resolved declarations where evidenced, not a runtime trace"});
  svg.append(make("title", {}, `${view.seed.name}: measured source steps, not a runtime trace${view.revision?.indexGeneration ? ` · revision ${view.revision.indexGeneration.slice(0, 8)}:${view.revision.indexRevision}` : ""}`));
  const positions = new Map(participants.map((p, i) => [p.id, 130 + i * 210]));
  const columnX = id => positions.get(sequenceColumn(participantById.get(id)).id);
  const origin = columnX(view.seed.id) || 130;
  // Steps of an expanded callee body run on that callee's lane (#522); the seed's own
  // steps carry no lane.
  const laneX = step => (step?.lane != null && columnX(step.lane)) || origin;
  const lines = make("g", {class:"sequence-lifelines"}); svg.append(lines);
  // Interactions (#636): an activation spans a cross-column call's arrow to its return.
  const activations = make("g", {class:"sequence-activations"}); svg.append(activations);
  const arrowTops = new Map();
  const kindLabels = {externalCandidate:"External type · candidate", unresolvedReceiver:"Receiver · type unresolved", unresolvedCallee:"Callee · unresolved", boundary:"Unknown target", builtin:"Built-in name", import:"Imported binding", receiver:"Receiver hint", internal:"Indexed target", method:"Selected method"};
  const compactKinds = {externalCandidate:"candidate", unresolvedReceiver:"unresolved", unresolvedCallee:"unresolved", boundary:"unknown", builtin:"source hint", import:"source hint", receiver:"source hint", internal:"indexed", method:"selected"};
  const shorten = (text, limit) => text.length > limit ? `${text.slice(0, limit-1)}…` : text;
  // A header names its kind and wraps its name inside its box, never clipping it (#716). The
  // tallest header sets how far down the lifelines and rows start.
  const headers = new Map(participants.map(p => [p.id, sequenceLaneHeader(p, p.label, 22).flatMap(line => line.match(/[\s\S]{1,22}/gu) || [""])]));
  const head = Math.max(0, ...[...headers.values()].map(lines => lines.length - 1)) * 16;
  for (const p of participants) {
    const x = positions.get(p.id), header = headers.get(p.id);
    svg.append(make("rect", {x:x-95, y:10, width:190, height:44+head, class:"participant"}));
    const text = make("text", {x, y:28, "text-anchor":"middle", class:"sequence-participant-label"}, header.length === 1 ? header[0] : undefined);
    if (header.length > 1) for (const [i, line] of header.entries()) text.append(make("tspan", {x, dy:i ? 16 : 0}, line));
    text.append(make("title", {}, p.label)); svg.append(text);
    if (p.members.length === 1 && p.members[0].declaration && typeof options.onOpenTarget === "function") {
      const member = p.members[0], proof = targetProofs.get(member.id);
      if (proof) {
        text.setAttribute("role", "button"); text.setAttribute("tabindex", "0");
        text.setAttribute("aria-label", `${sequenceTargetActionLabel(member.declaration)}: ${member.declaration.name}`);
        text.setAttribute("data-trellis-target-id", member.id);
        const open = () => { if ((!options.isCurrent || options.isCurrent()) && sequenceTarget(view, proof) === member) options.onOpenTarget(member.declaration, proof); };
        text.addEventListener("click", open);
        text.addEventListener("keydown", event => { if (event.key === "Enter" || event.key === " ") { event.preventDefault(); open(); } });
      }
    }
    const meta = make("text", {x, y:44+head, "text-anchor":"middle", class:"sequence-meta"}, (options.showDetails ? kindLabels[p.kind] : compactKinds[p.kind]) || p.kind);
    meta.append(make("title", {}, [kindLabels[p.kind] || p.kind, p.identification].filter(Boolean).join(" — "))); svg.append(meta);
  }
  // These are evidence categories, never runtime certainty or numeric confidence.
  const provenance = step => ({key:step.callEvidence?.boundaryReason || "syntax", label:sequenceEvidenceLabel(step, view)});
  const tooltip = step => {
    const range = step.range;
    const location = step.path && range ? `${step.path}:${range.startLine}${range.startColumn != null ? ":" + range.startColumn : ""}–${range.endLine}${range.endColumn != null ? ":" + range.endColumn : ""}` : "";
    return [`${step.kind}: ${step.label}`, location, sequenceEvidenceLabel(step, view)].filter(Boolean).join(" · ");
  };
  const wrap = (label, limit) => {
    const output = [];
    for (const paragraph of label.split("\n")) {
      let rest = paragraph;
      while (rest.length > limit) {
        const space = rest.lastIndexOf(" ", limit);
        const end = space > limit / 2 ? space : limit;
        output.push(rest.slice(0, end)); rest = rest.slice(end).replace(/^ /, "");
      }
      output.push(rest);
    }
    return output;
  };
  const quietGuard = step => !options.showDetails && step.kind === "branch" &&
    step.label === "only if the preceding path continues normally" && !step.alternate?.length;
  if (!options.showDetails) svg.append(make("text", {x:24, y:73+head, class:"sequence-meta"}, displayProjection.interactions
    ? "Interactions between columns · select for details" : "Compact labels · select for details"));
  let y = (options.showDetails ? 78 : 98) + head;
  let rows = 0; // Each row's number, as Pele counts them.
  function sourceControl(group, step, x, top, display = step, evidence = step, operator = null) {
    const ordinal = ++rows;
    const interactive = sourceWitness(step) && sourceWitness(evidence) && step.path === evidence.path &&
      (!options.isCurrent || options.isCurrent()) &&
      (typeof options.onSelect === "function" || typeof readSource === "function");
    const call = ["call", "boundary"].includes(evidence.kind) && evidence.callId, proof = call ? provenance(evidence) : null;
    const fullLabel = tooltip(step) + displayProjection.earlyReturn(step) + (evidence !== step ? ` · Entry preview only: ${tooltip(evidence)}` : "");
    const control = make("g", {class:"sequence-source", "data-source-step-id":step.id,
      ...(evidence.callId ? {"data-trellis-call-id":evidence.callId} : {}),
      ...(interactive ? {role:"button", tabindex:0, "aria-label":`${options.onSelect ? "Inspect" : "Read source"}: ${fullLabel}`, "aria-pressed":String(selection.id === step.id)} : {})});
    // Compact controls remain individual, ordered fragments. No matching guard
    // text is treated as equivalent, merged, or removed.
    const isControl = ["branch", "loop", "try"].includes(display.kind);
    let label = display.label || display.kind;
    if (["call", "boundary"].includes(evidence.kind) && evidence.callEvidence) label = `${sequenceEvidenceLabel(evidence, view)} · ${label}`;
    if (options.showDetails) {
      if (!operator && display.kind !== "group" && display.kind !== "call chain" && display.kind !== "call") label = `${display.kind} · ${label}`;
    } else {
      label = analysisKinds.has(display.kind) ? compactAnalysis(display) : (compactLabels.get(label) || label);
      if (analysisKinds.has(display.kind) && evidence.callEvidence) label = `${sequenceEvidenceLabel(evidence, view)} · ${label}`;
      if (quietGuard(display)) label = "[if prior path continues]";
      if (display.kind === "call") label = label.replace(/^call(?:\s*·\s*|\s+|:\s*)/i, "");
    }
    const tabWidth = operator ? Math.max(80, operator.length * 8 + 28) : 0;
    const labelX = operator ? x + tabWidth + 12 : x + 5;
    const capacity = Math.max(24, Math.floor((width-labelX-(proof && options.showDetails ? 145 : 35))/7.8));
    const early = displayProjection.earlyReturn(step);
    const limit = Math.min(SEQUENCE_LABEL_LIMIT, capacity);
    if (!options.showDetails) label = shorten(label.replace(/\s+/g, " "), isControl ? Math.min(56, capacity - (operator ? 2 : 0)) : Math.max(12, limit - early.length));
    label += early;
    // Brackets are a presentation frame, not an inferred or rewritten predicate.
    if (operator) label = `[${label}]`;
    let labels = options.showDetails ? wrap(label, capacity) : [label];
    if (display.kind === "call chain" && !options.showDetails) {
      const suffix = ` · +${step.children.length-1} calls collapsed (entry preview)${early}`;
      // Keep the collapsed count and entry-only claim visible even when the
      // measured name is long. At deep indentation the annotation can wrap.
      const proven = evidence.callEvidence ? `${sequenceEvidenceLabel(evidence, view)} · ` : "";
      labels = wrap(proven + shorten(evidence.label, Math.max(12, limit-proven.length-suffix.length)) + suffix, capacity);
    }
    const target = sequenceTarget(view, evidence), arrow = target && evidence.kind === "call" && columnX(target.id);
    if (arrow && !options.showDetails) labels = [sequenceArrowLabel(ordinal, target, display.kind === "call chain" ? step.children.length - 1 : 0, early, limit)];
    const extra = (labels.length-1)*16;
    control.append(make("rect", {x, y:top-(operator ? 19 : 15), width:Math.max(1,width-x-24), height:(operator ? 30 : 24)+extra, fill:"transparent", class:"sequence-row-hit"}));
    if (operator) {
      // UML-style clipped-corner tab. The whole header selects the original step.
      control.append(make("path", {d:`M ${x} ${top-19} H ${x+tabWidth} V ${top+1} L ${x+tabWidth-10} ${top+11} H ${x} Z`, class:"sequence-fragment-tab"}));
      control.append(make("text", {x:x+12, y:top, class:"sequence-fragment-operator"}, operator));
    }
    const text = make("text", {x:labelX, y:top, class:isControl ? "sequence-control-label" : "sequence-label"});
    for (const [i, line] of labels.entries()) text.append(make("tspan", {x:labelX, dy:i ? 16 : 0}, line));
    text.append(make("title", {}, fullLabel)); control.append(text);
    if (proof && options.showDetails) {
      const badge = make("text", {x:width-30, y:top, "text-anchor":"end", class:"sequence-provenance", "data-provenance":proof.key}, proof.label);
      badge.append(make("title", {}, tooltip(evidence))); control.append(badge);
    }
    if (interactive) {
      rowControls.push({control, id:step.id});
      const select = () => {
        if (!sourceWitness(step) || !sourceWitness(evidence) || step.path !== evidence.path || (options.isCurrent && !options.isCurrent())) return;
        selection.id = step.id;
        for (const row of rowControls) row.control.setAttribute("aria-pressed", String(row.id === selection.id));
        if (typeof options.onSelect === "function") options.onSelect(step);
        else readSource(step);
      };
      control.addEventListener("click", select);
      control.addEventListener("keydown", event => { if (event.key === "Enter" || event.key === " ") { event.preventDefault(); select(); } });
    }
    if (arrow) {
      const end = columnX(target.id), arrowY = top + extra + 10, from = laneX(evidence);
      arrowTops.set(step, arrowY);
      const d = end === from ? `M ${from} ${arrowY} h 48 v 18 h -48` : `M ${from} ${arrowY} H ${end}`;
      group.append(make("path", {d, class:"sequence-arrow", fill:"none", "data-trellis-call-id":evidence.callId, "data-trellis-target-id":target.id}));
      const direction = end === from ? -1 : end > from ? 1 : -1, headY = arrowY + (end === from ? 18 : 0);
      group.append(make("path", {d:`M ${end-direction*7} ${headY-4} L ${end} ${headY} L ${end-direction*7} ${headY+4}`, class:"sequence-arrow", fill:"none"}));
    }
    group.append(control);
    return extra + (target && columnX(target.id) === laneX(evidence) ? 20 : 0);
  }
  // The return of a cross-column call, after its expanded body: presentation, not an observed return.
  function drawReturn(group, step) {
    const target = sequenceTarget(view, step), from = target && columnX(target.id), to = laneX(step), top = arrowTops.get(step);
    if (from == null || top == null) return;
    const returnY = y - 6;
    activations.append(make("rect", {x:from-5, y:top, width:10, height:Math.max(8, returnY-top), rx:2, class:"sequence-activation", "data-trellis-target-id":target.id}));
    group.append(make("path", {d:`M ${from} ${returnY} H ${to}`, class:"sequence-arrow sequence-return", fill:"none", "data-trellis-return-step-id":step.id}));
    const direction = to > from ? 1 : -1;
    group.append(make("path", {d:`M ${to-direction*7} ${returnY-4} L ${to} ${returnY} L ${to-direction*7} ${returnY+4}`, class:"sequence-arrow sequence-return", fill:"none"}));
    y += 18;
  }
  function draw(steps, parent, depth) {
    for (const step of steps || []) {
      if (!displayProjection.visible.has(step)) {
        if (displayProjection.through?.has(step)) { draw(step.children, parent, depth); draw(step.alternate, parent, depth); }
        continue;
      }
      const group = make("g", {"data-kind":step.kind, "data-step-id":step.id}); parent.append(group);
      if (displayProjection.loops?.has(step)) {
        // Interactions (#636): a UML loop frame around its arrows. It is not a row: no number.
        const start = y, x = 18 + depth * 10, label = displayProjection.loopLabel(step);
        group.setAttribute("data-presentation", "interaction-loop");
        const box = make("rect", {x, y:start-19, width:width-x-18, height:34, class:"sequence-fragment"}); group.append(box);
        const tabWidth = 80;
        group.append(make("path", {d:`M ${x} ${start-19} H ${x+tabWidth} V ${start+1} L ${x+tabWidth-10} ${start+11} H ${x} Z`, class:"sequence-fragment-tab"}));
        group.append(make("text", {x:x+12, y:start, class:"sequence-fragment-operator"}, "loop"));
        if (label) group.append(make("text", {x:x+tabWidth+12, y:start, class:"sequence-control-label"}, `[${label}]`));
        y += 34;
        draw(step.children, group, depth+1); draw(step.alternate, group, depth+1);
        y += 4; box.setAttribute("height", y-start+8); y += 14;
        continue;
      }
      if (step.kind === "group") {
        const start = y, x = 18 + depth * 10, expanded = expandedGroups.has(step.id);
        const box = make("rect", {x, y:y-19, width:width-x-18, height:34, class:"sequence-fragment"}); group.append(box);
        const control = make("g", {role:"button", tabindex:0, "aria-expanded":String(expanded), "aria-label":`${expanded ? "Collapse" : "Expand"} call chain: ${step.label}`, class:"sequence-group-control"});
        control.append(make("rect", {x:x+4, y:y-15, width:88, height:24, class:"sequence-note"}));
        control.append(make("text", {x:x+10, y}, `${expanded ? "▾ Collapse" : "▸ Expand"}`));
        const toggle = () => {
          if (!options.isCurrent()) return;
          const top = container.scrollTop, left = container.scrollLeft;
          if (expandedGroups.has(step.id)) expandedGroups.delete(step.id); else expandedGroups.add(step.id);
          const controls = drawSequence(container, view, readSource, expandedGroups, {...options,isCurrent:current}, selection);
          controls.get(step.id)?.focus?.({preventScroll:true});
          container.scrollTop = top; container.scrollLeft = left;
        };
        control.addEventListener("click", toggle);
        control.addEventListener("keydown", event => { if (event.key === "Enter" || event.key === " ") { event.preventDefault(); toggle(); } });
        groupControls.set(step.id, control); group.append(control);
        const entry = !expanded && entryCall(step);
        if (entry) {
          const preview = make("g", {"data-kind":"call-preview", "data-entry-call-id":entry.callId || entry.id});
          preview.append(make("title", {}, "Preview of the first measured call only. Other calls are collapsed; their receivers and return types may differ."));
          group.append(preview);
          y += sourceControl(preview, step, x+98, y, {kind:"call chain", label:`${entry.label} · +${step.children.length-1} calls collapsed (entry preview)`}, entry);
          y += 24;
        } else { y += sourceControl(group, step, x+98, y); y += 28; }
        if (expanded) { draw(step.children, group, depth+1); draw(step.alternate, group, depth+1); y += 4; }
        box.setAttribute("height", y-start+8); y += 8;
        continue;
      }
      // A cross-column call's body is its activation (#636), not a frame.
      const returns = displayProjection.returns?.has(step);
      const fragment = !returns && (["branch", "loop", "try"].includes(step.kind) || step.children?.length || step.alternate?.length);
      if (quietGuard(step)) {
        // One rail per original generated guard. The rail encloses only that
        // guard's children and ends before its next sibling. No flattening.
        const start = y, x = 18 + depth * 10;
        group.setAttribute("data-presentation", "continuation-guard");
        const rail = make("path", {class:"sequence-guard-rail", fill:"none"}); group.append(rail);
        y += sourceControl(group, step, x+10, y); y += 28;
        draw(step.children, group, depth+1);
        rail.setAttribute("d", `M ${x+5} ${start-12} H ${x} V ${y-10} H ${x+5}`);
        y += 8;
      } else if (fragment) {
        const start = y, x = 18 + depth * 10;
        const box = make("rect", {x, y:start-19, width:width-x-18, height:34, class:"sequence-fragment"}); group.append(box);
        const operator = {branch:"alt", loop:"loop", try:"try"}[step.kind] || "block";
        y += sourceControl(group, step, x, y, step, step, operator); y += 34;
        draw(step.children, group, depth+1);
        if (step.alternate?.length) {
          group.append(make("line", {x1:x, x2:width-18, y1:y-12, y2:y-12, class:"sequence-divider"}));
          group.append(make("text", {x:x+10, y:y+2, class:"sequence-meta"}, step.kind === "branch" ? "else / alternate" : "handlers / cleanup (see guards)")); y += 24;
          draw(step.alternate, group, depth+1);
        }
        y += 4; box.setAttribute("height", y-start+8);
        y += 14; // Keep the next fragment's tab clear of this frame's bottom edge.
      } else {
        const note = analysisKinds.has(step.kind);
        if (note && !options.showDetails) group.setAttribute("data-presentation", "quiet-note");
        const labelX = note ? Math.max(30+depth*10, laneX(step)-90) : 30+depth*10;
        const box = note && options.showDetails ? make("rect", {x:labelX, y:y-15, width:width-labelX-24, height:24, class:"sequence-note"}) : null;
        if (box) group.append(box);
        const extra = sourceControl(group, step, labelX, y); y += extra;
        if (box) box.setAttribute("height", 24+extra);
        y += 28;
        if (returns) { draw(step.children, group, depth+1); draw(step.alternate, group, depth+1); drawReturn(group, step); }
      }
    }
  }
  draw(view.steps, svg, 0);
  y = Math.max(y+16, 160);
  for (const x of positions.values()) lines.append(make("line", {x1:x, x2:x, y1:54+head, y2:y-10}));
  svg.setAttribute("viewBox", `0 0 ${width} ${y}`); svg.setAttribute("height", y);
  container.append(svg);
  options.onRendered?.();
  return groupControls;
}

window.TrellisSequence = {render: renderSequence, target:sequenceTarget, evidenceLabel:sequenceEvidenceLabel, evidenceSummary:sequenceEvidenceSummary, basisLabel:sequenceBasisLabel, entry:sequenceEntry, earlyReturn:sequenceEarlyReturn, display:sequenceDisplay, beginRender:beginSequenceRender, targetActionLabel:sequenceTargetActionLabel, column:sequenceColumn, laneHeader:sequenceLaneHeader, compactLabel:sequenceCompactLabel, arrowLabel:sequenceArrowLabel, labelLimit:SEQUENCE_LABEL_LIMIT, emptySummary:sequenceEmptySummary, renderEmpty:renderSequenceEmpty, stepColumn:sequenceStepColumn};
