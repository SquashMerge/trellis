"use strict";
// Presentation only. Source access stays in app.js and its guarded callbacks.
(() => {
  const get = id => document.getElementById(id);
  const text = (id, value) => { if (get(id)) get(id).textContent = value; };
  const hide = (id, value) => { if (get(id)) get(id).hidden = value; };
  const make = (tag, value) => { const node = document.createElement(tag); if (value !== undefined) node.textContent = value; return node; };
  const describe = value => typeof value === "string" ? value : JSON.stringify(value, null, 2);
  const views = {architecture:"architecture-panel", treemap:"treemap-panel", sequence:"sequence-panel", classes:"classes-panel", libraries:"dependency-library"};
  let openSource = null, sourceReturnFocus = null, activeView = "sequence", inspectorClearCallback = null, sectionCallback = null, viewCallback = null;
  function setSection(name) {
    const files = name === "files";
    window.TrellisNavigation?.reset(); window.TrellisClasses?.closeContextMenu?.();
    document.body.classList.toggle("files-section", files);
    document.body.classList.toggle("classes-view", !files && activeView === "classes");
    drawer("explorer", false); drawer("inspector", false);
    get("section-files")?.setAttribute("aria-current", files ? "page" : "false");
    get("section-diagrams")?.setAttribute("aria-current", files ? "false" : "page");
    hide("diagram-tabs", files);
    hide("diagram-metrics", files || ["architecture", "treemap"].includes(activeView));
    for (const [key, panel] of Object.entries(views)) hide(panel, files || key !== activeView);
    hide("files-inspector", !files); hide("files-source-toolbar", !files);
    for (const id of ["inspector-empty", "inspector-content", "metric-inspector", "architecture-inspector", "treemap-inspector"]) {
      if (files) hide(id, true);
    }
    hide("inspector-clear", files);
    if (files) {
      hide("source-dock", false); hide("workspace-source-panel", false); hide("library-source-panel", true);
      get("file-filter")?.focus();
    } else {
      hide("inspector-empty", !get("inspector-content")?.hidden || !get("metric-inspector")?.hidden || !get("architecture-inspector")?.hidden || !get("treemap-inspector")?.hidden);
      get(`view-${activeView}`)?.focus();
    }
  }
  function setSectionCallback(callback) { sectionCallback = callback; }
  function showFiles() {
    if (get("workspace")?.hidden) return;
    if (sectionCallback) return sectionCallback("files");
    setSection("files");
  }
  function showDiagrams() {
    if (get("workspace")?.hidden) return;
    if (sectionCallback) return sectionCallback("diagrams");
    setSection("diagrams");
  }
  function clearMetricInspector() {
    hide("metric-inspector", true);
    document.body.classList.remove("metric-inspector-active");
    drawer("inspector", false);
    for (const id of ["metric-inspector-title", "metric-inspector-scope", "metric-inspector-values", "metric-inspector-top", "metric-inspector-state"]) get(id)?.replaceChildren();
    hide("inspector-empty", document.body.classList.contains("files-section") || !get("inspector-content")?.hidden);
    if (get("inspector-clear")) get("inspector-clear").disabled = !!get("inspector-content")?.hidden;
  }
  function showMetricInspector() {
    clearTreemapInspector();
    clearArchitectureInspector();
    openSource = null;
    hide("inspector-empty", true); hide("inspector-content", true);
    if (get("inspector-open-source")) get("inspector-open-source").disabled = true;
    hide("metric-inspector", false);
    document.body.classList.add("metric-inspector-active");
    if (get("inspector-clear")) get("inspector-clear").disabled = false;
    drawer("inspector", true);
  }
  function clearArchitectureInspector() {
    const wasOpen = get("architecture-inspector") && !get("architecture-inspector").hidden;
    hide("architecture-inspector", true);
    if (wasOpen) {
      drawer("inspector", false);
      hide("inspector-empty", document.body.classList.contains("files-section"));
      if (get("inspector-clear")) get("inspector-clear").disabled = true;
    }
    get("architecture-inspector-title")?.replaceChildren();
    get("architecture-inspector-content")?.replaceChildren();
    document.body.classList.remove("architecture-inspector-active");
  }
  function showArchitectureInspector() {
    resetInspector();
    hide("inspector-empty", true); hide("architecture-inspector", false);
    document.body.classList.add("architecture-inspector-active");
    if (get("inspector-clear")) get("inspector-clear").disabled = false;
    drawer("inspector", true);
  }
  function clearTreemapInspector() {
    const wasOpen = get("treemap-inspector") && !get("treemap-inspector").hidden;
    hide("treemap-inspector", true);
    if (wasOpen) {
      drawer("inspector", false);
      hide("inspector-empty", document.body.classList.contains("files-section"));
      if (get("inspector-clear")) get("inspector-clear").disabled = true;
    }
    get("treemap-inspector-title")?.replaceChildren();
    get("treemap-inspector-content")?.replaceChildren();
  }
  function showTreemapInspector() {
    resetInspector();
    hide("inspector-empty", true); hide("treemap-inspector", false);
    if (get("inspector-clear")) get("inspector-clear").disabled = false;
    drawer("inspector", true);
  }
  function setViewCallback(callback) { viewCallback = callback; }
  function setInspectorClearCallback(callback) {
    inspectorClearCallback = typeof callback === "function" ? callback : null;
  }
  function drawer(name, open) {
    if (open && window.matchMedia?.("(max-width: 900px)").matches) drawer(name === "explorer" ? "inspector" : "explorer", false);
    document.body.classList.toggle(`${name}-open`, open);
    get(`${name}-toggle`)?.setAttribute("aria-expanded", String(open));
  }
  function showView(name) {
    if (!Object.hasOwn(views, name)) return;
    if (document.body.classList.contains("files-section")) showDiagrams();
    activeView = name;
    hide("diagram-metrics", ["architecture", "treemap"].includes(name));
    document.body.classList.toggle("classes-view", name === "classes");
    window.TrellisClasses?.closeContextMenu?.();
    if (name === "sequence" || name === "classes" || ["architecture", "treemap"].includes(name)) drawer("explorer", false);
    for (const [key, panel] of Object.entries(views)) {
      hide(panel, key !== name);
      const tab = get(`view-${key}`);
      tab?.setAttribute("aria-selected", String(key === name));
      if (tab) tab.tabIndex = key === name ? 0 : -1;
    }
    viewCallback?.(name);
  }
  function showSource(name) {
    if (!["workspace", "library"].includes(name)) return;
    if (name === "library" && document.body.classList.contains("files-section")) showDiagrams();
    if (!document.body.classList.contains("files-section") && !get("source-dock")?.contains?.(document.activeElement)) sourceReturnFocus = document.activeElement;
    hide("source-dock", false);
    for (const key of ["workspace", "library"]) {
      hide(`${key}-source-panel`, key !== name);
      get(`dock-${key}`)?.setAttribute("aria-selected", String(key === name));
      if (get(`dock-${key}`)) get(`dock-${key}`).tabIndex = key === name ? 0 : -1;
    }
    // Clear latent mobile drawers even on desktop: a later resize must not
    // place an old inspector/explorer over the open source dock.
    drawer("explorer", false); drawer("inspector", false);
    if (window.matchMedia?.("(max-width: 900px)").matches) {
      (document.body.classList.contains("files-section") ? get("source") : get(`dock-${name}`))?.focus();
    }
  }
  function visibleControl(node) {
    return node && !node.disabled && !node.hidden && !node.closest?.("[hidden]") && (!node.getClientRects || node.getClientRects().length > 0);
  }
  function closeSource() {
    if (document.body.classList.contains("files-section")) return;
    hide("source-dock", true);
    const target = visibleControl(sourceReturnFocus) ? sourceReturnFocus : get(`view-${activeView}`);
    target?.focus(); sourceReturnFocus = null;
  }
  function wireTabs(names, prefix, select) {
    names.forEach((name, index) => {
      const tab = get(prefix + name);
      tab?.addEventListener("click", () => select(name));
      tab?.addEventListener("keydown", event => {
        let next;
        if (event.key === "ArrowRight") next = (index + 1) % names.length;
        else if (event.key === "ArrowLeft") next = (index + names.length - 1) % names.length;
        else if (event.key === "Home") next = 0;
        else if (event.key === "End") next = names.length - 1;
        else return;
        event.preventDefault(); select(names[next]); get(prefix + names[next])?.focus();
      });
    });
  }
  function resetInspector() {
    clearTreemapInspector();
    clearArchitectureInspector();
    clearMetricInspector();
    openSource = null;
    drawer("inspector", false);
    hide("inspector-empty", document.body.classList.contains("files-section")); hide("inspector-content", true);
    for (const id of ["inspector-title", "inspector-kind", "inspector-target", "inspector-location", "inspector-evidence", "inspector-detail"]) get(id)?.replaceChildren();
    if (get("inspector-open-source")) get("inspector-open-source").disabled = true;
    if (get("inspector-clear")) get("inspector-clear").disabled = true;
    if (get("inspector-title")) get("inspector-title").title = "";
  }
  function targetText() { return "Terminal syntax only · no verified target"; }
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
  function location(step) {
    if (!sourceWitness(step)) return "Source range unavailable";
    const r = step.range;
    return `${step.path}:${r.startLine}${r.startColumn == null ? "" : ":" + r.startColumn}–${r.endLine}${r.endColumn == null ? "" : ":" + r.endColumn}`;
  }
  function selectStep(step, view, callback) {
    resetInspector();
    if (!step || !view) return;
    hide("inspector-empty", true); hide("inspector-content", false);
    if (get("inspector-clear")) get("inspector-clear").disabled = false;
    text("inspector-title", step.label || step.id || "Selected step");
    text("inspector-kind", step.kind || "Step");
    text("inspector-location", `${location(step)} · ${view.revision?.indexGeneration && Number.isSafeInteger(view.revision.indexRevision) ? `revision ${view.revision.indexGeneration.slice(0, 8)}:${view.revision.indexRevision}` : "snapshot unavailable"}`);
    const children = step.children || [];
    const flat = step.kind === "group" && !step.alternate?.length && children.length && children.every(child => child.kind === "call" && !child.hidden && !child.children?.length && !child.alternate?.length);
    const entry = flat ? children[0] : step;
    if (flat) text("inspector-title", `${entry.label || entry.id} · +${children.length - 1} chain calls`);
    if (get("inspector-title")) get("inspector-title").title = step.label || step.id || "";
    text("inspector-target", flat
      ? `First measured entry call only: ${targetText(entry, view, false)}. Other calls may have different receivers and return types.`
      : targetText(step, view, false));
    text("inspector-evidence", `Measured source steps, not a runtime trace or verified dispatch.`);
    const detail = get("inspector-detail");
    // Every original field and all nested guards, alternates and chain calls remain
    // available as safe text. Never infer equivalent guards or invent confidence.
    function evidence(item, parent, label) {
      const disclosure = make("details");
      disclosure.append(make("summary", `${label ? label + " · " : ""}${item.kind || "step"}: ${item.label || item.id || "Details"}`));
      disclosure.append(make("p", `${location(item)} · ${targetText(item, view)}`));
      const fields = Object.fromEntries(Object.entries(item).filter(([key]) => !["children", "alternate", "target", "resolution", "candidateSymbols", "callbackArguments"].includes(key)));
      disclosure.append(make("pre", JSON.stringify(fields, null, 2)));
      for (const child of item.children || []) evidence(child, disclosure, "Child");
      for (const child of item.alternate || []) evidence(child, disclosure, "Alternate");
      parent.append(disclosure);
      return disclosure;
    }
    if (detail) evidence(step, detail, "Measured syntax");
    openSource = sourceWitness(step) && view.revision?.indexGeneration && Number.isSafeInteger(view.revision.indexRevision) && typeof callback === "function" ? callback : null;
    if (get("inspector-open-source")) get("inspector-open-source").disabled = !openSource;
    drawer("inspector", true);
  }
  let checkoutRoot = "";
  function formatGit(git) {
    if (git?.state !== "available") return "Git unavailable";
    const head = git.head?.kind === "branch" ? git.head.name
      : git.head?.kind === "detached" && git.head.shortCommit ? `Detached HEAD ${git.head.shortCommit}` : null;
    if (!head) return "Git unavailable";
    return git.upstream
      ? `${head} · ${git.upstream.name} ↑${git.upstream.ahead} ↓${git.upstream.behind} · As of last fetch`
      : `${head} · No upstream`;
  }
  function updateCheckout(root, git) {
    checkoutRoot = root || "";
    const label = checkoutRoot ? formatGit(git) : "";
    text("workspace-git", label);
    hide("workspace-git", !checkoutRoot);
    if (get("workspace-git")) get("workspace-git").title = label;
    text("workspace-name", checkoutRoot.split(/[\\/]/).filter(Boolean).at(-1) || "No workspace");
    if (get("workspace-name")) get("workspace-name").title = checkoutRoot;
  }
  function updateWorkspace(status) {
    const root = status?.workspaceRoot || checkoutRoot;
    text("workspace-name", root.split(/[\\/]/).filter(Boolean).at(-1) || "No workspace");
    text("workspace-meta", status ? `Revision ${status.revision.indexGeneration.slice(0, 8)}:${status.revision.indexRevision} · ${status.stats?.files ?? 0} files · ${"Syntax only · SCIP display metadata is not proof"}` : checkoutRoot ? "Index status unavailable" : "Connect to a local daemon");
    if (get("workspace-name")) get("workspace-name").title = root;
    const state = status ? (status.catchingUp === true ? "catching-up" : "fresh") : "unknown";
    get("workspace-index-state")?.setAttribute("data-state", state);
    text("workspace-index-label", status ? `${status.catchingUp === true ? "Catching up" : "Fresh"} · revision ${status.revision.indexGeneration.slice(0, 8)}:${status.revision.indexRevision}` : checkoutRoot ? "Index status unavailable" : "No checkout selected");
  }
  function reset() {
    setSection("diagrams"); resetInspector(); showView("sequence"); hide("source-dock", true); sourceReturnFocus = null;
    drawer("explorer", false); drawer("inspector", false); updateCheckout(null, null); updateWorkspace(null);
  }
  function setConnected(connected) {
    document.body.classList.toggle("connected", !!connected);
    for (const id of ["section-diagrams", "section-files"]) if (get(id)) get(id).disabled = !connected;
    if (!connected) reset();
  }
  wireTabs(Object.keys(views), "view-", showView);
  wireTabs(["workspace", "library"], "dock-", showSource);
  get("inspector-open-source")?.addEventListener("click", () => { if (openSource) return openSource(); });
  get("inspector-clear")?.addEventListener("click", () => { resetInspector(); inspectorClearCallback?.(); });
  get("section-diagrams")?.addEventListener("click", showDiagrams);
  get("section-files")?.addEventListener("click", showFiles);
  get("source-dock-close")?.addEventListener("click", closeSource);
  for (const name of ["explorer", "inspector"]) get(`${name}-toggle`)?.addEventListener("click", () => drawer(name, !document.body.classList.contains(`${name}-open`)));
  document.addEventListener("keydown", event => {
    if (event.key !== "Escape") return;
    const mobile = window.matchMedia?.("(max-width: 900px)").matches;
    if (mobile && document.body.classList.contains("inspector-open")) { drawer("inspector", false); get("inspector-toggle")?.focus(); }
    else if (mobile && document.body.classList.contains("explorer-open")) { drawer("explorer", false); get("explorer-toggle")?.focus(); }
    else if (get("source-dock") && !get("source-dock").hidden) closeSource();
  });
  window.TrellisShell = {showTreemapInspector, clearTreemapInspector,showArchitectureInspector, clearArchitectureInspector, setViewCallback, setSection, setSectionCallback, setConnected, updateCheckout, formatGit, updateWorkspace, showView, showSource, closeSource, showFiles, showDiagrams, showMetricInspector, clearMetricInspector, setInspectorClearCallback, selectStep, resetInspector, reset};
  reset();
})();
