"use strict";
// History entries and link routing for the diagram views, source and the Deviations tab. Declarations only; app.js wires them.
// History owns only opaque keys. Validated source/diagram evidence stays in this session.
const navigationEntries = new Map();
const navigationScope = String(Math.random()).slice(2);
let navigationKey = 0, navigationSerial = 0, navigationLocation = null;
let sourceInspector = null;
let routeViewChange = false;
function showNavigationView(name) {
  routeViewChange = true;
  try { window.TrellisShell?.showView(name); } finally { routeViewChange = false; }
}
function clearNavigationHistory() {
  navigationSerial++; navigationEntries.clear(); navigationLocation = null; sourceInspector = null;
}
function navigationContext() {
  const session = epoch, root = selectedRootKey, revision = status?.revision && IndexPin.copy(status.revision);
  return {session, root, revision};
}
function navigationCurrent(context) {
  return !!token && context.session === epoch && context.root === selectedRootKey && IndexPin.equal(context.revision, status?.revision);
}
function navigationError(message) {
  $("sequence-state").textContent = message;
  $("error").textContent = message; $("error").hidden = false;
}
function rememberNavigation(entry, replace, hash) {
  if (!window.history?.pushState || !window.history?.replaceState) return;
  const key = `${navigationScope}:${++navigationKey}`;
  navigationEntries.set(key, {...entry, hash, ...navigationContext()});
  // An entry with no hash names the page itself; an empty URL would keep the current hash.
  window.history[replace ? "replaceState" : "pushState"]({trellisNavigation:key}, "", hash || `${window.location?.pathname || ""}${window.location?.search || ""}`);
  navigationLocation = navigationSignature();
}
function navigationSignature() {
  return `${window.location?.hash || ""}:${window.history?.state?.trellisNavigation || ""}`;
}
function captureSourceEntry(view = currentDiagramView()) {
  if (!workspaceSource || !IndexPin.equal(workspaceSource.data.revision, status?.revision) ||
      $("source-dock")?.hidden || $("workspace-source-panel")?.hidden || (section === "files" && filesBasis !== "indexed")) return null;
  const focus = document.activeElement;
  const action = focus?.closest?.(".source-diagram-action");
  const offsets = id => ({top:$(id)?.scrollTop || 0, left:$(id)?.scrollLeft || 0});
  return {kind:"source", source:workspaceSource, sourceNavigation:workspaceSource.navigation?.snapshot?.(), section, method:selectedMethod, diagram:diagramSequence,
    view,
    showAll:!!$("all-steps").checked, boundary:sequenceBoundary, inspector:sourceInspector,
    file:filesFile, symbol:filesSymbol, basis:filesBasis, mini:filesMini, matches:filesMatches,
    line:Number($("source").dataset?.navigationLine) || workspaceSource.range?.startLine || 1,
    focus:action ? {symbol:action.dataset.symbolId} : {id:focus?.id || "source"},
    scroll:{source:offsets("source"), panel:offsets("workspace-source-panel"), dock:offsets("source-dock"),
      x:window.scrollX || 0, y:window.scrollY || 0}};
}
const SEQUENCE_LINK_CALL_IDS = 50, CALL_ID = /^occ:v2:[0-9a-f]{32}$/;
function sequenceHash(id, revision, boundary = null) {
  const query = new URLSearchParams({sid:id, rootKey:selectedRootKey, indexGeneration:revision.indexGeneration, indexRevision:String(revision.indexRevision)});
  const callIds = boundary?.symbolId === id ? boundary.callIds : [];
  if (callIds.length >= 1 && callIds.length <= SEQUENCE_LINK_CALL_IDS) for (const callId of callIds) query.append("callId", callId);
  return `#sequence?${query}`;
}
// Replaces the current history entry with what is on screen, so Back returns to it.
function rememberCurrent(revision, architectureEntry = null) {
  writeEntry(currentEntry(revision, architectureEntry));
}
// What is on screen as a history entry, or null when the current entry already describes it.
// A line selected in the Deviations tab is that tab's entry, under its link (#965).
function currentEntry(revision, architectureEntry = null, view = currentDiagramView()) {
  const source = captureSourceEntry(view);
  if (architectureEntry) return {entry:{kind:"architecture", architecture:architectureEntry},
    hash:architectureEntry.inspection?.kind === "deviation" ? deviationsHash({app:architectureEntry.scope ?? null, edge:architectureEntry.deviations?.chip ?? null}) : "#architecture"};
  if (source) return {entry:source, hash:"#source"};
  if (diagramSequence && selectedMethod?.id === diagramSequence.seed.id && IndexPin.equal(diagramSequence.revision, revision))
    return {entry:{kind:"sequence", symbol:selectedMethod, admitted:diagramSequence, showAll:!!$("all-steps").checked, boundary:sequenceBoundary}, hash:sequenceHash(selectedMethod.id, revision, sequenceBoundary)};
  if (!window.location?.hash) return {entry:{kind:"unavailable"}, hash:""};
  return null;
}
function writeEntry(saved) { if (saved) rememberNavigation(saved.entry, true, saved.hash); }
async function navigateDiagram(symbol, revision, kind = "sequence", admitted = null, showAll = false, boundary = null, architectureEntry = null, remembered = false) {
  IndexPin.copy(revision);
  if (!IndexPin.equal(revision, status?.revision) || !token || !selectedRootKey) throw new Error("Source navigation is stale. Refresh the indexed view.");
  const context = navigationContext(), ticket = ++navigationSerial;
  const current = () => ticket === navigationSerial && navigationCurrent(context);
  diagramSerial++;
  if (!remembered) rememberCurrent(revision, architectureEntry);
  rememberNavigation({kind, symbol, admitted, showAll, boundary}, false, kind === "sequence" ? sequenceHash(symbol.id, revision, boundary) : "#classes");
  window.TrellisClasses?.reset();
  clearSource(); diagramSequence = null;
  await setSection("diagrams", kind === "class" ? "classes" : "sequence");
  if (!current()) return;
  $("all-steps").checked = showAll;
  if (kind === "class") return openClasses({seed:symbol.id});
  return selectMethod(symbol, admitted, current, boundary);
}
async function restoreSourceEntry(entry, current) {
  // Restore the existing owners before normal section/source rendering rebinds them.
  filesSerial++; filesDiffSerial++; clearFilesInspector();
  filesFile = entry.file; filesSymbol = entry.symbol; filesBasis = entry.basis;
  filesMini = entry.mini; filesMatches = entry.matches; filesDiff = null;
  selectedMethod = entry.method; diagramSequence = null; sequenceBoundary = entry.boundary; $("all-steps").checked = entry.showAll;
  $("method-source").disabled = !selectedMethod;
  if ($("method-class")) $("method-class").disabled = !selectedMethod || !classLanguage(selectedMethod.path);
  workspaceSource = entry.source;
  await setSection(entry.section, entry.section === "diagrams" ? entry.view : null);
  if (!current()) return;
  diagramSequence = entry.diagram;
  if (entry.section === "diagrams" && entry.view === "sequence" && entry.diagram && entry.method) {
    await loadSequence(entry.diagram, true, current);
    if (!current()) return;
  }
  if (entry.section === "diagrams") showNavigationView(entry.view);
  // Source opened over the Deviations tab returns to the tab's tree (#965).
  if (entry.section === "diagrams" && entry.view === "deviations") void architecture?.showDeviations();
  updateFilesBasis(); renderFiles();
  if (entry.section === "files" && filesSymbol) {
    renderFilesSymbolHeader(filesSymbol);
    if (filesMatches) renderFilesMatches();
    if (filesMini) await renderFilesMini(filesGuard({symbol:true}));
    if (!current()) return;
  }
  const serial = ++sourceSerial;
  const sourceCurrent = () => current() && sourceSerial === serial && !(section === "files" && filesBasis !== "indexed");
  paintCachedSource(entry.source.data, entry.source.range, sourceCurrent, entry.line, entry.sourceNavigation);
  sourceInspector = entry.inspector;
  if (entry.inspector) {
    const inspector = entry.inspector;
    const readSource = () => {
      if (navigationCurrent(entry) && sourceInspector === inspector && sourceSerial === serial)
        return perform(() => showSource(inspector.symbol, entry.revision));
    };
    if (inspector.sequence) window.TrellisShell?.selectStep(inspector.symbol, inspector.sequence, readSource, sequenceTargetAction(inspector.sequence, () => navigationCurrent(entry) && sourceInspector === inspector));
    else window.TrellisShell?.inspectSymbol?.(inspector.symbol, entry.revision, readSource);
  }
  if (!sourceCurrent()) return;
  // Rendering and highlight reveal finish before exact focus/scroll restoration.
  await new Promise(resolve => window.requestAnimationFrame ? window.requestAnimationFrame(resolve) : resolve());
  if (!sourceCurrent()) return;
  const actions = $("source").querySelectorAll?.(".source-diagram-action") || [];
  const focus = entry.focus.symbol ? Array.from(actions).find(node => node.dataset.symbolId === entry.focus.symbol) : $(entry.focus.id);
  (focus || $("source")).focus({preventScroll:true});
  for (const [id, offset] of [["source", entry.scroll.source], ["workspace-source-panel", entry.scroll.panel], ["source-dock", entry.scroll.dock]]) {
    if ($(id)) { $(id).scrollTop = offset.top; $(id).scrollLeft = offset.left; }
  }
  window.scrollTo?.(entry.scroll.x, entry.scroll.y);
}
async function readNavigationLocation(force = false) {
  const signature = navigationSignature();
  if (!force && signature === navigationLocation) return;
  navigationLocation = signature;
  hideAgentNote();
  const ticket = ++navigationSerial; diagramSerial++; sourceSerial++;
  window.TrellisClasses?.reset();
  // A URL never authenticates or chooses a checkout. Explicit selection retries it.
  if (!token || !selectedRootKey || !status?.revision) return;
  const context = navigationContext(), current = () => ticket === navigationSerial && navigationCurrent(context);
  const candidate = navigationEntries.get(window.history?.state?.trellisNavigation);
  const entry = candidate?.hash === (window.location?.hash || "") ? candidate : null;
  // A stale history entry or link strips the URL, adding no entry, and opens the root view
  // with no error (#951, owner ruling 2026-11-12). Malformed and other-checkout links still error.
  const openRoot = async () => {
    window.history?.replaceState?.(null, "", `${window.location?.pathname || ""}${window.location?.search || ""}`);
    navigationLocation = navigationSignature();
    $("error").hidden = true; $("error").textContent = "";
    // Sequence starts empty, as on a fresh load: no selected method, enabled method actions or link error.
    clearSource(); diagramSequence = null; sequenceBoundary = null; selectedMethod = null;
    $("method-source").disabled = true; if ($("method-class")) $("method-class").disabled = true;
    $("sequence-diagram").replaceChildren(); $("sequence-warnings").replaceChildren();
    $("sequence-state").textContent = "Expand a file and select a method.";
    renderFiles(); renderSequenceStart();
    await setSection("diagrams");
    if (!current()) return;
    showNavigationView("architecture");
  };
  try {
    if (entry) {
      // A stale entry, a Deviations tab entry (#965) included, opens the root view.
      if (!navigationCurrent(entry)) return await openRoot();
      if (entry.kind === "unavailable") return navigationError("This history entry has no saved source view. The current diagram remains open; use Forward to return to its link.");
      $("error").hidden = true;
      if (entry.kind === "source") return await restoreSourceEntry(entry, current);
      if (entry.kind === "view") {
        // A view with no saved selection (#818): Architecture, Treemap or Classes, as shown.
        clearSource(); diagramSequence = null; sequenceBoundary = null;
        await setSection("diagrams");
        if (!current()) return;
        return showNavigationView(entry.view);
      }
      if (entry.kind === "deviations") return await showDeviationsRoute(entry.filter, current);
      if (entry.kind === "architecture") {
        clearSource(); diagramSequence = null; sequenceBoundary = null;
        await setSection("diagrams");
        if (!current()) return;
        showNavigationView(entry.architecture.inspection?.kind === "deviation" ? "deviations" : "architecture");
        return await architecture.restore(entry.architecture, current);
      }
      clearSource(); diagramSequence = null;
      await setSection("diagrams", entry.kind === "class" ? "classes" : "sequence");
      if (!current()) return;
      if (entry.kind === "class") return await openClasses({seed:entry.symbol.id});
      $("all-steps").checked = entry.showAll;
      return await selectMethod(entry.symbol, entry.admitted, current, entry.boundary);
    }
    const hash = window.location?.hash || "";
    const oldKey = window.history?.state?.trellisNavigation;
    const deviationsLink = hash === "#deviations" || hash.startsWith("#deviations?");
    // An entry an index or checkout change dropped, a Deviations tab entry (#965) included, opens the root view.
    if (typeof oldKey === "string" && oldKey.startsWith(`${navigationScope}:`)) return await openRoot();
    if (!hash) return;
    // A Deviations tab link (#965): the tab at the shown snapshot, on its app and edge.
    if (deviationsLink) {
      const query = new URLSearchParams(hash.slice("#deviations?".length));
      const filter = {app:query.get("app"), edge:query.get("edge")};
      rememberNavigation({kind:"deviations", filter}, true, hash);
      return await showDeviationsRoute(filter, current);
    }
    if (!hash.startsWith("#sequence")) {
      if (hash === "#source" || hash === "#classes" || window.history?.state?.trellisNavigation) return await openRoot();
      return;
    }
    if (!hash.startsWith("#sequence?")) throw new Error("Sequence link requires a symbol ID (sid).");
    const query = new URLSearchParams(hash.slice("#sequence?".length)), id = query.get("sid");
    if (!id || !id.trim() || query.getAll("sid").length !== 1) throw new Error("Sequence link requires one valid symbol ID (sid).");
    if (query.has("rootKey") && query.get("rootKey") !== selectedRootKey) throw new Error("Sequence link belongs to a different checkout. Select its checkout first.");
    const revision = context.revision;
    let requested = null;
    if (query.has("indexGeneration") || query.has("indexRevision")) {
      const value = query.get("indexRevision");
      if (!/^(0|[1-9][0-9]*)$/.test(value || "")) throw new Error("Sequence link has an invalid index snapshot.");
      requested = IndexPin.copy({indexGeneration:query.get("indexGeneration"), indexRevision:Number(value)});
    }
    const callIds = query.getAll("callId");
    if (callIds.length && (callIds.length > SEQUENCE_LINK_CALL_IDS || new Set(callIds).size !== callIds.length || !callIds.every(callId => CALL_ID.test(callId)) ||
        !query.has("rootKey") || !query.has("indexGeneration") || !query.has("indexRevision"))) throw new Error("Sequence link has invalid call IDs.");
    // Only a well-formed link for this checkout can be stale.
    if (requested && !IndexPin.equal(requested, revision)) return await openRoot();
    const boundary = callIds.length ? {symbolId:id, callIds} : null;
    clearSource(); diagramSequence = null;
    await setSection("diagrams", "sequence");
    if (!current()) return;
    window.TrellisShell?.showView("sequence");
    $("sequence-state").textContent = "Loading sequence link…";
    const data = await api("/api/sequence", "POST", {seed:id, expectedRevision:revision, showAll:false, interactions:true}, current);
    if (!current()) return;
    if (!IndexPin.equal(data?.revision, revision) || data?.seed?.id !== id) throw new Error("Sequence link provenance mismatch. Nothing displayed.");
    $("all-steps").checked = false;
    // A resolved route stores only a key in browser history; seed/DTO remain in memory.
    rememberNavigation({kind:"sequence", symbol:data.seed, admitted:data, showAll:false, boundary}, true, hash);
    await selectMethod(data.seed, data, current, boundary);
  } catch (error) {
    if (current() && error.name !== "AbortError") navigationError(`Cannot open sequence: ${error.message}`);
  }
}
// The Deviations tab (#965). Its link names the app and the tree's edge; no pin, so a reload shows the
// tab at the head. Entering or leaving it by a tab, an entry point or Show on diagram adds a history
// entry; a filter chosen inside it replaces the tab's own.
function deviationsHash(filter) {
  const query = new URLSearchParams();
  if (typeof filter?.app === "string") query.set("app", filter.app);
  if (typeof filter?.edge === "string") query.set("edge", filter.edge);
  const text = String(query);
  return text ? `#deviations?${text}` : "#deviations";
}
async function showDeviationsRoute(filter, current) {
  clearSource(); diagramSequence = null; sequenceBoundary = null;
  await setSection("diagrams");
  if (!current()) return;
  $("error").hidden = true;
  showNavigationView("deviations");
  return await architecture?.showDeviations(filter);
}
// The entry for what a view shows, written before another view's entry is added.
function viewEntry(revision, view) {
  if (view === "deviations") { const filter = architecture?.deviationsFilter() ?? {app:null, edge:null}; return {entry:{kind:"deviations", filter}, hash:deviationsHash(filter)}; }
  const saved = visibleEntry(revision, view);
  return saved?.entry.kind === "unavailable" ? {entry:{kind:"view", view}, hash:""} : saved;
}
function openDeviationsTab(filter) {
  const pin = status?.revision;
  if (section !== "diagrams" || !pin) return;
  const from = shownView;
  navigationSerial++; diagramSerial++;
  if (from !== "deviations") writeEntry(viewEntry(pin, from));
  rememberNavigation({kind:"deviations", filter}, from === "deviations", deviationsHash(filter));
  showNavigationView("deviations");
  return architecture?.showDeviations(filter);
}
function showDiagramTab() {
  if (section !== "diagrams" || !status?.revision) return;
  navigationSerial++; diagramSerial++;
  rememberNavigation({kind:"view", view:"architecture"}, shownView !== "deviations", "");
  showNavigationView("architecture");
}
function deviationsFilterChanged(filter) {
  if (section === "diagrams" && shownView === "deviations" && status?.revision) rememberNavigation({kind:"deviations", filter}, true, deviationsHash(filter));
}
