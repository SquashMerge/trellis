"use strict";
// Pele is presentation only. Ordinals bind its positional notes to original DTO objects.
(() => {
  const ROOT_STYLE = "max-width:100%;height:auto;--_bg:var(--pele-bg,#fff);--_fg:var(--pele-fg,#1f1f1f);--_m:var(--pele-muted,#6e6e6e);--_l:var(--pele-line,#1f1f1f);--_s:var(--pele-surface,#f3f3f3);--_a:var(--pele-surface-alt,#e6e6e6);--_b:var(--pele-border,#8a8a8a);--_c:var(--pele-accent,#5b7bd5);color:var(--_fg);--_r:var(--pele-radius,4px);--_hw:var(--pele-heading-weight,bold);--_fm:var(--pele-font-mono,monospace);font:16px sans-serif;letter-spacing:normal;word-spacing:normal;text-transform:none;direction:ltr;font-family:var(--pele-font,sans-serif)";
  const SVG_NS = "http://www.w3.org/2000/svg";
  const kinds = new Set(["call", "group", "branch", "loop", "try", "effect", "return", "throw", "await", "note", "boundary", "definition"]);
  const tags = new Set(["svg", "g", "path", "rect", "text", "tspan"]);
  const attrs = new Set(["xmlns", "class", "viewBox", "width", "height", "fill", "role", "aria-roledescription", "transform", "stroke", "font-size", "data-id", "d", "x", "y", "rx", "text-anchor", "font-weight", "stroke-dasharray", "xml:space"]);
  function unsupported(reason) { const error = new Error(reason); error.code = "unsupported"; throw error; }
  const encode = value => Array.from(String(value)).map(c => /[A-Za-z0-9 ._-]/.test(c) ? c : `U+${c.codePointAt(0).toString(16).toUpperCase()}`).join("");
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
    const seen = new Set(), rows = [], frames = [], noteOrder = [null], lines = ["sequenceDiagram", `participant S as ${encode(view.seed.name || view.seed.id)}`, "Note over S: Static possible paths - target and timing unknown"];
    let separators = 1;
    const add = (step, preview) => {
      const ordinal = rows.length + 1;
      if (ordinal > 200) unsupported("Too many source notes");
      let label = step.label || step.kind;
      if (step.kind === "call") label += " - measured syntax; target unverified";
      if (preview) label += ` - ${preview.label} - +${step.children.length - 1} calls collapsed (entry preview)`;
      // A compact label never changes the original evidence object retained in rows.
      if (!options.showDetails && label.length > 160) label = label.slice(0, 159) + "...";
      const marker = `[${String(ordinal).padStart(3, "0")}]`;
      const display = `${marker} ${encode(step.kind)} ${encode(label)}`;
      lines.push(`Note right of S: ${display}`);
      const row = {ordinal, marker, display, step, id:step.id, callId:step.callId, path:step.path, range:step.range, revision:view.revision, previewEntryCallId:preview?.callId, preview};
      rows.push(row); noteOrder.push(row);
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
        if (frame) {
          lines.push(frame === "rect" ? "rect" : `${frame} ${encode(step.label)}`);
          frames.push(frame);
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
        if (frame) lines.push("end");
        if (separators > 200) unsupported("Too many separators");
      }
    };
    visit(view.steps, 0);
    const source = lines.join("\n") + "\n";
    if (new TextEncoder().encode(source).length > 512 * 1024) unsupported("Mermaid source too large");
    return {source, rows, frames, separators, noteOrder};
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
    if (notes.length !== projection.rows.length + projection.separators) unsupported("Missing Pele note");
    const ids = new Set();
    for (const [i, note] of notes.entries()) {
      const id = note.getAttribute("data-id");
      if (!/^i\d+$/.test(id || "") || ids.has(id) || i && Number(id.slice(1)) <= Number(notes[i-1].getAttribute("data-id").slice(1))) unsupported("Changed Pele note order");
      ids.add(id);
      const marker = i === 0 ? "Static possible paths" : projection.noteOrder[i]?.marker || "handlers / cleanup (see guards)";
      const expected = i === 0 ? "Static possible paths - target and timing unknown" : projection.noteOrder[i]?.display || "handlers / cleanup (see guards)";
      if (!note.textContent.replace(/\s+/g, "").includes(expected.replace(/\s+/g, ""))) unsupported("Changed Pele note text");
    }
    const observed = [...root.querySelectorAll(".pele-frame")].map(n => n.classList.contains("pele-frame-alt") ? "alt" : n.classList.contains("pele-frame-opt") ? "opt" : n.classList.contains("pele-frame-loop") ? "loop" : "unknown");
    if (observed.join("|") !== projection.frames.filter(x => x !== "rect").join("|") || root.querySelectorAll(".pele-rect").length !== projection.frames.filter(x => x === "rect").length) unsupported("Changed Pele control frames");
    // The alt frame has no native ID; its original header is the numbered note.
    return {root, notes};
  }
  async function render(container, view, readSource, expandedGroups = new Set(), options = {}) {
    const current = () => !options.isCurrent || options.isCurrent();
    if (!current()) return {status:"stale"};
    try {
      const projection = project(view, expandedGroups, options);
      if (typeof window.pele?.render !== "function") unsupported("Pele asset unavailable");
      let timer;
      let result;
      try {
        result = await Promise.race([
          Promise.resolve().then(() => window.pele.render(projection.source, {}).svg),
          new Promise((_, reject) => { timer = setTimeout(() => reject(new Error("Pele render timed out")), 5000); })
        ]);
      } finally { clearTimeout(timer); }
      if (!current()) return {status:"stale"};
      const {root, notes} = validatedSvg(result, projection);
      const selected = {id:options.selectedId || null};
      const controls = new Map();
      for (const [i, row] of projection.rows.entries()) {
        const note = notes[projection.noteOrder.indexOf(row)], step = row.step;
        note.setAttribute("data-trellis-step-id", row.id);
        if (row.callId) note.setAttribute("data-trellis-call-id", row.callId);
        if (row.previewEntryCallId) note.setAttribute("data-trellis-preview-call-id", row.previewEntryCallId);
        note.setAttribute("tabindex", "0"); note.setAttribute("role", "button");
        note.setAttribute("aria-label", `Inspect ${step.kind}: ${step.label}${row.previewEntryCallId ? `; first measured entry ${row.preview.label}, call ${row.previewEntryCallId}; remaining calls collapsed` : ""} (${step.path}:${step.range.startLine})`);
        note.setAttribute("aria-pressed", String(selected.id === row.id));
        const select = () => {
          if (!current() || !witness(step) || row.id !== step.id || row.callId !== step.callId || row.path !== step.path || row.range !== step.range || projection.rows[i] !== row) return;
          selected.id = row.id;
          for (const n of notes.slice(1)) n.setAttribute("aria-pressed", String(n.getAttribute("data-trellis-step-id") === row.id));
          if (options.onSelect) options.onSelect(step); else if (readSource) readSource(step);
        };
        note.addEventListener("click", select);
        note.addEventListener("keydown", e => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); select(); } });
        if (step.kind === "group") {
          const toggle = document.createElement("button"); toggle.type = "button"; toggle.className = "sequence-group-control";
          toggle.textContent = expandedGroups.has(row.id) ? "Collapse call chain" : "Expand call chain";
          toggle.setAttribute("aria-label", `${toggle.textContent}: ${step.label}`);
          toggle.setAttribute("aria-expanded", String(expandedGroups.has(row.id)));
          toggle.dataset.trellisStepId = row.id;
          toggle.addEventListener("click", async () => {
            if (!current() || projection.rows[i] !== row) return;
            const top = container.scrollTop, left = container.scrollLeft;
            if (expandedGroups.has(row.id)) expandedGroups.delete(row.id); else expandedGroups.add(row.id);
            const outcome = await render(container, view, readSource, expandedGroups, {...options, selectedId:selected.id});
            if (!current() || outcome.status !== "rendered") return;
            outcome.controls.get(row.id)?.focus({preventScroll:true}); container.scrollTop = top; container.scrollLeft = left;
          });
          controls.set(row.id, toggle);
        }
      }
      if (!current()) return {status:"stale"};
      const fragment = document.createDocumentFragment(); fragment.append(root); // Adopt the validated node itself; cloning would drop selection listeners.
      for (const button of controls.values()) fragment.append(button);
      container.replaceChildren(fragment);
      return {status:"rendered", controls, source:projection.source};
    } catch (error) {
      if (!current()) return {status:"stale"};
      return {status:"unsupported", reason:String(error.message || error)};
    }
  }
  window.TrellisPeleSequence = {render, project, validatedSvg};
})();
