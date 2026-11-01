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
      evidence?.boundaryReason !== "none" || evidence.resolution !== "resolved" || evidence.freshness !== "fresh" ||
      !["direct", "constructor"].includes(evidence.dispatch) || !evidence.provenanceId || typeof step.target !== "string") return null;
  const participant = view.participants?.find(item => item.id === step.target);
  const declaration = participant?.declaration;
  return declaration?.id === step.target && sourceWitness(declaration) ? participant : null;
}
function sequenceEvidenceLabel(step, view) {
  if (!["call", "boundary"].includes(step?.kind) || !step.callId) return "Measured source syntax";
  const evidence = step.callEvidence;
  if (!evidence) return "measured syntax; target unverified";
  if (evidence.boundaryReason === "none" && step.hidden && step.incidental && !step.target) return "Resolved declaration - hidden incidental call, no lane";
  if (evidence.boundaryReason === "none" && view && !sequenceTarget(view, step)) return "Target unverified - invalid declaration evidence";
  const labels = {none:"Resolved declaration", external:"External boundary", unresolved:"Unresolved target",
    ambiguous:"Ambiguous target", dispatch:"Dispatch boundary", stale:"Stale evidence", nodeLimit:"Target omitted - participant limit",
    missingEvidence:"Unbound - missing semantic evidence"};
  return labels[evidence.boundaryReason] || "Target unverified";
}
function sequenceTargetActionLabel(declaration) {
  return ["function", "method"].includes(declaration.kind) ? "Open callee sequence" : "Read declaration source";
}
// The evidence tier of a view (#522): the producer whose fresh bindings built its lanes,
// or why it shows syntax only. Server text, never inferred here.
function sequenceEvidenceSummary(view) {
  const semantic = view?.semantic;
  if (semantic?.tier === "semantic" && typeof semantic.producerId === "string" && semantic.producerId)
    return `Semantic bindings · ${semantic.producerId}`;
  return typeof semantic?.reason === "string" && semantic.reason ? `Syntax only · ${semantic.reason}` : "Syntax only";
}
// The captured basis of a call's selected binding, as MCP reports it (#522).
function sequenceBasisLabel(evidence) {
  const basis = evidence?.semanticBasis;
  if (!basis || typeof basis.producerId !== "string") return "";
  return `basis ${basis.producerId}${typeof basis.producerVersion === "string" ? " " + basis.producerVersion : ""}`;
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
// Rust's standard library crates, as a producer names their package. Other languages' producers
// do not name one, so their external calls stay "external".
const STANDARD_LIBRARIES = new Set(["std", "core", "alloc"]);
// What to say when a view has steps and every one is hidden (#558), or "" when any shows.
// Counts come from the server; the reasons are the steps' own.
function sequenceEmptySummary(view) {
  const steps = view?.steps || [];
  if (!steps.length || steps.some(step => !step.hidden)) return "";
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
function renderSequenceEmpty(container, summary, onShow) {
  const note = document.createElement("p");
  note.className = "sequence-empty"; note.textContent = `${summary} `;
  if (onShow) {
    const show = document.createElement("button");
    show.type = "button"; show.textContent = "Show them";
    show.addEventListener("click", onShow);
    note.append(show);
  }
  container.replaceChildren(note);
}
const sequenceRenders = new WeakMap();
function beginSequenceRender(container) {
  const owner = {}; sequenceRenders.set(container, owner);
  return () => sequenceRenders.get(container) === owner;
}
function drawSequence(container, view, readSource, expandedGroups, options, selection) {
  const current = options.isCurrent, active = beginSequenceRender(container);
  options = {...options, isCurrent:() => active() && (!current || current())};
  const groupControls = new Map(), rowControls = [];
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
  while (pending.length) {
    const {step, depth} = pending.pop();
    if (step.hidden) continue;
    visibleDepth = Math.max(visibleDepth, depth);
    if (step.kind === "group" && !expandedGroups.has(step.id)) {
      const entry = entryCall(step);
      const target = sequenceTarget(view, entry);
      if (target) { visibleTargets.add(target.id); targetProofs.set(target.id, entry); }
      continue;
    }
    const target = sequenceTarget(view, step);
    if (target) { visibleTargets.add(target.id); targetProofs.set(target.id, step); }
    pending.push(...[...(step.alternate || []).slice().reverse(), ...(step.children || []).slice().reverse()].map(step => ({step, depth:depth+1})));
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
  const kindLabels = {externalCandidate:"External type · candidate", unresolvedReceiver:"Receiver · type unresolved", unresolvedCallee:"Callee · unresolved", boundary:"Unknown target", builtin:"Built-in name", import:"Imported binding", receiver:"Receiver hint", internal:"Indexed target", method:"Selected method"};
  const compactKinds = {externalCandidate:"candidate", unresolvedReceiver:"unresolved", unresolvedCallee:"unresolved", boundary:"unknown", builtin:"source hint", import:"source hint", receiver:"source hint", internal:"indexed", method:"selected"};
  const shorten = (text, limit) => text.length > limit ? `${text.slice(0, limit-1)}…` : text;
  for (const p of participants) {
    const x = positions.get(p.id);
    svg.append(make("rect", {x:x-95, y:10, width:190, height:44, class:"participant"}));
    const text = make("text", {x, y:28, "text-anchor":"middle", class:"sequence-participant-label"}, shorten(p.label, 24));
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
    const meta = make("text", {x, y:44, "text-anchor":"middle", class:"sequence-meta"}, (options.showDetails ? kindLabels[p.kind] : compactKinds[p.kind]) || p.kind);
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
  if (!options.showDetails) svg.append(make("text", {x:24, y:73, class:"sequence-meta"}, "Compact labels · select for details"));
  let y = options.showDetails ? 78 : 98;
  function sourceControl(group, step, x, top, display = step, evidence = step, operator = null) {
    const interactive = sourceWitness(step) && sourceWitness(evidence) && step.path === evidence.path &&
      (!options.isCurrent || options.isCurrent()) &&
      (typeof options.onSelect === "function" || typeof readSource === "function");
    const call = ["call", "boundary"].includes(evidence.kind) && evidence.callId, proof = call ? provenance(evidence) : null;
    const fullLabel = tooltip(step) + sequenceEarlyReturn(step) + (evidence !== step ? ` · Entry preview only: ${tooltip(evidence)}` : "");
    const control = make("g", {class:"sequence-source", "data-source-step-id":step.id,
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
    const early = sequenceEarlyReturn(step);
    if (!options.showDetails) label = shorten(label.replace(/\s+/g, " "), isControl ? Math.min(56, capacity - (operator ? 2 : 0)) : Math.max(12, capacity - early.length));
    label += early;
    // Brackets are a presentation frame, not an inferred or rewritten predicate.
    if (operator) label = `[${label}]`;
    let labels = options.showDetails ? wrap(label, capacity) : [label];
    if (display.kind === "call chain" && !options.showDetails) {
      const suffix = ` · +${step.children.length-1} calls collapsed (entry preview)${early}`;
      // Keep the collapsed count and entry-only claim visible even when the
      // measured name is long. At deep indentation the annotation can wrap.
      labels = wrap((evidence.callEvidence ? `${sequenceEvidenceLabel(evidence, view)} · ` : "") + shorten(evidence.label, Math.max(12, capacity-suffix.length)) + suffix, capacity);
    }
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
    const target = sequenceTarget(view, evidence);
    if (target && evidence.kind === "call" && columnX(target.id)) {
      const end = columnX(target.id), arrowY = top + extra + 10, from = laneX(evidence);
      const d = end === from ? `M ${from} ${arrowY} h 48 v 18 h -48` : `M ${from} ${arrowY} H ${end}`;
      group.append(make("path", {d, class:"sequence-arrow", fill:"none", "data-trellis-call-id":evidence.callId, "data-trellis-target-id":target.id}));
      const direction = end === from ? -1 : end > from ? 1 : -1, headY = arrowY + (end === from ? 18 : 0);
      group.append(make("path", {d:`M ${end-direction*7} ${headY-4} L ${end} ${headY} L ${end-direction*7} ${headY+4}`, class:"sequence-arrow", fill:"none"}));
    }
    group.append(control);
    return extra + (target && columnX(target.id) === laneX(evidence) ? 20 : 0);
  }
  function draw(steps, parent, depth) {
    for (const step of steps || []) {
      if (step.hidden) continue;
      const group = make("g", {"data-kind":step.kind, "data-step-id":step.id}); parent.append(group);
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
      const fragment = ["branch", "loop", "try"].includes(step.kind) || step.children?.length || step.alternate?.length;
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
      }
    }
  }
  draw(view.steps, svg, 0);
  y = Math.max(y+16, 160);
  for (const x of positions.values()) lines.append(make("line", {x1:x, x2:x, y1:54, y2:y-10}));
  svg.setAttribute("viewBox", `0 0 ${width} ${y}`); svg.setAttribute("height", y);
  container.append(svg);
  return groupControls;
}

window.TrellisSequence = {render: renderSequence, target:sequenceTarget, evidenceLabel:sequenceEvidenceLabel, evidenceSummary:sequenceEvidenceSummary, basisLabel:sequenceBasisLabel, entry:sequenceEntry, earlyReturn:sequenceEarlyReturn, beginRender:beginSequenceRender, targetActionLabel:sequenceTargetActionLabel, column:sequenceColumn, compactLabel:sequenceCompactLabel, emptySummary:sequenceEmptySummary, renderEmpty:renderSequenceEmpty};
