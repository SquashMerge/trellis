"use strict";
// Language-neutral DTO model and display projection. It draws no diagram: Pele does (sequence-pele.js).

// HTTP interface hops (#853). A client call that interface evidence matches to a route of
// another app targets that route's handler, drawn in its app's column (`app:<name>`). The
// match is structural, by method and path; its confidence stays on the arrow and in the
// inspector. An unresolved call has no handler and draws nothing new.
const SEQUENCE_HOP_CONFIDENCE = {exact:"exact", template:"template", wrapperInferred:"wrapper-inferred"};
function sequenceHop(view, step) {
  const hop = step?.interface;
  if (!["call", "boundary"].includes(step?.kind) || !step.callId || !window.TrellisSourceWitness(step) || hop?.kind !== "http" ||
      !Object.hasOwn(SEQUENCE_HOP_CONFIDENCE, hop.confidence) || typeof hop.participant !== "string" || !hop.participant ||
      typeof hop.method !== "string" || typeof hop.pathTemplate !== "string" || typeof hop.serverApp !== "string" || !hop.serverApp) return null;
  const participant = view?.participants?.find(item => item.id === hop.participant);
  return participant?.kind === "interfaceHandler" && participant.column?.id === `app:${hop.serverApp}` &&
    typeof participant.declaration?.id === "string" && typeof participant.declaration.name === "string" && window.TrellisSourceWitness(participant.declaration) ? participant : null;
}
// The words of a hop (#853): method, path template, confidence, and that it is not proof.
function sequenceHopLabel(hop) {
  return `${hop.method} ${hop.pathTemplate} · ${SEQUENCE_HOP_CONFIDENCE[hop.confidence]} · structural match, not proof`;
}
// What the inspector says of a call's interface (#853), or "" when it has none.
function sequenceInterfaceText(step, view) {
  const hop = step?.interface;
  if (hop?.kind !== "http") return "";
  const route = [hop.method, hop.pathTemplate].filter(item => typeof item === "string").join(" ") || "path not readable";
  if (sequenceHop(view, step)) return `HTTP interface hop · ${route} · ${SEQUENCE_HOP_CONFIDENCE[hop.confidence]} · ${hop.clientApp || "client"} → ${hop.serverApp} · structural match by method and path, not proof the request is sent`;
  if (hop.confidence === "unresolved") return `HTTP client call · ${route} · unresolved: no route matched · a missing match proves nothing`;
  // A match whose app column did not fit the participant limit is told, never drawn.
  if (typeof hop.serverApp === "string" && hop.serverApp && Object.hasOwn(SEQUENCE_HOP_CONFIDENCE, hop.confidence))
    return `HTTP interface · ${route} · ${SEQUENCE_HOP_CONFIDENCE[hop.confidence]} · ${hop.clientApp || "client"} → ${hop.serverApp} · not drawn: sequence participant limit · structural match, not proof`;
  return "";
}
// The call-chain groups that hold a drawn hop (#853). A collapsed chain previews its first call
// only when every member is shown, so a chain whose other calls stay hidden incidental calls
// opens expanded: the hop the status line counts is drawn. As #646 opens highlighted chains.
function sequenceHopGroups(view) {
  const groups = new Set();
  const walk = steps => {
    let found = false;
    for (const step of steps || []) {
      if (step.hidden) continue;
      const inner = walk(step.children) | walk(step.alternate);
      const here = !!sequenceHop(view, step);
      if (step.kind === "group" && inner) groups.add(step.id);
      found ||= here || !!inner;
    }
    return found;
  };
  walk(view?.steps);
  return groups;
}
// The view's interface reading (#853), for the status line. Absence is never no request.
function sequenceInterfaceSummary(view) {
  const read = view?.interfaces;
  if (!read) return "";
  if (read.state === "notMeasured") return "HTTP interfaces not measured yet for this revision";
  if (read.state === "unavailable") return "HTTP interfaces unavailable";
  if (read.state !== "ready") return "";
  const count = (n, one, many) => `${n} ${n === 1 ? one : many}`;
  return [read.hops > 0 && `${count(read.hops, "HTTP interface hop", "HTTP interface hops")} (structural matches, not proof)`,
    read.unresolved > 0 && count(read.unresolved, "unresolved HTTP call", "unresolved HTTP calls")].filter(Boolean).join(" · ");
}
function sequenceTarget(view, step) {
  const hop = sequenceHop(view, step);
  if (hop) return hop;
  const evidence = step?.callEvidence, pin = view?.revision;
  if (!["call", "boundary"].includes(step?.kind) || !step.callId || !window.TrellisSourceWitness(step) ||
      typeof pin?.indexGeneration !== "string" || !pin.indexGeneration || !Number.isSafeInteger(pin.indexRevision) ||
      evidence?.boundaryReason !== "none" || evidence.resolution !== "resolved" ||
      !(evidence.freshness === "fresh" && !evidence.projection ||
        evidence.freshness === "possiblyStale" && evidence.projection?.semanticStatus === "olderBasis" &&
        evidence.projection.currentPin?.indexGeneration === pin.indexGeneration &&
        evidence.projection.currentPin?.indexRevision === pin.indexRevision) ||
      !["direct", "constructor"].includes(evidence.dispatch) || !evidence.provenanceId || typeof step.target !== "string") return null;
  const participant = view.participants?.find(item => item.id === step.target);
  const declaration = participant?.declaration;
  return declaration?.id === step.target && window.TrellisSourceWitness(declaration) ? participant : null;
}
function sequenceEvidenceLabel(step, view) {
  if (!["call", "boundary"].includes(step?.kind) || !step.callId) return "Measured source syntax";
  // The matched route travels with the confidence wherever the hop is drawn, Show all included.
  if (view && sequenceHop(view, step)) return `HTTP interface hop · ${sequenceHopLabel(step.interface)}`;
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
function sequenceTargetActionLabel(declaration, participant = null) {
  // A hop's handler continues on demand (#853): its own Sequence, in its own app.
  if (participant?.kind === "interfaceHandler") return ["function", "method"].includes(declaration.kind) ? "Open handler sequence" : "Read handler source";
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
    children.every(child => child.kind === "call" && !child.hidden && !child.children?.length && !child.alternate?.length && window.TrellisSourceWitness(child) && child.path === step.path) &&
    window.TrellisSourceWitness(step) ? children[0] : null;
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
// The row of an arrow (#558): its number and the exact target's name. The
// lane names the type. The call's source text and evidence are in the inspector and Show all.
function sequenceArrowLabel(ordinal, target, collapsed, early, limit = SEQUENCE_LABEL_LIMIT, hop = null) {
  const marker = `[${String(ordinal).padStart(3, "0")}]`, name = target.declaration.name || target.label;
  const suffix = `${collapsed ? ` +${collapsed} calls collapsed (entry preview)` : ""}${early}`;
  if (hop && target.kind === "interfaceHandler") {
    // The confidence and the caveat always show; a long path template is cut instead (#853).
    const tail = ` · ${SEQUENCE_HOP_CONFIDENCE[hop.confidence]} · structural match, not proof${suffix}`, method = `${hop.method} `;
    const room = Math.max(8, Math.max(limit, 96) - marker.length - 1 - method.length - tail.length);
    return `${marker} ${method}${hop.pathTemplate.length > room ? `${hop.pathTemplate.slice(0, room - 1)}…` : hop.pathTemplate}${tail}`;
  }
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
  // An HTTP interface hop's handler is drawn in its app's column (#853).
  if (id.startsWith("app:") && participant.kind === "interfaceHandler") return "app";
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
// - a branch or try that encloses an arrow is an `alt` frame (`opt` with no alternate) around
//   its arrows, headed by calls-first's compact label; a same-column call whose expanded body
//   encloses an arrow is a frame named for its callee (#907). Like loops, neither is a row;
// - every other step draws nothing: internal steps, nested callables, calls without a target and
//   any chain or callback around an arrow (`through`: its arrows draw in its place).
//   `hiddenSteps` counts the shown DTO steps that draw nothing; Show all has them.
// - at most SEQUENCE_INTERACTION_LIMIT arrows are drawn, in source order (#663). The server
//   sends Interactions candidates before its visible step bound, which counts internal steps;
//   this bound counts arrows only, and `omittedInteractions` counts the arrows past it.
const SEQUENCE_INTERACTION_LIMIT = 200;
function sequenceInteractions(view, options, earlyReturn) {
  const visible = new Set(), through = new Set(), loops = new Set(), returns = new Set(), holds = new Set();
  const branches = new Set(), callees = new Set();
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
      // #907: a branch, or a same-column call's expanded body, around an arrow is a frame.
      else if (["branch", "try"].includes(step.kind) && holds.has(step)) { visible.add(step); branches.add(step); }
      else if (step.kind === "call" && holds.has(step) && sequenceTarget(view, step)) { visible.add(step); callees.add(step); }
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
  // A branch frame's header is calls-first's compact label; a callee frame's is the callee's name.
  const frameKind = step => step.alternate?.length ? "alt" : "opt";
  // `frameText` is the whole heading; `frameLabel` is it at the label limit.
  const frameText = step => {
    if (callees.has(step)) { const target = sequenceTarget(view, step); return target.declaration?.name || target.label; }
    return sequenceCompactLabel(step).replace(/\s+/g, " ");
  };
  const frameLabel = step => compactName(frameText(step), SEQUENCE_LABEL_LIMIT);
  return {interactions:true, visible, through, loops, branches, callees, returns, hiddenSteps, omittedInteractions, loopLabel, frameKind, frameLabel, frameText, earlyReturn};
}
function compactName(text, limit) {
  return text.length > limit ? `${text.slice(0, limit - 1)}…` : text;
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
// Replaces a diagram that could not be drawn with the renderer's reason (#1007). Nothing else
// is drawn in its place. It takes the container's render ownership, as a drawing would, so an
// older render cannot install over it and the controls of a replaced diagram stay inert.
function renderSequenceError(container, reason) {
  beginSequenceRender(container);
  const note = document.createElement("p");
  note.className = "sequence-empty"; note.textContent = `This Sequence could not be drawn: ${reason}`;
  container.replaceChildren(note);
}

window.TrellisSequence = {target:sequenceTarget, hop:sequenceHop, hopLabel:sequenceHopLabel, hopGroups:sequenceHopGroups, interfaceText:sequenceInterfaceText, interfaceSummary:sequenceInterfaceSummary, evidenceLabel:sequenceEvidenceLabel, evidenceSummary:sequenceEvidenceSummary, basisLabel:sequenceBasisLabel, entry:sequenceEntry, earlyReturn:sequenceEarlyReturn, display:sequenceDisplay, beginRender:beginSequenceRender, targetActionLabel:sequenceTargetActionLabel, column:sequenceColumn, laneHeader:sequenceLaneHeader, compactLabel:sequenceCompactLabel, arrowLabel:sequenceArrowLabel, labelLimit:SEQUENCE_LABEL_LIMIT, emptySummary:sequenceEmptySummary, renderEmpty:renderSequenceEmpty, renderError:renderSequenceError, stepColumn:sequenceStepColumn};
