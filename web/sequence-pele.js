"use strict";
// Pele is presentation only. Statement positions bind notes and messages to original DTO objects.
(() => {
  const ROOT_STYLE = "max-width:100%;height:auto;--_bg:var(--pele-bg,#fff);--_fg:var(--pele-fg,#1f1f1f);--_m:var(--pele-muted,#6e6e6e);--_l:var(--pele-line,#1f1f1f);--_s:var(--pele-surface,#f3f3f3);--_a:var(--pele-surface-alt,#e6e6e6);--_b:var(--pele-border,#8a8a8a);--_c:var(--pele-accent,#5b7bd5);color:var(--_fg);--_r:var(--pele-radius,4px);--_hw:var(--pele-heading-weight,bold);--_fm:var(--pele-font-mono,monospace);font:16px sans-serif;letter-spacing:normal;word-spacing:normal;text-transform:none;direction:ltr;font-family:var(--pele-font,sans-serif)";
  const SVG_NS = "http://www.w3.org/2000/svg";
  const kinds = new Set(["call", "group", "branch", "loop", "try", "effect", "return", "throw", "await", "note", "boundary", "definition"]);
  const tags = new Set(["svg", "g", "path", "rect", "text", "tspan"]);
  const attrs = new Set(["xmlns", "class", "viewBox", "width", "height", "fill", "fill-opacity", "role", "aria-roledescription", "transform", "stroke", "font-size", "data-id", "d", "x", "y", "rx", "text-anchor", "font-weight", "stroke-dasharray", "stroke-linecap", "xml:space"]);
  function unsupported(reason) { const error = new Error(reason); error.code = "unsupported"; throw error; }
  // Pele decodes numeric entities into SVG text. Escape everything outside
  // a small literal alphabet so repository text cannot become Mermaid grammar.
  const encode = value => Array.from(String(value)).map(c => /[A-Za-z0-9 ._-]/.test(c) ? c : `#${c.codePointAt(0)};`).join("");
  function witness(step) {
    const r = step?.range;
    return typeof step?.path === "string" && !!step.path.trim() && r &&
      Number.isSafeInteger(r.startLine) && r.startLine > 0 && Number.isSafeInteger(r.endLine) && r.endLine >= r.startLine &&
      (r.startColumn == null || Number.isSafeInteger(r.startColumn) && r.startColumn > 0) &&
      (r.endColumn == null || Number.isSafeInteger(r.endColumn) && r.endColumn > 0) &&
      (r.startLine !== r.endLine || r.startColumn == null || r.endColumn == null || r.endColumn >= r.startColumn) &&
      (r.startByte == null || Number.isSafeInteger(r.startByte) && r.startByte >= 0) &&
      (r.endByte == null || Number.isSafeInteger(r.endByte) && r.endByte > (r.startByte ?? -1));
  }
  function project(view, expanded = new Set(), options = {}) {
    if (!view || !view.seed || !Array.isArray(view.steps) || !Array.isArray(view.participants) || !view.revision) unsupported("Invalid sequence view");
    const seen = new Set(), rows = [], frames = [], frameHeaders = [], closeFrames = [], closeHeaders = [], noteOrder = [null];
    const intro = "Static possible paths - not a runtime trace";
    const lanes = [{alias:"S", id:view.seed.id, label:view.seed.name || view.seed.id, participant:view.participants.find(p => p.id === view.seed.id), proof:null}];
    const lines = ["sequenceDiagram", `Note over S: ${intro}`];
    let separators = 1;
    const targetFor = step => window.TrellisSequence?.target(view, step);
    const laneFor = (target, proof) => {
      let lane = lanes.find(item => item.id === target.id);
      if (!lane) {
        if (lanes.length >= 20) unsupported("Too many sequence participants");
        lane = {alias:`P${lanes.length}`, id:target.id, label:target.declaration.name || target.label, participant:target, proof}; lanes.push(lane);
      } else if (!lane.proof) lane.proof = proof;
      return lane;
    };
    const add = (step, preview) => {
      const ordinal = rows.length + 1;
      if (ordinal > 200) unsupported("Too many source notes");
      let label = step.label || step.kind;
      if (step.kind === "call" || step.kind === "boundary" && step.callId) label = step.callEvidence
        ? `${window.TrellisSequence.evidenceLabel(step, view)} - ${label}`
        : `${label} - ${window.TrellisSequence.evidenceLabel(step, view)}`;
      const shorten = (text, max) => text.length > max ? text.slice(0, Math.max(1, max - 3)) + "..." : text;
      let displayLabel;
      if (preview) {
        // Source labels are encoded separately; the app-generated count and
        // original call ID stay literal and visible for long measured names.
        const entryId = preview.callId || preview.id;
        if (typeof entryId !== "string" || !/^[A-Za-z0-9:_-]+$/.test(entryId)) unsupported("Unrepresentable preview identity");
        const suffix = ` +${step.children.length - 1} calls collapsed (entry preview)`;
        const fixed = ` - first measured entry:  - call ${entryId} -${suffix}`;
        const room = Math.max(24, 160 - fixed.length);
        const groupLimit = Math.max(12, Math.floor(room / 2));
        const entryLimit = Math.max(12, room - groupLimit);
        displayLabel = `${shorten(label, groupLimit)} - first measured entry: ${shorten(preview.label, entryLimit)} - call ${entryId} -${suffix}`;
        if (preview.callEvidence) displayLabel = `${window.TrellisSequence.evidenceLabel(preview, view)} - ${displayLabel}`;
      } else {
        if (!options.showDetails) label = shorten(label, 160);
        displayLabel = label;
      }
      const marker = `[${String(ordinal).padStart(3, "0")}]`;
      const display = `${marker} ${step.kind} ${displayLabel}`;
      const evidence = preview || step, target = targetFor(evidence), lane = target && laneFor(target, evidence);
      const message = lane && evidence.kind === "call", type = message ? "message" : "note";
      lines.push(message ? `S->>${lane.alias}: ${encode(display)}` : `Note right of S: ${encode(display)}`);
      const row = {ordinal, marker, display, step, id:step.id, callId:step.callId, path:step.path, range:step.range, revision:view.revision, previewEntryCallId:preview?.callId, preview, evidence, target, type};
      rows.push(row); if (!message) noteOrder.push(row);
    };
    const visit = (steps, depth) => {
      if (!Array.isArray(steps) || depth > 24) unsupported("Unsupported sequence depth");
      for (const step of steps) {
        if (!step || !kinds.has(step.kind) || typeof step.id !== "string" || !step.id || typeof step.label !== "string" ||
            !Array.isArray(step.children) || !Array.isArray(step.alternate) || typeof step.hidden !== "boolean" || !witness(step)) unsupported("Unsupported sequence step");
        if (seen.has(step.id)) unsupported("Duplicate sequence identity");
        seen.add(step.id);
        if (step.hidden) continue;
        const children = step.children, alternate = step.alternate;
        const collapsed = step.kind === "group" && !expanded.has(step.id);
        const preview = collapsed && !alternate.length && children.length && children.every(child => child.kind === "call" && !child.hidden && !child.children?.length && !child.alternate?.length && witness(child) && child.path === step.path) ? children[0] : null;
        // Validate collapsed members too: they become visible upon expansion.
        if (collapsed) {
          const check = members => { for (const child of members) { if (!child || !kinds.has(child.kind) || !witness(child) || !child.id || seen.has(child.id) || !Array.isArray(child.children) || !Array.isArray(child.alternate)) unsupported("Unsupported collapsed member"); seen.add(child.id); if (child.children.length || child.alternate.length) check([...child.children, ...child.alternate]); } };
          check([...children, ...alternate]);
        }
        let frame = null;
        if (step.kind === "branch") frame = alternate.length ? "alt" : "opt";
        else if (step.kind === "loop") frame = "loop";
        else if (step.kind === "try" || (step.kind !== "group" && (children.length || alternate.length))) frame = "rect";
        const header = frame && frame !== "rect" ? {kind:frame, header:`[${step.label}]`, alternate:frame === "alt" ? "[alternate path]" : null} : null;
        if (frame) {
          lines.push(frame === "rect" ? "rect" : `${frame} ${encode(step.label)}`);
          frames.push(frame);
          if (header) frameHeaders.push(header);
        }
        add(step, preview);
        if (!collapsed) {
          visit(children, depth + 1);
          if (alternate.length) {
            if (frame === "alt") lines.push("else alternate path");
            else { lines.push("Note over S: handlers / cleanup (see guards)"); separators++; noteOrder.push(null); }
            visit(alternate, depth + 1);
          }
        }
        if (frame) {
          lines.push("end");
          // Pele appends completed frames to SVG after their nested frames.
          // Identity remains in the numbered header note, never frame geometry.
          if (header) { closeFrames.push(frame); closeHeaders.push(header); }
        }
        if (separators > 200) unsupported("Too many separators");
      }
    };
    // Validate original DTO target evidence even when its row is hidden or folded.
    const pending = [...view.steps];
    let inspected = 0;
    while (pending.length) {
      if (++inspected > 12000) unsupported("Too many sequence members");
      const step = pending.pop();
      if (step?.target != null && !targetFor(step)) unsupported("Invalid resolved target evidence");
      if (step?.callEvidence?.boundaryReason === "none" && !targetFor(step)) unsupported("Missing resolved declaration witness");
      if (Array.isArray(step?.children)) pending.push(...step.children);
      if (Array.isArray(step?.alternate)) pending.push(...step.alternate);
    }
    visit(view.steps, 0);
    lines.splice(1, 0, ...lanes.map(lane => `participant ${lane.alias} as ${encode(lane.label)}`));
    const source = lines.join("\n") + "\n";
    if (new TextEncoder().encode(source).length > 512 * 1024) unsupported("Mermaid source too large");
    // Participants do not consume Pele statement IDs. Every note, message,
    // operator, separator and end does, including statements without a row.
    let statement = 0, rowIndex = 0;
    const noteIds = [], messageIds = [];
    for (const line of lines) {
      if (line === "sequenceDiagram" || line.startsWith("participant ")) continue;
      const id = `i${statement++}`;
      if (line.startsWith("Note ")) noteIds.push(id);
      if (line.startsWith("S->>")) messageIds.push(id);
      if (line.startsWith("Note right of S:") || line.startsWith("S->>")) rows[rowIndex++].statementId = id;
    }
    return {source, rows, lanes, intro, frames, frameHeaders, closeFrames, closeHeaders, noteIds, messageIds, separators, noteOrder};
  }

  function validatedSvg(raw, projection) {
    if (typeof raw !== "string" || new TextEncoder().encode(raw).length > 8 * 1024 * 1024) unsupported("SVG output too large");
    const start = /^<svg xmlns="http:\/\/www\.w3\.org\/2000\/svg" class="pele pele-sequence"/;
    if (!start.test(raw)) unsupported("Unexpected Pele root");
    // Pele omits the heading-weight variable when no opt/alt/loop frame exists.
    const rootStyle = projection.frames.some(frame => frame !== "rect")
      ? ROOT_STYLE : ROOT_STYLE.replace("--_hw:var(--pele-heading-weight,bold);", "");
    const signature = ` style="${rootStyle}"`;
    if (raw.split(signature).length !== 2) unsupported("Unexpected Pele theme");
    let safe = raw.replace(signature, "").replace(start, '<svg xmlns="http://www.w3.org/2000/svg" class="pele pele-sequence trellis-pele"');
    safe = safe.replaceAll(' style="rx:var(--_r)"', ' rx="var(--_r)"');
    if (/\sstyle\s*=|<style\b/i.test(safe)) unsupported("Unexpected inline Pele style");
    const root = new DOMParser().parseFromString(safe, "image/svg+xml").documentElement;
    if (root.tagName !== "svg" || root.namespaceURI !== SVG_NS || root.querySelector("parsererror")) unsupported("Invalid SVG");
    let nodes = 0;
    for (const node of [root, ...root.querySelectorAll("*")]) {
      if (++nodes > 12000 || node.namespaceURI !== SVG_NS || !tags.has(node.localName)) unsupported("Unexpected SVG element");
      for (const a of node.attributes) {
        if (!attrs.has(a.name) || /url\s*\(|javascript:|<|>|[\u0000-\u001f]/i.test(a.value)) unsupported("Unexpected SVG attribute");
      }
    }
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
    const messages = [...root.querySelectorAll(".pele-message")], messageLabels = [...root.querySelectorAll(".pele-edge-label")];
    const messageRows = projection.rows.filter(row => row.type === "message");
    if (messages.length !== messageRows.length || messageLabels.length !== messageRows.length) unsupported("Missing Pele message");
    for (const [i, row] of messageRows.entries()) {
      if (messages[i].getAttribute("data-id") !== row.statementId || messageLabels[i].getAttribute("data-id") !== row.statementId ||
          messageLabels[i].textContent !== row.display) unsupported("Changed Pele message identity or text");
    }
    const actors = [...root.querySelectorAll(".pele-actor")], lifelines = [...root.querySelectorAll(".pele-lifeline")];
    if (actors.length !== projection.lanes.length * 2 || lifelines.length !== projection.lanes.length) unsupported("Changed Pele participants");
    for (const lane of projection.lanes) {
      const members = actors.filter(actor => actor.getAttribute("data-id") === lane.alias);
      if (members.length !== 2 || members.some(actor => actor.textContent !== lane.label) ||
          lifelines.filter(line => line.getAttribute("data-id") === lane.alias).length !== 1) unsupported("Changed Pele participant identity or text");
    }
    const observed = [...root.querySelectorAll(".pele-frame")].map(n => n.classList.contains("pele-frame-alt") ? "alt" : n.classList.contains("pele-frame-opt") ? "opt" : n.classList.contains("pele-frame-loop") ? "loop" : "unknown");
    if (observed.join("|") !== projection.closeFrames.join("|") || root.querySelectorAll(".pele-rect").length !== projection.frames.filter(x => x === "rect").length) unsupported("Changed Pele control frames");
    for (const [i, frame] of [...root.querySelectorAll(".pele-frame")].entries()) {
      const expected = projection.closeHeaders[i];
      if (frame.hasAttribute("data-id") || frame.querySelector("[data-id]")) unsupported("Unexpected Pele frame identity");
      if (frame.querySelector(".pele-frame-kind")?.textContent !== expected.kind) unsupported("Changed Pele frame kind");
      const labels = [...frame.querySelectorAll(".pele-frame-label")].map(n => n.textContent.replace(/\s+/g, ""));
      const wanted = (expected.alternate ? [expected.alternate, expected.header] : [expected.header]).map(n => n.replace(/\s+/g, ""));
      if (labels.join("|") !== wanted.join("|")) unsupported("Changed Pele frame header");
    }
    // The alt frame has no native ID; its original header is the numbered note.
    return {root, notes, messages, messageLabels, actors};
  }
  async function render(container, view, readSource, expandedGroups = new Set(), options = {}) {
    const current = () => !options.isCurrent || options.isCurrent();
    if (!current()) return {status:"stale"};
    const renderCurrent = window.TrellisSequence.beginRender(container);
    const active = () => current() && renderCurrent();
    try {
      const projection = project(view, expandedGroups, options);
      if (typeof window.pele?.render !== "function") unsupported("Pele asset unavailable");
      // Pele renders synchronously. Input caps bound the work; a timer cannot
      // interrupt it. Output byte/node caps are checked before installation.
      const result = window.pele.render(projection.source, {}).svg;
      if (!current()) return {status:"stale"};
      const {root, notes, messages, messageLabels, actors} = validatedSvg(result, projection);
      const rowNodes = [...notes, ...messageLabels];
      for (const lane of projection.lanes) {
        if (!lane.proof || typeof options.onOpenTarget !== "function") continue;
        for (const actor of actors.filter(node => node.getAttribute("data-id") === lane.alias)) {
          actor.setAttribute("role", "button"); actor.setAttribute("tabindex", "0");
          actor.setAttribute("data-trellis-target-id", lane.id);
          actor.setAttribute("aria-label", `${window.TrellisSequence.targetActionLabel(lane.participant.declaration)}: ${lane.participant.declaration.name}`);
          const open = () => {
            if (active() && window.TrellisSequence.target(view, lane.proof) === lane.participant) options.onOpenTarget(lane.participant.declaration, lane.proof);
          };
          actor.addEventListener("click", open);
          actor.addEventListener("keydown", event => { if (event.key === "Enter" || event.key === " ") { event.preventDefault(); open(); } });
        }
      }
      const selected = {id:options.selectedId || null};
      const controls = new Map();
      for (const [i, row] of projection.rows.entries()) {
        const note = rowNodes.find(node => node.getAttribute("data-id") === row.statementId), step = row.step;
        note.setAttribute("data-trellis-step-id", row.id);
        if (row.callId) note.setAttribute("data-trellis-call-id", row.callId);
        if (row.type === "message") {
          const arrow = messages.find(node => node.getAttribute("data-id") === row.statementId);
          arrow.setAttribute("data-trellis-step-id", row.id);
          if (row.evidence.callId) arrow.setAttribute("data-trellis-call-id", row.evidence.callId);
          if (row.previewEntryCallId) arrow.setAttribute("data-trellis-preview-call-id", row.previewEntryCallId);
          arrow.setAttribute("data-trellis-target-id", row.target.id);
        }
        if (row.previewEntryCallId) note.setAttribute("data-trellis-preview-call-id", row.previewEntryCallId);
        note.setAttribute("tabindex", "0"); note.setAttribute("role", "button");
        note.setAttribute("aria-label", `Inspect ${step.kind}: ${step.label}${row.previewEntryCallId ? `; first measured entry ${row.preview.label}, call ${row.previewEntryCallId}; remaining calls collapsed` : ""} (${step.path}:${step.range.startLine})`);
        note.setAttribute("aria-pressed", String(selected.id === row.id));
        const select = () => {
          if (!active() || !witness(step) || row.id !== step.id || row.callId !== step.callId || row.path !== step.path || row.range !== step.range || projection.rows[i] !== row) return;
          selected.id = row.id;
          for (const n of rowNodes) n.setAttribute("aria-pressed", String(n.getAttribute("data-trellis-step-id") === row.id));
          if (options.onSelect) options.onSelect(step); else if (readSource) readSource(step);
        };
        note.addEventListener("click", select);
        note.addEventListener("keydown", e => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); select(); } });
        if (step.kind === "group") {
          // Attach the HTML action to its already verified SVG note. Geometry
          // places it only; ordinal/original-object witnesses own identity.
          const rect = note.querySelector("rect");
          const text = note.querySelector("text");
          // Parsed SVG is not connected, so getBBox() is not available. Text
          // coordinates place this control only; original row evidence owns it.
          const labelWidth = Math.max(1, row.display.length * 8);
          const box = rect ? {x:Number(rect.getAttribute("x")), y:Number(rect.getAttribute("y")), width:Number(rect.getAttribute("width"))}
            : {x:Number(text?.getAttribute("x")) - labelWidth / 2, y:Number(text?.getAttribute("y")) - 20, width:labelWidth};
          const {x, y, width} = box;
          if (![x, y, width].every(Number.isFinite) || width <= 0) unsupported("Changed Pele group geometry");
          const foreign = document.createElementNS(SVG_NS, "foreignObject");
          foreign.setAttribute("x", String(x + width + 4)); foreign.setAttribute("y", String(y));
          foreign.setAttribute("width", "126"); foreign.setAttribute("height", "32");
          const noteId = `trellis-pele-group-note-${row.ordinal}`;
          const toggleId = `trellis-pele-group-toggle-${row.ordinal}`;
          note.id = noteId; note.setAttribute("aria-describedby", toggleId);
          const toggle = document.createElement("button"); toggle.type = "button"; toggle.className = "sequence-group-control";
          toggle.id = toggleId; toggle.setAttribute("aria-controls", noteId);
          toggle.textContent = expandedGroups.has(row.id) ? "Collapse call chain" : "Expand call chain";
          toggle.setAttribute("aria-label", `${toggle.textContent}: ${step.label}`);
          toggle.setAttribute("aria-expanded", String(expandedGroups.has(row.id)));
          toggle.dataset.trellisStepId = row.id;
          // Native button Enter/Space dispatches click. Do not bubble those
          // keys or the click into the separately selectable group note.
          toggle.addEventListener("keydown", event => event.stopPropagation());
          toggle.addEventListener("click", async event => {
            event.stopPropagation();
            if (!active() || projection.rows[i] !== row) return;
            const top = container.scrollTop, left = container.scrollLeft;
            if (expandedGroups.has(row.id)) expandedGroups.delete(row.id); else expandedGroups.add(row.id);
            const outcome = await render(container, view, readSource, expandedGroups, {...options, selectedId:selected.id});
            if (!current()) return;
            if (outcome.status === "unsupported") {
              options.onUnsupported?.(outcome.reason);
              return;
            }
            if (outcome.status !== "rendered") return;
            outcome.controls.get(row.id)?.focus({preventScroll:true}); container.scrollTop = top; container.scrollLeft = left;
          });
          foreign.append(toggle); note.append(foreign);
          controls.set(row.id, toggle);
        }
      }
      if (!current()) return {status:"stale"};
      if (controls.size) {
        // Pele's viewBox fits its own notes; reserve measured-end space for
        // adjacent controls, without tying any identity to pixel positions.
        const dimensions = root.getAttribute("viewBox")?.split(/\s+/).map(Number);
        if (dimensions?.length !== 4 || !dimensions.every(Number.isFinite) || dimensions[2] <= 0) unsupported("Changed Pele viewBox");
        dimensions[2] += 140;
        root.setAttribute("viewBox", dimensions.join(" "));
        root.setAttribute("width", String(dimensions[2]));
      }
      const fragment = document.createDocumentFragment(); fragment.append(root); // Adopt the validated node itself; cloning would drop selection listeners.
      container.replaceChildren(fragment);
      return {status:"rendered", controls, source:projection.source};
    } catch (error) {
      if (!current()) return {status:"stale"};
      return {status:"unsupported", reason:String(error.message || error)};
    }
  }
  window.TrellisPeleSequence = {render, project, validatedSvg};
})();
