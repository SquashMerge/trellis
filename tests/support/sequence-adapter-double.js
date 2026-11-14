"use strict";
// Test double for the app harnesses that have no DOM parser (browse-ui, shell-ui, files-ui).
// It runs the real Pele projection (rows, lanes, caps, refusals) and, in place of Pele's SVG,
// installs one selectable node per projected row. Load it after web/sequence-pele.js.
(() => {
  const real = window.TrellisPeleSequence, SVG = "http://www.w3.org/2000/svg";
  async function render(container, view, readSource, expandedGroups = new Set(), options = {}) {
    const current = () => !options.isCurrent || options.isCurrent();
    if (!current()) return {status:"stale"};
    const owned = window.TrellisSequence.beginRender(container), active = () => current() && owned();
    let projection;
    try { projection = real.project(view, expandedGroups, options); }
    catch (error) { return {status:"unsupported", reason:String(error.message || error)}; }
    const svg = document.createElementNS(SVG, "svg");
    for (const row of projection.rows) {
      const node = document.createElementNS(SVG, "g");
      node.setAttribute("class", "pele-note"); node.setAttribute("role", "button");
      node.setAttribute("data-trellis-step-id", row.id);
      if (row.callId) node.setAttribute("data-trellis-call-id", row.callId);
      node.textContent = row.display;
      node.addEventListener("click", () => {
        if (!active()) return;
        if (options.onSelect) options.onSelect(row.step); else if (readSource) readSource(row.step);
      });
      svg.append(node);
    }
    container.replaceChildren(svg);
    options.onRendered?.();
    return {status:"rendered", controls:new Map(), source:projection.source};
  }
  window.TrellisPeleSequence = {...real, render};
})();
