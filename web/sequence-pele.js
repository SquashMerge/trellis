"use strict";
// Pele is presentation only. Statement positions bind notes and messages to original DTO objects.
(() => {
  const {unsupported, encode, rootStyle, themeOnce, asButton, NS:SVG_NS} = window.TrellisPeleSvg;
  const LAYOUT = '%%{init: {"sequence": {"actorMargin": 12}}}%%';
  const kinds = new Set(["call", "group", "branch", "loop", "try", "effect", "return", "throw", "exit", "await", "note", "boundary", "definition", "callback"]);
  const tags = new Set(["svg", "g", "path", "rect", "text", "tspan"]);
  const attrs = new Set(["xmlns", "class", "viewBox", "width", "height", "fill", "fill-opacity", "role", "aria-roledescription", "transform", "stroke", "font-size", "data-id", "d", "x", "y", "rx", "text-anchor", "font-weight", "stroke-dasharray", "stroke-linecap", "xml:space"]);
  // Pele omits the heading-weight variable when no opt/alt/loop frame exists.
  const THEME = {prefix:"max-width:100%;height:auto;"}, PLAIN_STYLE = rootStyle(16, THEME), FRAMED_STYLE = rootStyle(16, {...THEME, heading:true});
  const ADMISSION = {kind:"sequence", mark:true, active:/<\/?(?:script|foreignObject|image|a)\b/i, tags, attrs, words:{
    large:"SVG output too large", root:"Unexpected Pele root", theme:"Unexpected Pele theme", style:"Unexpected inline Pele style",
    active:"Unexpected SVG element", invalid:"Invalid SVG", element:"Unexpected SVG element", attribute:"Unexpected SVG attribute"}};
  const witness = window.TrellisSourceWitness;
  const shorten = (text, max) => text.length > max ? text.slice(0, Math.max(1, max - 3)) + "..." : text;
  const targetFor = (p, step) => window.TrellisSequence?.target(p.view, step);
  // One lane per type or module (#558); its targets are the exact methods drawn in it.
  const columnOf = participant => window.TrellisSequence.column(participant);
  // A lane's header says what kind of column it is and wraps a long name (#716).
  const laneHeader = (participant, label) => ({label, lines:window.TrellisSequence.laneHeader(participant, label)});
  // Each statement's meta, if any, receives its Pele statement ID: a row or a return.
  // Statement IDs follow emit order, so every statement goes through here, in source order.
  const emit = (p, line, meta = null) => { p.lines.push(line); p.metas.push(meta); };

  // The projection being built. Every helper below takes it as `p`, reads the view, the expanded
  // set, the options and the display projection from it, and appends to its lists in emit order.
  function projectionState(view, expanded, options) {
    const display = window.TrellisSequence.display(view, options);
    const intro = "Static possible paths - not a runtime trace";
    const seed = view.participants.find(p => p.id === view.seed.id);
    const lanes = [{alias:"S", id:columnOf(seed).id ?? view.seed.id, ...laneHeader(seed, seed?.column ? columnOf(seed).label : view.seed.name || view.seed.id), targets:new Map([[view.seed.id, {participant:seed, proof:null}]])}];
    return {view, expanded, options, display, intro, lanes, seen:new Set(), rows:[], frames:[], frameHeaders:[], closeFrames:[], closeHeaders:[],
      noteOrder:[null], messageOrder:[], lines:["sequenceDiagram", `Note over S: ${intro}`], metas:[null, null], separators:1, activations:0};
  }
  function laneFor(p, target, proof) {
    const column = columnOf(target);
    let lane = p.lanes.find(item => item.id === column.id);
    if (!lane) {
      if (p.lanes.length >= 20) unsupported("Too many sequence participants");
      lane = {alias:`P${p.lanes.length}`, id:column.id, ...laneHeader(target, target.column ? column.label : target.declaration.name || target.label), targets:new Map()}; p.lanes.push(lane);
    }
    const member = lane.targets.get(target.id);
    if (member) member.proof ||= proof; else lane.targets.set(target.id, {participant:target, proof});
    return lane;
  }
  // An expanded callee body runs on the callee's lane, opened by the call before it (#522).
  // A wrapper whose call is an HTTP interface hop (#853) draws its arrow to the handler's
  // app, so its expanded body opens the wrapper's own lane here.
  function ownerOf(p, step) {
    const laneParticipant = step.lane == null ? null : p.view.participants.find(item => item.id === step.lane);
    const ownerId = step.lane == null ? null : columnOf(laneParticipant).id;
    let owner = step.lane == null ? p.lanes[0] : p.lanes.find(item => item.id === ownerId);
    if (!owner && laneParticipant?.declaration && witness(laneParticipant.declaration)) owner = laneFor(p, laneParticipant, null);
    if (!owner) unsupported("Unknown sequence lane");
    return owner;
  }
  // A step's own label, with its evidence words when it is a call.
  function stepLabel(p, step) {
    const label = p.options.showDetails ? step.label || step.kind : window.TrellisSequence.compactLabel(step);
    if (!(step.kind === "call" || step.kind === "boundary" && step.callId)) return label;
    return step.callEvidence
      ? `${window.TrellisSequence.evidenceLabel(step, p.view)} - ${label}`
      : `${label} - ${window.TrellisSequence.evidenceLabel(step, p.view)}`;
  }
  // A collapsed chain's label: its own, then its first measured entry.
  function previewLabel(p, step, preview, label) {
    // Source labels are encoded separately; the app-generated count and
    // original call ID stay literal and visible for long measured names.
    const entryId = preview.callId || preview.id;
    if (typeof entryId !== "string" || !/^[A-Za-z0-9:_-]+$/.test(entryId)) unsupported("Unrepresentable preview identity");
    const suffix = ` +${step.children.length - 1} calls collapsed (entry preview)`;
    const fixed = ` - first measured entry:  - call ${entryId} -${suffix}`;
    const room = Math.max(24, 160 - fixed.length);
    const groupLimit = Math.max(12, Math.floor(room / 2));
    const entryLimit = Math.max(12, room - groupLimit);
    const displayLabel = `${shorten(label, groupLimit)} - first measured entry: ${shorten(preview.label, entryLimit)} - call ${entryId} -${suffix}`;
    return preview.callEvidence ? `${window.TrellisSequence.evidenceLabel(preview, p.view)} - ${displayLabel}` : displayLabel;
  }
  function noteLabel(p, step, preview, early) {
    const label = stepLabel(p, step);
    if (preview) return previewLabel(p, step, preview, label);
    // The row number and any mark count toward the limit.
    return p.options.showDetails ? label : shorten(label, window.TrellisSequence.labelLimit - 6 - early.length);
  }
  // What the row draws: an arrow's label, or a numbered note.
  function rowDisplay(p, {step, preview, evidence, target, ordinal, marker, early, displayLabel, message}) {
    return message && !p.options.showDetails
      ? window.TrellisSequence.arrowLabel(ordinal, target, preview ? step.children.length - 1 : 0, early, undefined, window.TrellisSequence.hop(p.view, evidence) && evidence.interface)
      : `${marker} ${p.options.showDetails ? `${step.kind} ` : ""}${displayLabel}${early}`;
  }
  // One row: a message when its evidence is a call with a lane, else a note. One statement.
  function add(p, step, preview) {
    const ordinal = p.rows.length + 1;
    if (ordinal > 200) unsupported("Too many source notes");
    const early = p.display.earlyReturn(step);
    const displayLabel = noteLabel(p, step, preview, early);
    const marker = `[${String(ordinal).padStart(3, "0")}]`;
    const evidence = preview || step, target = targetFor(p, evidence), lane = target && laneFor(p, target, evidence);
    const message = lane && evidence.kind === "call", type = message ? "message" : "note";
    const display = rowDisplay(p, {step, preview, evidence, target, ordinal, marker, early, displayLabel, message});
    const owner = ownerOf(p, step);
    const row = {ordinal, marker, display, step, id:step.id, callId:step.callId, path:step.path, range:step.range, revision:p.view.revision, previewEntryCallId:preview?.callId, preview, evidence, target, type, owner, lane};
    emit(p, message ? `${owner.alias}->>${lane.alias}: ${encode(display)}` : `Note right of ${owner.alias}: ${encode(display)}`, row);
    p.rows.push(row); if (message) p.messageOrder.push(row); else p.noteOrder.push(row);
    return row;
  }
  // Interactions (#636): a return closes the activation its cross-column call opened. It is
  // not a row: it takes no row number.
  function addReturn(p, row) {
    const entry = {returnOf:row};
    emit(p, `${row.lane.alias}-->>${row.owner.alias}: `, entry);
    emit(p, `deactivate ${row.lane.alias}`);
    p.messageOrder.push(entry);
  }
  function admitStep(p, step) {
    if (!step || !kinds.has(step.kind) || typeof step.id !== "string" || !step.id || typeof step.label !== "string" ||
        !Array.isArray(step.children) || !Array.isArray(step.alternate) || typeof step.hidden !== "boolean" || !witness(step)) unsupported("Unsupported sequence step");
    if (p.seen.has(step.id)) unsupported("Duplicate sequence identity");
    p.seen.add(step.id);
  }
  // Validate collapsed members too: they become visible upon expansion.
  function admitCollapsed(p, members) {
    for (const child of members) {
      if (!child || !kinds.has(child.kind) || !witness(child) || !child.id || p.seen.has(child.id) || !Array.isArray(child.children) || !Array.isArray(child.alternate)) unsupported("Unsupported collapsed member");
      p.seen.add(child.id);
      if (child.children.length || child.alternate.length) admitCollapsed(p, [...child.children, ...child.alternate]);
    }
  }
  // A collapsed chain of plain measured calls in one file shows its first entry.
  function previewOf(step) {
    const children = step.children;
    return !step.alternate.length && children.length && children.every(child => child.kind === "call" && !child.hidden && !child.children?.length && !child.alternate?.length && witness(child) && child.path === step.path) ? children[0] : null;
  }
  function plainFrame(p, step) {
    const children = step.children, alternate = step.alternate;
    if (step.kind === "branch") return alternate.length ? "alt" : "opt";
    if (step.kind === "loop") return "loop";
    // A cross-column call's body is its activation (#636), not a frame.
    if (step.kind === "try" || (step.kind !== "group" && (children.length || alternate.length) && !p.display.returns?.has(step))) return "rect";
    return null;
  }
  // The frame a step opens, if any, with its heading. Pure: it emits nothing.
  function frameFor(p, step) {
    let frame = plainFrame(p, step);
    // Interactions (#636): a loop around arrows is a frame only, labelled, with no row.
    const loopFrame = !!p.display.loops?.has(step);
    // Interactions (#907): so is a branch around arrows, and a same-column call's expanded
    // body: a shaded region whose first statement is a note naming the callee.
    const branchFrame = !!p.display.branches?.has(step), calleeFrame = !!p.display.callees?.has(step);
    if (branchFrame) frame = p.display.frameKind(step);
    else if (calleeFrame) frame = "rect";
    const heading = loopFrame ? p.display.loopLabel(step) : branchFrame ? p.display.frameLabel(step)
      : p.options.showDetails ? step.label : shorten(window.TrellisSequence.compactLabel(step), window.TrellisSequence.labelLimit);
    // An unqualified Interactions loop has a tag and no guard: Pele then draws no label.
    const header = frame && frame !== "rect" ? {kind:frame, header:heading ? `[${heading}]` : null, alternate:frame === "alt" ? "[alternate path]" : null} : null;
    return {frame, heading, header, loopFrame, calleeFrame, frameOnly:loopFrame || branchFrame || calleeFrame};
  }
  // Statements, in order: the frame's opening, then a callee frame's name note.
  function openFrame(p, step, {frame, heading, header, calleeFrame}) {
    if (frame) {
      emit(p, frame === "rect" ? "rect" : heading ? `${frame} ${encode(heading)}` : frame);
      p.frames.push(frame);
      if (header) p.frameHeaders.push(header);
    }
    if (calleeFrame) {
      const name = {display:p.display.frameLabel(step)};
      emit(p, `Note over ${ownerOf(p, step).alias}: ${encode(name.display)}`, name);
      p.noteOrder.push(name);
    }
  }
  // Statements, in order: the children, then the alternate path behind its divider.
  function visitBody(p, step, {frame, loopFrame}, depth) {
    visit(p, step.children, depth + 1);
    if (!step.alternate.length) return;
    if (frame === "alt") emit(p, "else alternate path");
    else if (!loopFrame) { emit(p, "Note over S: handlers / cleanup (see guards)"); p.separators++; p.noteOrder.push(null); }
    visit(p, step.alternate, depth + 1);
  }
  function closeFrame(p, {frame, header}) {
    if (!frame) return;
    emit(p, "end");
    // Pele appends completed frames to SVG after their nested frames.
    // Identity remains in the numbered header note, never frame geometry.
    if (header) { p.closeFrames.push(frame); p.closeHeaders.push(header); }
  }
  // Statements, in order: frame opening, callee name, the row, its activation, the body, its
  // return and deactivation, the frame's end.
  function visitShown(p, step, depth) {
    const collapsed = step.kind === "group" && !p.expanded.has(step.id);
    const preview = collapsed ? previewOf(step) : null;
    if (collapsed) admitCollapsed(p, [...step.children, ...step.alternate]);
    const shape = frameFor(p, step);
    openFrame(p, step, shape);
    const row = shape.frameOnly ? null : add(p, step, preview);
    const returns = p.display.returns?.has(step) && row?.type === "message";
    if (returns) { emit(p, `activate ${row.lane.alias}`); p.activations++; }
    if (!collapsed) visitBody(p, step, shape, depth);
    if (returns) addReturn(p, row);
    closeFrame(p, shape);
    if (p.separators > 200) unsupported("Too many separators");
  }
  function visit(p, steps, depth) {
    if (!Array.isArray(steps) || depth > 24) unsupported("Unsupported sequence depth");
    for (const step of steps) {
      admitStep(p, step);
      if (p.display.visible.has(step)) visitShown(p, step, depth);
      else if (p.display.through?.has(step)) { visit(p, step.children, depth + 1); visit(p, step.alternate, depth + 1); }
    }
  }
  // Validate original DTO target evidence even when its row is hidden or folded.
  function admitTargets(p) {
    const pending = [...p.view.steps];
    let inspected = 0;
    while (pending.length) {
      if (++inspected > 12000) unsupported("Too many sequence members");
      const step = pending.pop();
      if (step?.target != null && !targetFor(p, step)) unsupported("Invalid resolved target evidence");
      // A hidden incidental step has no lane or target (#547); Show all gives it both.
      if (step?.callEvidence?.boundaryReason === "none" && !targetFor(p, step) && !(step.hidden && step.incidental)) unsupported("Missing resolved declaration witness");
      if (Array.isArray(step?.children)) pending.push(...step.children);
      if (Array.isArray(step?.alternate)) pending.push(...step.alternate);
    }
  }
  function sourceOf(p) {
    // Each header line is encoded on its own; only the app's line break stays literal.
    const participants = p.lanes.map(lane => `participant ${lane.alias} as ${lane.lines.map(encode).join("<br/>")}`);
    // Lanes sit closer than Pele's default 40 units apart (#716): with wrapped headers a wide
    // diagram needs less sideways scrolling, and arrow labels still widen a gap they need.
    const source = [LAYOUT, p.lines[0], ...participants, ...p.lines.slice(1)].join("\n") + "\n";
    if (new TextEncoder().encode(source).length > 512 * 1024) unsupported("Mermaid source too large");
    return source;
  }
  // Participants do not consume Pele statement IDs. Every note, message, activation,
  // operator, separator and end does, including statements without a row.
  function statementIds(p) {
    let statement = 0;
    const noteIds = [], messageIds = [];
    for (const [i, line] of p.lines.entries()) {
      if (i === 0) continue;
      const id = `i${statement++}`;
      if (p.metas[i]) p.metas[i].statementId = id;
      if (line.startsWith("Note ")) noteIds.push(id);
      if (/^(?:S|P[0-9]+)--?>>/.test(line)) messageIds.push(id);
    }
    return {noteIds, messageIds};
  }
  function project(view, expanded = new Set(), options = {}) {
    if (!view || !view.seed || !Array.isArray(view.steps) || !Array.isArray(view.participants) || !view.revision) unsupported("Invalid sequence view");
    const p = projectionState(view, expanded, options);
    admitTargets(p);
    visit(p, view.steps, 0);
    const source = sourceOf(p), {noteIds, messageIds} = statementIds(p);
    return {source, rows:p.rows, lanes:p.lanes, intro:p.intro, frames:p.frames, frameHeaders:p.frameHeaders, closeFrames:p.closeFrames, closeHeaders:p.closeHeaders,
      noteIds, messageIds, separators:p.separators, noteOrder:p.noteOrder, messageOrder:p.messageOrder, activations:p.activations};
  }

  function checkNotes(root, projection) {
    const notes = [...root.querySelectorAll(".pele-note")];
    if (notes.length !== projection.noteIds.length) unsupported("Missing Pele note");
    const ids = new Set();
    for (const [i, note] of notes.entries()) {
      const id = note.getAttribute("data-id");
      if (id !== projection.noteIds[i] || ids.has(id)) unsupported("Changed Pele note order");
      ids.add(id);
      const expected = i === 0 ? projection.intro : projection.noteOrder[i]?.display || "handlers / cleanup (see guards)";
      if (note.textContent !== expected) unsupported("Changed Pele note text");
    }
    return notes;
  }
  function checkMessages(root, projection) {
    const messages = [...root.querySelectorAll(".pele-message")], messageLabels = [...root.querySelectorAll(".pele-edge-label")];
    // A return (#636) is an unlabeled message: it draws an arrow and no label.
    const messageRows = projection.rows.filter(row => row.type === "message");
    if (messages.length !== projection.messageOrder.length || messageLabels.length !== messageRows.length) unsupported("Missing Pele message");
    for (const [i, entry] of projection.messageOrder.entries()) {
      if (messages[i].getAttribute("data-id") !== entry.statementId) unsupported("Changed Pele message identity or text");
    }
    for (const [i, row] of messageRows.entries()) {
      if (messageLabels[i].getAttribute("data-id") !== row.statementId || messageLabels[i].textContent !== row.display) unsupported("Changed Pele message identity or text");
    }
    return {messages, messageLabels};
  }
  function checkLanes(root, projection) {
    const actors = [...root.querySelectorAll(".pele-actor")], lifelines = [...root.querySelectorAll(".pele-lifeline")];
    if (actors.length !== projection.lanes.length * 2 || lifelines.length !== projection.lanes.length) unsupported("Changed Pele participants");
    // A one-line header is a bare text; a wrapped one has a tspan per line.
    const headerLines = actor => { const spans = [...actor.querySelectorAll("tspan")].map(n => n.textContent); return spans.length ? spans : [actor.textContent]; };
    for (const lane of projection.lanes) {
      const members = actors.filter(actor => actor.getAttribute("data-id") === lane.alias);
      if (members.length !== 2 || members.some(actor => headerLines(actor).join("\n") !== lane.lines.join("\n")) ||
          lifelines.filter(line => line.getAttribute("data-id") === lane.alias).length !== 1) unsupported("Changed Pele participant identity or text");
    }
    return actors;
  }
  const frameKindOf = n => n.classList.contains("pele-frame-alt") ? "alt" : n.classList.contains("pele-frame-opt") ? "opt" : n.classList.contains("pele-frame-loop") ? "loop" : "unknown";
  function checkFrames(root, projection) {
    const observed = [...root.querySelectorAll(".pele-frame")].map(frameKindOf);
    if (observed.join("|") !== projection.closeFrames.join("|") || root.querySelectorAll(".pele-rect").length !== projection.frames.filter(x => x === "rect").length) unsupported("Changed Pele control frames");
    for (const [i, frame] of [...root.querySelectorAll(".pele-frame")].entries()) {
      const expected = projection.closeHeaders[i];
      if (frame.hasAttribute("data-id") || frame.querySelector("[data-id]")) unsupported("Unexpected Pele frame identity");
      if (frame.querySelector(".pele-frame-kind")?.textContent !== expected.kind) unsupported("Changed Pele frame kind");
      const labels = [...frame.querySelectorAll(".pele-frame-label")].map(n => n.textContent.replace(/\s+/g, ""));
      const wanted = [expected.alternate, expected.header].filter(Boolean).map(n => n.replace(/\s+/g, ""));
      if (labels.join("|") !== wanted.join("|")) unsupported("Changed Pele frame header");
    }
  }
  // Checks run in this order: admission, notes, messages, activations, lanes, frames.
  function validatedSvg(raw, projection) {
    const theme = raw => themeOnce(raw, projection.frames.some(frame => frame !== "rect") ? FRAMED_STYLE : PLAIN_STYLE);
    const {root} = window.TrellisPeleSvg.admit(raw, {...ADMISSION, theme});
    const notes = checkNotes(root, projection);
    const {messages, messageLabels} = checkMessages(root, projection);
    const activations = [...root.querySelectorAll(".pele-activation")];
    if (activations.length !== projection.activations) unsupported("Changed Pele activations");
    const actors = checkLanes(root, projection);
    checkFrames(root, projection);
    // The alt frame has no native ID; its original header is the numbered note.
    return {root, notes, messages, messageLabels, actors};
  }

  // The helpers below decorate one validated drawing. `d` is that render's context: the caller's
  // arguments, its `current` and `active` checks, the projection, the validated nodes, the
  // selection and the group controls made so far.
  function bindLaneActors(d) {
    const {view, options} = d;
    for (const lane of d.projection.lanes) {
      // A column of several methods names no one target: its arrows do, in the inspector.
      const [{participant, proof} = {}] = lane.targets.size === 1 ? lane.targets.values() : [];
      if (!proof || typeof options.onOpenTarget !== "function") continue;
      for (const actor of d.actors.filter(node => node.getAttribute("data-id") === lane.alias)) {
        asButton(actor, `${window.TrellisSequence.targetActionLabel(participant.declaration, participant)}: ${participant.declaration.name}`, () => {
          if (d.active() && window.TrellisSequence.target(view, proof) === participant) options.onOpenTarget(participant.declaration, proof);
        });
        actor.setAttribute("data-trellis-target-id", participant.id);
      }
    }
  }
  function bindArrow(d, row) {
    const arrow = d.messages.find(node => node.getAttribute("data-id") === row.statementId);
    arrow.setAttribute("data-trellis-step-id", row.id);
    if (row.evidence.callId) arrow.setAttribute("data-trellis-call-id", row.evidence.callId);
    if (row.previewEntryCallId) arrow.setAttribute("data-trellis-preview-call-id", row.previewEntryCallId);
    arrow.setAttribute("data-trellis-target-id", row.target.id);
  }
  function selectRow(d, row, i) {
    const {options, readSource} = d, step = row.step;
    if (!d.active() || !witness(step) || row.id !== step.id || row.callId !== step.callId || row.path !== step.path || row.range !== step.range || d.projection.rows[i] !== row) return;
    d.selected.id = row.id;
    for (const n of d.rowNodes) n.setAttribute("aria-pressed", String(n.getAttribute("data-trellis-step-id") === row.id));
    if (options.onSelect) options.onSelect(step); else if (readSource) readSource(step);
  }
  function bindRow(d, row, i) {
    const note = d.rowNodes.find(node => node.getAttribute("data-id") === row.statementId), step = row.step;
    note.setAttribute("data-trellis-step-id", row.id);
    if (row.callId) note.setAttribute("data-trellis-call-id", row.callId);
    if (row.type === "message") bindArrow(d, row);
    if (row.previewEntryCallId) note.setAttribute("data-trellis-preview-call-id", row.previewEntryCallId);
    asButton(note, `Inspect ${step.kind}: ${step.label}${row.previewEntryCallId ? `; first measured entry ${row.preview.label}, call ${row.previewEntryCallId}; remaining calls collapsed` : ""} (${step.path}:${step.range.startLine})`, () => selectRow(d, row, i));
    note.setAttribute("aria-pressed", String(d.selected.id === row.id));
    if (step.kind === "group") attachGroupToggle(d, row, i, note);
  }
  // Where a group's control goes: beside its note.
  function groupBox(note, row) {
    const rect = note.querySelector("rect");
    const text = note.querySelector("text");
    // Parsed SVG is not connected, so getBBox() is not available. Text
    // coordinates place this control only; original row evidence owns it.
    const labelWidth = Math.max(1, row.display.length * 8);
    const box = rect ? {x:Number(rect.getAttribute("x")), y:Number(rect.getAttribute("y")), width:Number(rect.getAttribute("width"))}
      : {x:Number(text?.getAttribute("x")) - labelWidth / 2, y:Number(text?.getAttribute("y")) - 20, width:labelWidth};
    const {x, y, width} = box;
    if (![x, y, width].every(Number.isFinite) || width <= 0) unsupported("Changed Pele group geometry");
    return box;
  }
  // Expand or collapse a chain: draw again, then put focus and scroll back.
  async function toggleGroup(d, row, i, event) {
    const {container, expandedGroups, options} = d;
    event.stopPropagation();
    if (!d.active() || d.projection.rows[i] !== row) return;
    const top = container.scrollTop, left = container.scrollLeft;
    if (expandedGroups.has(row.id)) expandedGroups.delete(row.id); else expandedGroups.add(row.id);
    const outcome = await render(container, d.view, d.readSource, expandedGroups, {...options, selectedId:d.selected.id});
    if (!d.current()) return;
    if (outcome.status === "unsupported") {
      options.onUnsupported?.(outcome.reason);
      return;
    }
    if (outcome.status !== "rendered") return;
    outcome.controls.get(row.id)?.focus({preventScroll:true}); container.scrollTop = top; container.scrollLeft = left;
  }
  function groupToggle(d, row, i, noteId, toggleId) {
    const toggle = document.createElement("button"); toggle.type = "button"; toggle.className = "sequence-group-control";
    toggle.id = toggleId; toggle.setAttribute("aria-controls", noteId);
    toggle.textContent = d.expandedGroups.has(row.id) ? "Collapse call chain" : "Expand call chain";
    toggle.setAttribute("aria-label", `${toggle.textContent}: ${row.step.label}`);
    toggle.setAttribute("aria-expanded", String(d.expandedGroups.has(row.id)));
    toggle.dataset.trellisStepId = row.id;
    // Native button Enter/Space dispatches click. Do not bubble those
    // keys or the click into the separately selectable group note.
    toggle.addEventListener("keydown", event => event.stopPropagation());
    toggle.addEventListener("click", event => toggleGroup(d, row, i, event));
    return toggle;
  }
  // Attach the HTML action to its already verified SVG note. Geometry
  // places it only; ordinal/original-object witnesses own identity.
  function attachGroupToggle(d, row, i, note) {
    const {x, y, width} = groupBox(note, row);
    const foreign = document.createElementNS(SVG_NS, "foreignObject");
    foreign.setAttribute("x", String(x + width + 4)); foreign.setAttribute("y", String(y));
    foreign.setAttribute("width", "126"); foreign.setAttribute("height", "32");
    const noteId = `trellis-pele-group-note-${row.ordinal}`;
    const toggleId = `trellis-pele-group-toggle-${row.ordinal}`;
    note.id = noteId; note.setAttribute("aria-describedby", toggleId);
    const toggle = groupToggle(d, row, i, noteId, toggleId);
    foreign.append(toggle); note.append(foreign);
    d.controls.set(row.id, toggle);
  }
  // Interactions (#636): a return names its call.
  function bindReturns(d) {
    for (const entry of d.projection.messageOrder) {
      if (!entry.returnOf) continue;
      const arrow = d.messages.find(node => node.getAttribute("data-id") === entry.statementId);
      arrow.setAttribute("data-trellis-return-step-id", entry.returnOf.id);
      arrow.setAttribute("data-trellis-target-id", entry.returnOf.target.id);
    }
  }
  // Synchronous: lane headers, then each row in order, then returns. Returns the group controls.
  function decorate(call, projection, {notes, messages, messageLabels, actors}) {
    const d = {...call, projection, messages, actors, rowNodes:[...notes, ...messageLabels]};
    bindLaneActors(d);
    d.selected = {id:call.options.selectedId || null}; d.controls = new Map();
    for (const [i, row] of projection.rows.entries()) bindRow(d, row, i);
    bindReturns(d);
    return d.controls;
  }
  // Pele's viewBox fits its own notes; reserve measured-end space for
  // adjacent controls, without tying any identity to pixel positions.
  function reserveControlSpace(root) {
    const dimensions = root.getAttribute("viewBox")?.split(/\s+/).map(Number);
    if (dimensions?.length !== 4 || !dimensions.every(Number.isFinite) || dimensions[2] <= 0) unsupported("Changed Pele viewBox");
    dimensions[2] += 140;
    root.setAttribute("viewBox", dimensions.join(" "));
    root.setAttribute("width", String(dimensions[2]));
  }
  // Decorate the validated drawing, then put it in the container: the only step that touches it.
  function install(call, projection, drawn) {
    const controls = decorate(call, projection, drawn), root = drawn.root;
    if (!call.current()) return {status:"stale"};
    if (controls.size) reserveControlSpace(root);
    const fragment = document.createDocumentFragment(); fragment.append(root); // Adopt the validated node itself; cloning would drop selection listeners.
    call.container.replaceChildren(fragment);
    call.options.onRendered?.();
    return {status:"rendered", controls, source:projection.source};
  }
  // Stop points, in order: not current on entry (before taking the container); after each pause
  // (not current, or another render took the container); after Pele drew and after decoration
  // (not current). Only `install` touches the container.
  // `pause` is an optional async yield between heavy phases, for hosts that cap one synchronous
  // run (the Herdr mod, #707). Without it nothing here awaits.
  // Pele renders synchronously. Input caps bound the work; a timer cannot
  // interrupt it. Output byte/node caps are checked before installation.
  async function render(container, view, readSource, expandedGroups = new Set(), options = {}) {
    const current = () => !options.isCurrent || options.isCurrent();
    if (!current()) return {status:"stale"};
    const renderCurrent = window.TrellisSequence.beginRender(container);
    const active = () => current() && renderCurrent();
    const pause = typeof options.pause === "function" ? options.pause : null;
    const breathe = async () => { await pause(); return active(); };
    const call = {container, view, readSource, expandedGroups, options, current, active};
    try {
      if (pause && !(await breathe())) return {status:"stale"};
      const projection = project(view, expandedGroups, options);
      if (typeof window.pele?.render !== "function") unsupported("Pele asset unavailable");
      if (pause && !(await breathe())) return {status:"stale"};
      const result = window.pele.render(projection.source, {}).svg;
      if (!current()) return {status:"stale"};
      if (pause && !(await breathe())) return {status:"stale"};
      const drawn = validatedSvg(result, projection);
      if (pause && !(await breathe())) return {status:"stale"};
      return install(call, projection, drawn);
    } catch (error) {
      if (!current()) return {status:"stale"};
      return {status:"unsupported", reason:String(error.message || error)};
    }
  }
  window.TrellisPeleSequence = {render, project, validatedSvg};
})();
