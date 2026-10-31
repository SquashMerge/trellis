"use strict";
const $ = id => document.getElementById(id);
let token = "", epoch = 0, selectedRootKey = null, selectedWorkspaceRoot = null, selectedGit = null, gitSerial = 0, checkoutBusy = false, checkouts = [], sourceSerial = 0;
const pendingRequests = new Set();
let status = null, statusUnavailable = false, jobUnavailable = false, jobObservation = 0, indexNotReady = false, staleMessage = "";
let statusSerial = 0, statusRefreshDepth = 0, pairRefreshPending = false, pairRefreshObserved = null;
let pairRefreshQueued = null, pairRefreshFollowup = false;
let job = null, pollTimer = null, waitingForJob = false;
const IndexPin = Object.freeze({
  copy(value) {
    if (!value || typeof value !== "object" || Array.isArray(value) ||
        !/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(value.indexGeneration) ||
        !Number.isSafeInteger(value.indexRevision) || value.indexRevision < 0) throw new Error("Invalid index snapshot pair. Refresh status.");
    return Object.freeze({indexGeneration:value.indexGeneration, indexRevision:value.indexRevision});
  },
  equal(a, b) {
    return (a == null && b == null) || (!!a && !!b && typeof a.indexGeneration === "string" && typeof b.indexGeneration === "string" &&
      Number.isSafeInteger(a.indexRevision) && Number.isSafeInteger(b.indexRevision) &&
      a.indexGeneration === b.indexGeneration && a.indexRevision === b.indexRevision);
  },
  key(value) { const pin = this.copy(value); return `${pin.indexGeneration}:${pin.indexRevision}`; },
  query(value) { const pin = this.copy(value); return `indexGeneration=${encodeURIComponent(pin.indexGeneration)}&indexRevision=${pin.indexRevision}`; },
  label(value) { const pin = this.copy(value); return `${pin.indexGeneration.slice(0, 8)}:${pin.indexRevision}`; },
  isConflict(error) { return error?.status === 409 && error.code === "revision_conflict"; },
});
window.TrellisIndexPin = IndexPin;
const objectValue = value => !!value && typeof value === "object" && !Array.isArray(value);
const sourceCache = new Map();
let headObservation = null;
function resetHeadObservation() {
  headObservation = null;
  $("retained-pin-status").hidden = true;
  $("retained-pin-label").textContent = "";
}
function showHeadObservation(observation) {
  const {pin, head, failed} = observation;
  const rebuilt = head && head.indexGeneration !== pin.indexGeneration;
  const older = head && !rebuilt && head.indexRevision > pin.indexRevision;
  $("retained-pin-status").hidden = !older && !rebuilt && !failed;
  $("retained-pin-label").textContent = older
    ? `Viewing revision ${IndexPin.label(pin)} · Older than head ${IndexPin.label(head)}`
    : rebuilt ? `Viewing revision ${IndexPin.label(pin)} · Index rebuilt. Reload at head.`
      : failed ? `Viewing revision ${IndexPin.label(pin)} · Head comparison unavailable.` : "";
  renderIndexStatus();
}
function observePinnedHead() {
  if (!token || !selectedRootKey || !status?.revision || !status.workspaceRoot) return;
  const pin = IndexPin.copy(status.revision), session = epoch, root = selectedRootKey, workspace = status.workspaceRoot;
  const current = () => session === epoch && root === selectedRootKey && workspace === status?.workspaceRoot && IndexPin.equal(pin, status?.revision);
  if (!headObservation || !currentHeadContext(headObservation, session, root, workspace, pin)) {
    headObservation = {session, root, workspace, pin, head:null, pending:false, again:false, failed:false};
  }
  const observation = headObservation;
  if (observation.pending) { observation.again = true; return; }
  observation.pending = true;
  void (async () => {
    try {
      for (let attempt = 0; attempt < 2; attempt++) {
        observation.again = false;
        try {
          const data = await api("/api/status", "GET", undefined, () => current() && headObservation === observation);
          if (!current() || headObservation !== observation) return;
          if (data.workspaceRoot !== workspace || !data.revision) throw new Error("Head unavailable");
          const head = IndexPin.copy(data.revision);
          // A later response cannot erase a newer same-generation observation.
          if (!observation.head || head.indexGeneration !== observation.head.indexGeneration || head.indexRevision >= observation.head.indexRevision) observation.head = head;
          observation.failed = false;
        } catch (error) {
          if (!current() || headObservation !== observation || error.name === "AbortError") return;
          observation.failed = true;
        }
        showHeadObservation(observation);
        if (!observation.again || !current() || headObservation !== observation) break;
      }
    } finally { observation.pending = false; }
  })();
}
function currentHeadContext(value, session, root, workspace, pin) {
  return value.session === session && value.root === root && value.workspace === workspace && IndexPin.equal(value.pin, pin);
}
function observePinnedResponse(path, method, body, data) {
  const query = new URLSearchParams(path.split("?", 2)[1] || "");
  const route = path.split("?", 1)[0];
  const reads = new Set(["/api/files", "/api/methods", "/api/source", "/api/symbols", "/api/classes", "/api/metrics", "/api/architecture"]);
  const posts = new Set(["/api/sequence", "/api/syntax-matches", "/api/class-diagram", "/api/navigation"]);
  const requested = method === "GET" && reads.has(route) && query.has("indexGeneration") && query.has("indexRevision")
    ? {indexGeneration:query.get("indexGeneration"), indexRevision:Number(query.get("indexRevision"))}
    : method === "POST" && posts.has(route) ? body?.expectedRevision : null;
  const returned = ["/api/metrics", "/api/architecture"].includes(route) ? data?.payload : data?.revision;
  if (requested && IndexPin.equal(requested, status?.revision) && IndexPin.equal(returned, requested)) observePinnedHead();
}

let section = "diagrams", sectionSerial = 0, filesSerial = 0, filesDiffSerial = 0, filesMatchesSerial = 0;
let filesFile = null, filesSymbol = null, filesBasis = "indexed", filesMini = null, filesMatches = null;
let filesGitStatus = null, filesDiff = null, filesCatalogLoaded = false;
let diagramSequence = null;

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
  window.history[replace ? "replaceState" : "pushState"]({trellisNavigation:key}, "", hash);
  navigationLocation = navigationSignature();
}
function navigationSignature() {
  return `${window.location?.hash || ""}:${window.history?.state?.trellisNavigation || ""}`;
}
function captureSourceEntry() {
  if (!workspaceSource || !IndexPin.equal(workspaceSource.data.revision, status?.revision) ||
      $("source-dock")?.hidden || $("workspace-source-panel")?.hidden || (section === "files" && filesBasis !== "indexed")) return null;
  const focus = document.activeElement;
  const action = focus?.closest?.(".source-diagram-action");
  const offsets = id => ({top:$(id)?.scrollTop || 0, left:$(id)?.scrollLeft || 0});
  return {kind:"source", source:workspaceSource, sourceNavigation:workspaceSource.navigation?.snapshot?.(), section, method:selectedMethod, diagram:diagramSequence,
    view:["sequence", "classes", "architecture", "treemap"].find(name => $("view-" + name)?.getAttribute?.("aria-selected") === "true") || "sequence",
    showAll:!!$("all-steps").checked, inspector:sourceInspector,
    file:filesFile, symbol:filesSymbol, basis:filesBasis, mini:filesMini, matches:filesMatches,
    line:Number($("source").dataset?.navigationLine) || workspaceSource.range?.startLine || 1,
    focus:action ? {symbol:action.dataset.symbolId} : {id:focus?.id || "source"},
    scroll:{source:offsets("source"), panel:offsets("workspace-source-panel"), dock:offsets("source-dock"),
      x:window.scrollX || 0, y:window.scrollY || 0}};
}
function sequenceHash(id, revision) {
  const query = new URLSearchParams({sid:id, rootKey:selectedRootKey, indexGeneration:revision.indexGeneration, indexRevision:String(revision.indexRevision)});
  return `#sequence?${query}`;
}
async function navigateDiagram(symbol, revision, kind = "sequence", admitted = null) {
  IndexPin.copy(revision);
  if (!IndexPin.equal(revision, status?.revision) || !token || !selectedRootKey) throw new Error("Source navigation is stale. Refresh the indexed view.");
  const context = navigationContext(), ticket = ++navigationSerial;
  const current = () => ticket === navigationSerial && navigationCurrent(context);
  diagramSerial++;
  const source = captureSourceEntry();
  if (source) rememberNavigation(source, true, "#source");
  else if (diagramSequence && selectedMethod?.id === diagramSequence.seed.id && IndexPin.equal(diagramSequence.revision, revision)) {
    rememberNavigation({kind:"sequence", symbol:selectedMethod, admitted:diagramSequence, showAll:!!$("all-steps").checked}, true, sequenceHash(selectedMethod.id, revision));
  } else if (!window.location?.hash) rememberNavigation({kind:"unavailable"}, true, "");
  rememberNavigation({kind, symbol, admitted, showAll:false}, false, kind === "sequence" ? sequenceHash(symbol.id, revision) : "#classes");
  window.TrellisClasses?.reset();
  clearSource(); diagramSequence = null;
  await setSection("diagrams", kind === "class" ? "classes" : "sequence");
  if (!current()) return;
  $("all-steps").checked = false;
  if (kind === "class") return openClasses({seed:symbol.id});
  return selectMethod(symbol, admitted, current);
}
async function restoreSourceEntry(entry, current) {
  // Restore the existing owners before normal section/source rendering rebinds them.
  filesSerial++; filesDiffSerial++; clearFilesInspector();
  filesFile = entry.file; filesSymbol = entry.symbol; filesBasis = entry.basis;
  filesMini = entry.mini; filesMatches = entry.matches; filesDiff = null;
  selectedMethod = entry.method; diagramSequence = null; $("all-steps").checked = entry.showAll;
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
  const ticket = ++navigationSerial; diagramSerial++; sourceSerial++;
  window.TrellisClasses?.reset();
  // A URL never authenticates or chooses a checkout. Explicit selection retries it.
  if (!token || !selectedRootKey || !status?.revision) return;
  const context = navigationContext(), current = () => ticket === navigationSerial && navigationCurrent(context);
  const candidate = navigationEntries.get(window.history?.state?.trellisNavigation);
  const entry = candidate?.hash === (window.location?.hash || "") ? candidate : null;
  try {
    if (entry) {
      if (!navigationCurrent(entry)) throw new Error("This history entry is stale. Select source from the current snapshot.");
      if (entry.kind === "unavailable") return navigationError("This history entry has no saved source view. The current diagram remains open; use Forward to return to its link.");
      $("error").hidden = true;
      if (entry.kind === "source") return await restoreSourceEntry(entry, current);
      clearSource(); diagramSequence = null;
      await setSection("diagrams", entry.kind === "class" ? "classes" : "sequence");
      if (!current()) return;
      if (entry.kind === "class") return await openClasses({seed:entry.symbol.id});
      $("all-steps").checked = entry.showAll;
      return await selectMethod(entry.symbol, entry.admitted, current);
    }
    const hash = window.location?.hash || "";
    const oldKey = window.history?.state?.trellisNavigation;
    if (typeof oldKey === "string" && oldKey.startsWith(`${navigationScope}:`)) throw new Error("This history entry is stale. Open source from the current snapshot.");
    if (!hash) return;
    if (!hash.startsWith("#sequence")) {
      if (hash === "#source" || hash === "#classes" || window.history?.state?.trellisNavigation) throw new Error("This source history is no longer available. Open source from the current snapshot.");
      return;
    }
    if (!hash.startsWith("#sequence?")) throw new Error("Sequence link requires a symbol ID (sid).");
    const query = new URLSearchParams(hash.slice("#sequence?".length)), id = query.get("sid");
    if (!id || !id.trim() || query.getAll("sid").length !== 1) throw new Error("Sequence link requires one valid symbol ID (sid).");
    if (query.has("rootKey") && query.get("rootKey") !== selectedRootKey) throw new Error("Sequence link belongs to a different checkout. Select its checkout first.");
    const revision = context.revision;
    if (query.has("indexGeneration") || query.has("indexRevision")) {
      const value = query.get("indexRevision");
      if (!/^(0|[1-9][0-9]*)$/.test(value || "")) throw new Error("Sequence link has an invalid index snapshot.");
      const requested = IndexPin.copy({indexGeneration:query.get("indexGeneration"), indexRevision:Number(value)});
      if (!IndexPin.equal(requested, revision)) throw new Error("Sequence link is stale for the selected index snapshot. Reload or select a current link.");
    }
    clearSource(); diagramSequence = null;
    await setSection("diagrams", "sequence");
    if (!current()) return;
    window.TrellisShell?.showView("sequence");
    $("sequence-state").textContent = "Loading sequence link…";
    const data = await api("/api/sequence", "POST", {seed:id, expectedRevision:revision, showAll:false}, current);
    if (!current()) return;
    if (!IndexPin.equal(data?.revision, revision) || data?.seed?.id !== id) throw new Error("Sequence link provenance mismatch. Nothing displayed.");
    $("all-steps").checked = false;
    // A resolved route stores only a key in browser history; seed/DTO remain in memory.
    rememberNavigation({kind:"sequence", symbol:data.seed, admitted:data, showAll:false}, true, hash);
    await selectMethod(data.seed, data, current);
  } catch (error) {
    if (current() && error.name !== "AbortError") navigationError(`Cannot open sequence: ${error.message}`);
  }
}
window.addEventListener?.("popstate", () => { void readNavigationLocation(); });
window.addEventListener?.("hashchange", () => { void readNavigationLocation(); });

function element(tag, text, className) {
  const node = document.createElement(tag);
  if (text !== undefined) node.textContent = text;
  if (className) node.className = className;
  return node;
}
function button(text, action) {
  const node = element("button", text); node.type = "button";
  node.addEventListener("click", () => perform(action, node)); return node;
}
function describe(value) { return typeof value === "string" ? value : JSON.stringify(value, null, 2); }
function aborted() { return new DOMException("Superseded request", "AbortError"); }
function operationGuard() {
  const session = epoch, root = selectedRootKey, method = selectedMethod;
  const revision = status?.revision && IndexPin.copy(status.revision);
  return () => session === epoch && root === selectedRootKey && method === selectedMethod && IndexPin.equal(revision, status?.revision);
}
function selectedUrl(path, method, root) {
  if (path === "/api/checkouts" || path === "/api/daemon/status") {
    if (method !== "GET") throw new Error("Global API requests must be GET.");
    return path;
  }
  const suffix = path.startsWith("/api/") ? path.slice(5) : "";
  const route = suffix.split("?", 1)[0];
  const simple = new Set(["status", "git", "git/diff", "metrics", "architecture", "architecture/details", "architecture/rules", "architecture/rules/preview", "architecture/rules/apply", "index", "tree", "files", "methods", "symbols", "syntax-matches", "sequence", "classes", "class-diagram", "navigation", "source", "jobs/current", "dependencies", "dependencies/refresh", "dependencies/symbols", "dependencies/source", "rust-sources", "rust-sources/tree", "rust-sources/file"]);
  if (route === "architecture/rules" && method !== "GET" || ["architecture/rules/preview", "architecture/rules/apply"].includes(route) && method !== "POST") throw new Error("Invalid rules request method.");
  if (route === "syntax-matches" && method !== "POST") throw new Error("Syntax match requests must be POST.");
  if (["git", "git/diff", "metrics", "architecture", "architecture/details", "symbols"].includes(route) && method !== "GET") throw new Error("Read-only API requests must be GET.");
  const item = /^jobs\/[^/?]+(?:\/cancel)?$/.test(route);
  if (!simple.has(route) && !item || /[#\s]/.test(suffix) || suffix.includes("//")) throw new Error("Unknown checkout API route.");
  if (!root || !uniqueSelectableRoot(root)) throw new Error("Choose an available checkout first.");
  return `/api/checkouts/${encodeURIComponent(root)}/${suffix}`;
}
async function api(path, method = "GET", body, callerCurrent = () => true) {
  const session = epoch, root = selectedRootKey, sourceAtStart = sourceSerial;
  const url = selectedUrl(path, method, root);
  const guarded = path.startsWith("/api/source?");
  const operationCurrent = operationGuard();
  const current = () => callerCurrent() && session === epoch && root === selectedRootKey && (!guarded || (operationCurrent() && sourceAtStart === sourceSerial));
  let response, data, timer;
  const controller = new AbortController(); pendingRequests.add(controller);
  const timeoutError = new Error("Request timed out. No automatic retry was made. Refresh status before trying again.");
  timeoutError.name = "TimeoutError";
  const deadline = 15000;
  try {
    const timeout = new Promise((_, reject) => {
      timer = setTimeout(() => { reject(timeoutError); controller.abort(); }, deadline);
    });
    await Promise.race([timeout, (async () => {
      response = await fetch(url, {method, cache: "no-store", credentials: "omit", redirect: "error", signal: controller.signal,
        headers: {Authorization: `Bearer ${token}`, ...(body === undefined ? {} : {"Content-Type": "application/json"})},
        body: body === undefined ? undefined : JSON.stringify(body)});
      if (!current()) throw aborted();
      data = response.status === 204 ? null : await response.json();
    })()]);
  } catch (error) { if (!current()) throw aborted(); throw error; }
  finally { clearTimeout(timer); pendingRequests.delete(controller); }
  // Check before any HTTP error side effects, including invalidation on 409.
  if (!current()) throw aborted();
  if (!response.ok) {
    const error = new Error(data?.error?.message || `Request failed (${response.status})`);
    error.status = response.status;
    error.code = data?.error?.code;
    error.reason = data?.error?.reason;
    if (error.code === "invalid_rules") error.diagnostic = data.error;
    error.workspaceRoot = response.headers?.get?.("X-Trellis-Workspace") || null;
    if (error.status === 401 || error.status === 403) disconnect(`Authentication failed (${error.code || error.status}). Connect with a daemon token again.`);
    else if (root && error.code === "workspace_selection_failed") invalidateSelection("Checkout identity changed. Choose an available checkout again.");
    throw error;
  }
  observePinnedResponse(path, method, body, data);
  return data;
}
async function perform(action, control) {
  if (control?.disabled) return;
  if (control) control.disabled = true;
  $("error").hidden = true;
  const session = epoch;
  let current = () => session === epoch;
  try {
    const pending = action();
    // Actions may synchronously advance their serial before their first await.
    current = operationGuard();
    await pending;
  }
  catch (error) { if (error.name !== "AbortError" && current()) { if (IndexPin.isConflict(error)) { void refreshStatus().catch(() => {}); window.TrellisShell?.resetInspector(); diagramSerial++; clearSource(); stale("The index revision changed. Refresh this view before reading source."); } $("error").textContent = error.message; $("error").hidden = false; } }
  finally { if (control) control.disabled = false; }
}
function form(id, action) {
  $(id).addEventListener("submit", event => { event.preventDefault(); perform(action, $(id).querySelector("button[type=submit]")); });
}
function stale(message) { staleMessage = message; $("stale").textContent = message; $("stale").hidden = false; renderIndexStatus(); }
function clearSource() {
  sourceSerial++; sourceInspector = null; workspaceSource = null; window.TrellisNavigation?.reset(); $("source").replaceChildren();
  $("source-path").textContent = "Choose a call to read its cached source snapshot.";
  if (!$("source-dock")?.hidden) window.TrellisShell?.closeSource?.();
}
function changedCount(value) { return Array.isArray(value) ? value.length : Number(value || 0); }
function showSelectedGit(git) {
  selectedGit = git || null;
  if (selectedGit?.state !== "available") {
    filesGitStatus = null; filesDiffSerial++; filesDiff = null;
    $("files-diff")?.replaceChildren();
  }
  const row = checkouts.find(row => row.rootKey === selectedRootKey);
  if (row) row.git = selectedGit;
  updateCheckoutOptions();
  updateFilesBasis();
  if (treeMode || files.length || filesCatalogLoaded) renderFiles();
  window.TrellisShell?.updateCheckout(selectedWorkspaceRoot, selectedGit);
  if (!status) window.TrellisShell?.updateWorkspace(null);
}
async function refreshGit() {
  const serial = ++gitSerial, session = epoch, root = selectedRootKey;
  filesDiffSerial++; filesDiff = null; $("files-diff")?.replaceChildren();
  try {
    const data = await api("/api/git");
    if (serial === gitSerial && session === epoch && root === selectedRootKey) { filesGitStatus = data; showSelectedGit(data?.git); }
  } catch (error) {
    if (serial === gitSerial && session === epoch && root === selectedRootKey && error.name !== "AbortError") { filesGitStatus = null; showSelectedGit(null); }
  }
}
async function refreshStatus(followup = false) {
  statusRefreshDepth++;
  try {
  const serial = ++statusSerial, session = epoch;
  let data;
  try { data = await api("/api/status"); }
  catch (error) {
    if (serial === statusSerial && session === epoch) {
      statusUnavailable = error.code !== "index_not_ready";
      indexNotReady = error.code === "index_not_ready";
      renderIndexStatus();
    }
    if (error.code === "index_not_ready" && serial === statusSerial && session === epoch && selectedRootKey) {
      const root = selectedRootKey, workspaceRoot = error.workspaceRoot || status?.workspaceRoot;
      const listedRoot = selectedWorkspaceRoot, git = selectedGit;
      resetCheckout();
      selectedRootKey = root; selectedWorkspaceRoot = listedRoot; indexNotReady = true;
      showSelectedGit(git);
      void refreshGit();
      $("checkout-select").value = root;
      $("checkout-root").textContent = workspaceRoot || "Selected checkout · verified root unavailable until status responds";
      $("workspace").hidden = false; $("refresh").disabled = false; $("index").disabled = false;
      window.TrellisShell?.setConnected(true);
      showCheckoutIndexState(checkouts.find(row => row.rootKey === root));
      $("status").textContent = "No validated index head is available yet.";
      $("checkout-state").textContent = "Index not ready. Retry status after indexing starts.";
      stale("Index not ready. Retry status after indexing starts.");
    }
    throw error;
  }
  if (serial !== statusSerial || session !== epoch) return;
  if (typeof data?.workspaceRoot !== "string" || !data.workspaceRoot) throw new Error("Selected checkout identity unavailable. Retry status.");
  $("checkout-root").textContent = data.workspaceRoot;
  const workspaceChanged = status && status.workspaceRoot !== data.workspaceRoot;
  const nextPin = data.revision == null ? null : IndexPin.copy(data.revision);
  const browseChanged = !status || workspaceChanged || !IndexPin.equal(status.revision, nextPin);
  if (status && browseChanged) { clearNavigationHistory(); clearBrowse("Index changed. Choose a method from the refreshed files."); sourceCache.clear(); clearSource(); }
  if (browseChanged) clearDependencyCatalog();
  resetHeadObservation();
  statusUnavailable = false; indexNotReady = !nextPin;
  status = {...data, revision:nextPin};
  data = status;
  updateFilesBasis();
  const listed = checkouts.find(row => row.rootKey === selectedRootKey);
  if (listed) Object.assign(listed, {revision:nextPin, catchingUp:data.catchingUp, indexError:nextPin ? null : "index_not_ready"});
  window.TrellisShell?.updateWorkspace(nextPin ? status : null);
  $("catching-up").hidden = data.catchingUp !== true || !nextPin;
  updateCheckoutOptions();
  if (browseChanged && nextPin) {
    void viewerMetrics?.refresh();
    if (section === "diagrams" && !$("architecture-panel").hidden) void architecture?.refresh();
    if (section === "diagrams" && !$("treemap-panel").hidden) void treemap?.refresh();
    await loadTreeRoot();
  }
  if (serial !== statusSerial || session !== epoch || status !== data) return;
  const stats = status.stats || {};
  $("status").textContent = `${status.workspaceRoot} · ${status.revision ? `Revision ${IndexPin.label(status.revision)}` : "Index not ready"} · ${stats.files ?? 0} files · ${stats.symbols ?? 0} symbols · ${stats.calls ?? 0} calls · Semantic proof unavailable · SCIP (if present): display only · Indexed: ${status.indexedAt ? new Date(Number(status.indexedAt)).toLocaleString() : "not yet"}`;
  $("diagnostics").textContent = describe(status.diagnostics || []);
  if (job && !activeJob(job)) showJob();
  staleMessage = ""; $("stale").hidden = true;
  if (!nextPin) stale("Index not ready. Retry status after this checkout finishes indexing.");
  if (changedCount(stats.changedFiles)) stale(`${changedCount(stats.changedFiles)} inputs differ from the imported SCIP manifest. Syntax remains source-only; imported SCIP is display metadata, never semantic proof.`);
  renderIndexStatus();
  // A rejected optional catalog must not recursively request status when status is unchanged.
  if (nextPin && (browseChanged || !pairRefreshPending)) void refreshDependencies();
  } finally {
    statusRefreshDepth--;
    if (!statusRefreshDepth && pairRefreshQueued) {
      const queued = pairRefreshQueued;
      pairRefreshQueued = null;
      if (!followup && queued.session === epoch &&
          (!status || queued.key !== `${status.workspaceRoot}:${IndexPin.key(status.revision)}`)) {
        pairRefreshFollowup = true;
        void refreshStatus(true).catch(() => {}).finally(() => { pairRefreshFollowup = false; });
      }
    }
  }
}
function unexpectedPair(message = "Index snapshot changed. Refresh status.", receivedPair) {
  clearNavigationHistory(); clearBrowse(message); clearSource(); sourceCache.clear(); clearDependencyCatalog();
  $("files-state").textContent = message;
  $("dependency-state").textContent = message;
  $("dependency-symbol-state").textContent = message;
  stale(message);
  let observed = null;
  try { if (receivedPair) observed = IndexPin.copy(receivedPair); } catch (_) { /* Status remains authoritative. */ }
  if (statusRefreshDepth) {
    if (!pairRefreshFollowup && observed) pairRefreshQueued = {session:epoch, key:`${status?.workspaceRoot}:${IndexPin.key(observed)}`};
    return;
  }
  if (!pairRefreshPending) {
    pairRefreshPending = true;
    void refreshStatus().catch(() => {}).finally(() => { pairRefreshPending = false; });
  }
}
const tokenStorageKey = "trellis.daemonToken.v1";
let tokenFileSerial = 0, tokenSaveFailed = false;
const safeTokenText = value => value.length <= 512 && /^[A-Za-z0-9_-]+$/.test(value);
function forgetStoredToken() {
  try { localStorage.removeItem(tokenStorageKey); return true; } catch (_) { return false; }
}
try {
  const saved = localStorage.getItem(tokenStorageKey);
  if (saved && safeTokenText(saved)) {
    $("token").value = saved; $("remember-token").checked = true;
    $("notice").textContent = "Saved token loaded. Select Connect to browse.";
  }
} catch (_) { /* Normal in-memory authentication still works. */ }
$("load-token-file").addEventListener("click", () => $("token-file").click());
$("remember-token").addEventListener("change", () => {
  if (!$("remember-token").checked) { tokenSaveFailed = false; forgetStoredToken(); }
});
$("token-file").addEventListener("change", async () => {
  const input = $("token-file"), file = input.files?.[0], session = epoch, serial = ++tokenFileSerial;
  if (!file) return;
  try {
    if (file.size > 1024) throw new Error("Choose the small daemon.token file, not a source or data file.");
    const value = (await file.text()).trim();
    if (session !== epoch || serial !== tokenFileSerial || $("connect-form").hidden) return;
    if (!safeTokenText(value)) throw new Error("This file does not contain a valid daemon token. Choose daemon.token.");
    $("token").value = value; $("error").hidden = true;
    $("notice").textContent = "Token file loaded locally. Select Connect.";
  } catch (error) {
    if (session !== epoch || serial !== tokenFileSerial) return;
    $("error").textContent = "Could not load token file: " + (file.size > 1024 ? "file is too large." : "choose a readable daemon.token file."); $("error").hidden = false;
  } finally { if (serial === tokenFileSerial) input.value = ""; }
});
const validRootKey = key => typeof key === "string" && /^[0-9a-f]{64}$/.test(key);
function checkoutOption(label, value) { const option = element("option", label); option.value = value; return option; }
function resetCheckout() {
  // Advance the generation before aborting or awaiting any request. Equal revision pairs are not identity.
  epoch++; clearNavigationHistory(); gitSerial++; section = "diagrams"; sectionSerial++; filesGitStatus = null; selectedRootKey = null; selectedWorkspaceRoot = null; selectedGit = null; checkoutBusy = false;
  classSupport = null;
  for (const controller of pendingRequests) controller.abort();
  clearTimeout(pollTimer); pollTimer = null; statusSerial++;
  pairRefreshPending = false; pairRefreshQueued = null;
  clearDependencyCatalog(); clearExternalSources(); clearBrowse(); clearSource(); sourceCache.clear();
  status = null; statusUnavailable = false; jobUnavailable = false; indexNotReady = false; staleMessage = ""; job = null; waitingForJob = false;
  resetHeadObservation();
  ["status", "diagnostics", "job"].forEach(id => $(id).textContent = "");
  $("file-filter").value = "";
  $("stale").hidden = true; $("catching-up").hidden = true;
  renderIndexStatus();
  $("checkout-root").textContent = "No checkout selected";
  $("workspace").hidden = true; $("index").disabled = true; $("cancel").hidden = true;
  window.TrellisShell?.setConnected(false);
}
function disconnect(message) {
  tokenFileSerial++; tokenSaveFailed = false; const forgotten = forgetStoredToken();
  resetCheckout(); checkouts = []; token = "";
  $("remember-token").checked = false; $("token-file").value = ""; $("token").value = "";
  $("checkout-picker").hidden = true; $("checkout-select").replaceChildren(checkoutOption("Choose a checkout", ""));
  $("app-menu").hidden = true; $("app-menu").open = false; $("connect-form").hidden = false;
  document.body.classList.toggle("daemon-connected", false);
  $("error").hidden = !message; $("error").textContent = message || "";
  $("notice").textContent = message || (forgotten ? "Disconnected. Saved token and cached source cleared." : "Disconnected. Browser storage could not be cleared; clear this address’s site data to forget any saved token.");
  $("token").focus();
}
function invalidateSelection(message) {
  resetCheckout();
  $("checkout-select").value = "";
  $("checkout-state").textContent = message;
  $("notice").textContent = message;
  void loadCheckouts();
}
function selectable(row) {
  return objectValue(row) && validRootKey(row.rootKey) && row.state === "available" && typeof row.workspaceRoot === "string" && row.workspaceRoot.startsWith("/");
}
function uniqueSelectableRoot(key) {
  const matches = checkouts.filter(row => row.rootKey === key);
  return matches.length === 1 && selectable(matches[0]);
}
function checkoutIndexLabel(row) {
  if (!row || row.state !== "available") return {text: row?.state === "storage_busy" ? "Busy" : "Failed", state:"failed", shape:"□"};
  const pin = row.revision, catchingUp = row.catchingUp;
  if (row.indexError && row.indexError !== "index_not_ready") return {text:`Index unavailable · ${row.indexError}`, state:"failed", shape:"□"};
  if (!pin) return catchingUp === true ? {text:"Catching up", state:"catching-up", shape:"◐"}
    : {text:"Not indexed", state:"unknown", shape:"◇"};
  let label;
  try { label = IndexPin.label(pin); } catch (_) { return {text:"Index status unavailable", state:"unknown", shape:"◇"}; }
  return catchingUp === true ? {text:`Catching up · rev ${label}`, state:"catching-up", shape:"◐"}
    : catchingUp === false ? {text:`Fresh · rev ${label}`, state:"fresh", shape:"○"}
    : {text:`Indexed · idle · rev ${label}`, state:"unknown", shape:"◇"};
}
function selectedIndexStatus() {
  const row = checkouts.find(row => row.rootKey === selectedRootKey);
  const catchingUp = status?.revision && IndexPin.equal(row?.revision, status.revision) ? row.catchingUp : status?.catchingUp;
  const base = status?.revision ? checkoutIndexLabel({...row, state:"available", revision:status.revision, catchingUp}) : checkoutIndexLabel(row);
  if (!selectedRootKey) return {state:"unknown", text:"No checkout selected"};
  if (checkoutBusy) return {state:"failed", text:"Checkout busy · retry checkout list"};
  if (statusUnavailable) return {state:"failed", text:"Index status unavailable · retry status"};
  if (jobUnavailable) return {state:"failed", text:"Index job unavailable · retry status"};
  if (job?.state === "failed" && !status?.revision) return {state:"failed", text:`Index failed · ${jobCause(job)}`};
  if (job && activeJob(job)) return {state:"catching-up", text:jobText(job)};
  if (indexNotReady) return {state:"unknown", text:"Index not ready · retry status"};
  if (!status?.revision) return {...base, state:"unknown", text:"Loading index status…"};
  if (headObservation?.failed) return {state:"stale", text:"Head comparison unavailable · see Diagnostics"};
  const head = headObservation?.head, pin = status.revision;
  if (head && (head.indexGeneration !== pin.indexGeneration || head.indexRevision > pin.indexRevision)) {
    return {state:"stale", text:"Viewing retained revision · reload at head"};
  }
  if (staleMessage) return {state:"stale", text:"Stale evidence · see Diagnostics"};
  return base;
}
// The visible label follows every poll; the live region speaks only when the leading state words change.
function stateWord(text) { return String(text).split(" · ")[0]; }
// The recorded failure code and reason, never the generic job message.
function jobCause(value) {
  const code = typeof value?.error?.code === "string" && value.error.code ? value.error.code : "cause not recorded";
  const reason = typeof value?.error?.reason === "string" && value.error.reason ? `: ${value.error.reason}` : "";
  return `${code}${reason}`;
}
function renderIndexStatus() {
  const state = selectedIndexStatus();
  $("workspace-index-state")?.setAttribute("data-state", state.state);
  $("workspace-index-label").textContent = state.text;
  // The served revision stays the status; a failed later re-index is a secondary note.
  const note = job?.state === "failed" && status?.revision ? `Last re-index failed · ${jobCause(job)}` : "";
  const noteNode = $("workspace-index-note");
  if (noteNode) { noteNode.textContent = note; noteNode.title = note; noteNode.hidden = !note; }
  const announce = $("workspace-index-announce"), spoken = note ? `${state.text} · ${note}` : state.text;
  if (announce && stateWord(announce.textContent) !== stateWord(spoken)) announce.textContent = spoken;
  else if (announce && note && !announce.textContent.includes(note)) announce.textContent = spoken;
  // Index workspace is offered, and the tabs are greyed out, only when the checkout has no usable index.
  const noIndex = !!selectedRootKey && indexNotReady;
  // While this page waits on an accepted first index, Stop waiting replaces the (disabled) button.
  if ($("index")) $("index").hidden = !noIndex || (waitingForJob && !!job && activeJob(job));
  window.TrellisShell?.setIndexAvailable?.(!noIndex);
}
const lastCheckoutKey = "trellis.lastCheckout.v1";
function rememberCheckout(key) {
  try { localStorage.setItem(lastCheckoutKey, key); } catch (_) { /* A per-browser convenience only. */ }
}
// Arrival picks the only selectable checkout, else the one last used in this browser.
// The agent's working directory is not observable from the browser or daemon here.
function arrivalCheckout() {
  const available = checkouts.filter(selectable);
  if (available.length === 1) return available[0].rootKey;
  let last = null;
  try { last = localStorage.getItem(lastCheckoutKey); } catch (_) { last = null; }
  return validRootKey(last) && uniqueSelectableRoot(last) ? last : null;
}
function storageWarning() { return tokenSaveFailed ? "Browser storage is unavailable; token was not saved." : ""; }
function updateCheckoutOptions() {
  for (const option of $("checkout-select").children) {
    const row = checkouts.find(row => row.rootKey === option.value);
    if (!row) continue;
    option.textContent = window.TrellisShell.checkoutLabel(row);
    option.title = row.workspaceRoot || row.rootKey;
  }
  $("checkout-select").title = selectedWorkspaceRoot || "Choose a checkout";
}
function showCheckoutIndexState() {
  renderIndexStatus();
}
async function loadCheckouts() {
  const session = epoch, gitAtStart = gitSerial;
  $("checkout-state").textContent = "Loading checkouts…";
  $("checkout-select").disabled = true; $("checkout-retry").hidden = true;
  try {
    const data = await api("/api/checkouts");
    if (session !== epoch) return;
    if (!Array.isArray(data?.checkouts)) throw new Error("Checkout list is invalid. Retry the list.");
    const keys = data.checkouts.filter(row => objectValue(row) && typeof row.rootKey === "string").map(row => row.rootKey);
    if (new Set(keys).size !== keys.length) throw new Error("Duplicate checkout root key. Retry the list.");
    const next = data.checkouts.filter(row => objectValue(row) && validRootKey(row.rootKey) && typeof row.state === "string");
    const selected = selectedRootKey && next.find(row => row.rootKey === selectedRootKey);
    // A transient list read cannot revoke an explicit selection of the same listed root.
    // Erase derived evidence and fence scoped requests until the list recovers.
    const verifiedRoot = selectedWorkspaceRoot;
    const sameRoot = selected && verifiedRoot && selected.workspaceRoot === verifiedRoot;
    const busy = sameRoot && selected.state === "storage_busy";
    const recovering = checkoutBusy && sameRoot && selectable(selected);
    if (busy && !checkoutBusy) {
      const key = selectedRootKey, git = selectedGit;
      resetCheckout();
      selectedRootKey = key; selectedWorkspaceRoot = verifiedRoot;
      showSelectedGit(git);
      checkoutBusy = true;
      $("checkout-root").textContent = verifiedRoot;
    }
    if (sameRoot && gitAtStart !== gitSerial) selected.git = selectedGit;
    checkouts = next;
      if (selectedRootKey && (!sameRoot || (!busy && !selectable(selected)))) {
      invalidateSelection("Selected checkout is no longer available. Choose again."); return;
    }
    const select = $("checkout-select"), choice = selectedRootKey || "";
    select.replaceChildren(checkoutOption("Choose a checkout", ""));
    for (const row of checkouts) {
      const option = checkoutOption(window.TrellisShell.checkoutLabel(row), row.rootKey);
      option.title = row.workspaceRoot || row.rootKey;
      option.disabled = !selectable(row); select.append(option);
    }
    select.value = choice;
    if (choice && !busy && gitAtStart === gitSerial) {
      gitSerial++; filesGitStatus = null; showSelectedGit(selected.git);
      if (section === "files" && filesFile && filesBasis === "working") void setFilesBasis("working");
      else void refreshGit();
    }
    select.disabled = !checkouts.some(selectable);
    $("checkout-state").textContent = busy ? "Selected checkout is temporarily busy. Retry the checkout list."
      : !checkouts.length ? "No checkouts found. Register a checkout, then retry."
      : !checkouts.some(selectable) ? "No available checkouts. Unavailable or corrupt entries cannot be selected. Retry the list."
      : choice ? storageWarning() : "Choose a checkout to inspect.";
    $("checkout-retry").hidden = !busy && checkouts.length > 0 && checkouts.every(selectable);
    if (choice && !busy) showCheckoutIndexState(selected);
    if (recovering) await selectCheckout(choice);
    else if (choice && !busy && status?.revision && Object.hasOwn(selected, "revision") && !IndexPin.equal(selected.revision, status.revision)) {
      clearBrowse("Index changed. Rebinding the selected checkout."); clearSource(); sourceCache.clear(); clearDependencyCatalog();
      await refreshStatus();
    }
    return true;
  } catch (error) {
    if (session !== epoch || error.name === "AbortError") return;
    if (selectedRootKey) resetCheckout();
    checkouts = []; $("checkout-select").replaceChildren(checkoutOption("Choose a checkout", ""));
    $("checkout-select").disabled = true; $("checkout-retry").hidden = false;
    $("checkout-state").textContent = `Checkout list unavailable: ${error.message} Retry the list.`;
    return false;
  }
}
async function loadDaemonStatus() {
  const session = epoch;
  try {
    const data = await api("/api/daemon/status");
    if (session === epoch) $("daemon-state").textContent = Number.isSafeInteger(data?.activeCheckouts) ? `${data.activeCheckouts} active checkouts` : "Daemon status unavailable.";
  } catch (error) { if (session === epoch && error.name !== "AbortError") $("daemon-state").textContent = `Daemon status unavailable: ${error.message}`; }
}
async function selectCheckout(key) {
  resetCheckout();
  $("checkout-select").value = key;
  if (!uniqueSelectableRoot(key)) {
    $("checkout-select").value = ""; $("checkout-state").textContent = "Choose an available checkout."; return;
  }
  selectedRootKey = key; rememberCheckout(key);
  const selected = checkouts.find(row => row.rootKey === key);
  selectedWorkspaceRoot = selected.workspaceRoot;
  showSelectedGit(selected.git);
  showCheckoutIndexState(selected);
  $("error").hidden = true; $("error").textContent = "";
  $("checkout-root").textContent = "Verifying selected checkout…";
  $("checkout-state").textContent = "Loading selected checkout…";
  $("workspace").hidden = false; $("refresh").disabled = false; $("index").disabled = false;
  window.TrellisShell?.setConnected(true);
  const session = epoch;
  void refreshGit();
  try {
    await refreshStatus();
    if (session !== epoch) return;
    const lookup = await lookupCurrentJob();
    if (session !== epoch) return;
    // The browse notice is hidden once a checkout is shown; the storage warning stays in the header status line.
    $("checkout-state").textContent = lookup.error && jobUnavailable
      ? `Index job unavailable: ${lookup.error.message} Retry status; your selection is unchanged.` : storageWarning();
    void readNavigationLocation(true);
    $("notice").textContent = `Browse folders below. Indexed files expand to methods.${storageWarning() ? ` ${storageWarning()}` : ""}`;
  } catch (error) {
    if (session !== epoch || error.name === "AbortError") return;
    const cold = error.code === "index_not_ready";
    showCheckoutIndexState(cold ? checkouts.find(row => row.rootKey === key) : {state:"unavailable"});
    $("checkout-root").textContent = error.workspaceRoot || status?.workspaceRoot || "Selected checkout · verified root unavailable until status responds";
    $("checkout-state").textContent = cold ? "Index not ready. Retry status after indexing starts." : `Checkout unavailable: ${error.message} Retry status; your selection is unchanged.`;
    $("status").textContent = cold ? "No validated index head is available yet." : "Selected checkout status unavailable.";
    $("refresh").disabled = false;
    stale($("checkout-state").textContent);
    renderIndexStatus();
  }
}
$("checkout-select").addEventListener("change", event => { void selectCheckout(event.target.value); });
$("checkout-retry").addEventListener("click", async () => {
  const [listed] = await Promise.all([loadCheckouts(), loadDaemonStatus()]);
  if (listed && token && $("remember-token").checked) {
    try { localStorage.setItem(tokenStorageKey, token); tokenSaveFailed = false; }
    catch (_) {
      tokenSaveFailed = true;
      const line = $("checkout-state");
      if (!line.textContent.includes(storageWarning())) line.textContent = `${line.textContent} ${storageWarning()}`.trim();
    }
  }
});
form("connect-form", async () => {
  const supplied = $("token").value.trim();
  if (!safeTokenText(supplied)) throw new Error("Paste a daemon token or load daemon.token first.");
  tokenFileSerial++; tokenSaveFailed = false; token = supplied; $("token").value = ""; $("token-file").value = "";
  $("error").hidden = true; $("error").textContent = "";
  $("checkout-picker").hidden = false; $("connect-form").hidden = true; $("app-menu").hidden = false;
  document.body.classList.toggle("daemon-connected", true);
  const [listed] = await Promise.all([loadCheckouts(), loadDaemonStatus()]);
  if (!token) return;
  let remembered = false;
  if (listed && $("remember-token").checked) {
    try { localStorage.setItem(tokenStorageKey, token); remembered = true; }
    catch (_) { tokenSaveFailed = true; }
  } else if (listed) forgetStoredToken();
  const arrival = listed && !selectedRootKey ? arrivalCheckout() : null;
  $("notice").textContent = listed ? `Connected to the daemon on ${window.location?.host || "this address"}. ${arrival ? "Opening the checkout." : "Choose a checkout before browsing."}`
    + (remembered ? " Token saved on this browser." : tokenSaveFailed ? " Browser storage is unavailable; token was not saved." : "")
    : "Checkout list unavailable. Retry the list; no checkout has been selected.";
  if (arrival) await selectCheckout(arrival);
  else $("checkout-select").focus();
});
$("logout").addEventListener("click", () => disconnect());

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
async function showSource(item, revision) {
  if (!sourceWitness(item)) throw new Error("Measured source path and range unavailable.");
  return showCachedSource(item.path, item.range, revision);
}
let workspaceSource = null;
function paintCachedSource(data, range, current, startLine = range?.startLine || 1, metadata) {
  workspaceSource = {data, range};
  $("source-path").textContent = `${data.file.path} · ${data.file.language} · ${data.file.text.split("\n").length} lines · revision ${IndexPin.label(data.revision)} · cached snapshot`;
  const fragment = document.createDocumentFragment();
  const lastLine = range && range.endLine > range.startLine && range.endColumn === 1 ? range.endLine - 1 : range?.endLine;
  const lines = data.file.text.split("\n"), syntax = section === "files" ? window.TrellisFiles?.lineClass : null;
  lines.forEach((text, index) => {
    const number = index + 1, line = element("span", undefined, "source-line");
    if (range && number >= range.startLine && number <= lastLine) line.classList.add("highlight");
    const kind = syntax?.(data.file.language, text, lines.length);
    if (kind) line.classList.add(kind);
    line.append(element("span", String(number), "line-number"), document.createTextNode(text)); fragment.append(line);
  });
  $("source").replaceChildren(fragment); $("source").hidden = false;
  window.TrellisShell?.showSource("workspace");
  const binding = window.TrellisNavigation?.attachSource($("source"), {
    path:data.file.path, revision:data.revision, startLine, metadataSnapshot:metadata,
    isCurrent: () => current() && !!token && IndexPin.equal(status?.revision, data.revision) &&
      !(section === "files" && filesBasis !== "indexed") && !$("source-dock")?.hidden && !$("workspace-source-panel")?.hidden,
  });
  workspaceSource.navigation = binding;
  return binding;
}
async function showCachedSource(path, range, revision, callerCurrent = () => true) {
  navigationSerial++;
  IndexPin.copy(revision);
  if (!status?.revision || !IndexPin.equal(revision, status.revision)) throw new Error("This source has no current paired snapshot. Refresh the view first.");
  const serial = ++sourceSerial, session = epoch, root = selectedRootKey;
  const current = () => callerCurrent() && serial === sourceSerial && session === epoch && root === selectedRootKey && IndexPin.equal(status?.revision, revision);
  const key = `${IndexPin.key(revision)}:${path}`;
  window.TrellisNavigation?.reset(); workspaceSource = null;
  $("source-path").textContent = `Loading ${path}…`; $("source").replaceChildren();
  const cached = sourceCache.get(key);
  const data = cached || await api(`/api/source?path=${encodeURIComponent(path)}&${IndexPin.query(revision)}`, "GET", undefined, current);
  if (cached && current()) observePinnedHead();
  if (!current()) return;
  if (!IndexPin.equal(data.revision, revision)) { unexpectedPair("Source snapshot changed. Refresh status.", data.revision); return; }
  if (data.file.path !== path) throw new Error("Source path mismatch. Nothing displayed.");
  sourceCache.set(key, data);
  if (section === "diagrams") {
    // An explicit diagram source read replaces the shared source selection.
    filesSerial++; filesDiffSerial++; clearFilesInspector();
    filesFile = files.find(file => file.path === path) || {path}; filesBasis = "indexed"; filesDiff = null;
  }
  paintCachedSource(data, range, current);
  $("source").focus({preventScroll:true});
  const highlight = $("source").querySelector(".highlight");
  if (highlight) {
    $("source").scrollTop = highlight.offsetTop - $("source").offsetTop - 80;
    highlight.scrollIntoView?.({block:"start", inline:"nearest"});
  }
}
async function refreshView() {
  resetHeadObservation();
  const revision = status?.revision, session = epoch, root = selectedRootKey;
  const file = filesFile, basis = filesBasis;
  clearSource(); filesDiffSerial++; filesDiff = null; $("files-diff")?.replaceChildren();
  await refreshGit(); if (session !== epoch || root !== selectedRootKey) return;
  await refreshStatus();
  if (jobUnavailable) {
    // Retrying status also retries the job lookup that failed during selection.
    const lookup = await lookupCurrentJob();
    if (session !== epoch || root !== selectedRootKey) return;
    if (lookup.error) throw lookup.error;
    $("checkout-state").textContent = storageWarning();
  }
  if (IndexPin.equal(revision, status?.revision)) {
    void viewerMetrics?.refresh(); await refreshTree();
    if (section === "files" && file) {
      if (file.changeOnly) { filesFile = file; await setFilesBasis("working"); }
      else { await selectFilesFile(file); if (basis === "working") await setFilesBasis("working"); }
    }
  }
  if (section !== "files" && !$("treemap-panel").hidden) await treemap?.refresh();
  else if (section !== "files" && !$("architecture-panel").hidden) await architecture?.refresh();
  else if (section !== "files" && selectedMethod) await loadSequence();
  schedulePoll();
}
$("refresh").addEventListener("click", () => perform(refreshView, $("refresh")));
$("reload-head").addEventListener("click", () => perform(refreshView, $("reload-head")));
function activeJob(value) { return ["queued", "running"].includes(value.state); }
// Progress is shown only while it means something; a finished index states its result.
function jobText(value) {
  const progress = value.progress, total = progress?.total;
  if (value.state === "done") {
    const pin = value.revision;
    return `Indexed${pin ? ` · rev ${IndexPin.label(pin)}` : ""}`;
  }
  if (value.state === "failed") return `Index failed · ${jobCause(value)}`;
  const parts = [value.state === "queued" ? "Index queued · accepted" : "Indexing"];
  if (progress?.phase) parts.push(progress.phase);
  if (Number.isSafeInteger(total) && total > 0) parts.push(`${progress.completed ?? 0}/${total}`);
  if (!waitingForJob) parts.push("no longer waiting here");
  if (value.error) parts.push("see Diagnostics");
  return parts.join(" · ");
}
// Every job read from the daemon is an observation; an older lookup never overrides a newer one.
function observeJob(value) { job = value; jobObservation++; jobUnavailable = false; }
async function lookupCurrentJob() {
  const observed = jobObservation, session = epoch;
  let current, error = null;
  try { current = await api("/api/jobs/current"); }
  catch (failure) { if (failure.name === "AbortError") return {error:null}; error = failure; }
  if (session !== epoch || observed !== jobObservation) return {error:null};
  if (error) {
    // The job is unknown: never let the header fall back to Fresh.
    jobUnavailable = true; $("job").textContent = `Index job unavailable: ${error.message}`; renderIndexStatus();
    return {error};
  }
  observeJob(current); waitingForJob = !!job && activeJob(job); showJob();
  if (job) schedulePoll();
  return {error:null};
}
function showJob() {
  if (!job) { $("job").textContent = ""; $("cancel").hidden = true; $("index").disabled = false; renderIndexStatus(); return; }
  $("job").textContent = `${jobText(job)}${job.error ? `\n${describe(job.error)}` : ""}`;
  renderIndexStatus();
  $("cancel").hidden = !waitingForJob || !activeJob(job);
  $("index").disabled = waitingForJob && activeJob(job);
}
function schedulePoll() {
  clearTimeout(pollTimer);
  if (!waitingForJob || !job || !activeJob(job) || !token) return;
  pollTimer = setTimeout(() => perform(async () => {
    observeJob(await api(`/api/jobs/${encodeURIComponent(job.id)}`)); showJob();
    if (activeJob(job)) schedulePoll();
    else { waitingForJob = false; showJob(); await refreshStatus(); }
  }), 700);
}
$("index").addEventListener("click", () => perform(async () => {
  clearNavigationHistory();
  window.TrellisClasses?.reset();
  diagramSerial++; viewerMetrics?.clearInspection(); window.TrellisShell?.resetInspector(); clearSource();
  observeJob(await api("/api/index", "POST", {})); waitingForJob = true; showJob(); schedulePoll();
}, $("index")).then(() => { if (job) showJob(); }));
$("cancel").addEventListener("click", () => {
  // Stopping this page's polling never cancels a durably accepted request.
  clearTimeout(pollTimer); waitingForJob = false; showJob();
});


// Files uses the existing checkout/session owner, source cache and tree DOM.
function filesGuard({symbol = false, basis = false} = {}) {
  const session = epoch, root = selectedRootKey, pin = status?.revision, generation = filesSerial;
  const surface = sectionSerial, file = filesFile, selected = filesSymbol, sourceBasis = filesBasis;
  return () => session === epoch && root === selectedRootKey && IndexPin.equal(pin, status?.revision) &&
    section === "files" && surface === sectionSerial && file === filesFile &&
    (!symbol || (generation === filesSerial && selected === filesSymbol)) && (!basis || sourceBasis === filesBasis);
}
function clearFilesInspector() {
  filesMatchesSerial++; filesSymbol = null; filesMini = null; filesMatches = null;
  for (const id of ["files-symbol-location", "files-symbol-header", "files-symbol-return", "files-mini-sequence", "files-matches", "files-matches-state"]) $(id)?.replaceChildren();
  if ($("files-symbol-title")) $("files-symbol-title").textContent = "Choose a symbol";
  if ($("files-symbol-kind")) $("files-symbol-kind").textContent = "Indexed symbol";
  if ($("files-matches-title")) $("files-matches-title").textContent = "References";
  if ($("files-mini-state")) $("files-mini-state").textContent = "Select a callable to preview its measured sequence.";
  if ($("files-open-full")) $("files-open-full").disabled = true;
  for (const id of ["files-matches-more", "files-matches-retry"]) if ($(id)) $(id).hidden = true;
}
function clearFilesEvidence() {
  filesSerial++; filesDiffSerial++; filesFile = null; filesBasis = "indexed"; filesDiff = null; filesCatalogLoaded = false;
  clearFilesInspector(); $("files-diff")?.replaceChildren();
  if ($("files-empty")) $("files-empty").hidden = false;
  updateFilesBasis();
}
function filesFailure(error, target, current) {
  if (!current() || error.name === "AbortError") return;
  if (["revision_conflict", "pin_expired", "root_changed"].includes(error.code)) { unexpectedPair("Indexed evidence is stale. Refresh status before selecting again."); return; }
  if ($(target)) $(target).textContent = error.message;
}
async function setSection(name, view = null) {
  if (!["files", "diagrams"].includes(name) || !selectedRootKey) return;
  if (section === name) {
    const target = name === "files" ? filesBasis === "working" ? $("files-diff") : workspaceSource ? $("source") : $("file-filter") : $("view-sequence");
    target?.focus(); return;
  }
  architecture?.reset();
  treemap?.reset();
  section = name; sectionSerial++; sourceSerial++; filesDiffSerial++;
  for (const state of fileStates.values()) if (state.outlineLoading) {
    state.serial++; state.outlineLoading = false; state.outlineError = "Symbol read interrupted. Retry to read this indexed outline.";
  }
  window.TrellisNavigation?.reset(); window.TrellisClasses?.closeContextMenu?.();
  window.TrellisShell?.setSection(name);
  if (name !== "files") {
    // A navigation names its target view: the last-used tab neither shows nor refreshes (#451).
    if (view) {
      // The shell focused the last-used tab on entry; keep focus on the tab that is now shown.
      const tabFocused = document.activeElement?.closest?.("#diagram-tabs");
      showNavigationView(view);
      if (tabFocused) $(`view-${view}`)?.focus();
    }
    $("source").hidden = false; $("files-diff").hidden = true;
    if (!workspaceSource) window.TrellisShell?.closeSource();
    renderFiles();
    const cached = workspaceSource, surface = sectionSerial, session = epoch, root = selectedRootKey, sourceAtStart = sourceSerial;
    const current = () => section === "diagrams" && surface === sectionSerial && session === epoch && root === selectedRootKey && sourceAtStart === sourceSerial && IndexPin.equal(cached?.data.revision, status?.revision);
    if (!$("treemap-panel").hidden) await treemap?.refresh();
    else if (!$("architecture-panel").hidden) await architecture?.refresh();
    else if (diagramSequence && selectedMethod?.id === diagramSequence.seed.id) await loadSequence(diagramSequence, true);
    if (cached && current()) paintCachedSource(cached.data, cached.range, current, cached.range?.startLine || 1, cached.navigation?.snapshot?.());
    return;
  }
  viewerMetrics?.clearInspection();
  // Section changes keep validated evidence but never choose or read a file.
  updateFilesBasis(); renderFiles();
  if (!workspaceSource && filesFile && !filesFile.changeOnly && status?.revision) {
    const cached = sourceCache.get(`${IndexPin.key(status.revision)}:${filesFile.path}`);
    if (cached) workspaceSource = {data:cached, range:filesSymbol?.symbol.range || null};
  }
  if (workspaceSource && filesBasis === "indexed") {
    const source = workspaceSource;
    paintCachedSource(source.data, source.range, filesGuard({basis:true}), source.range?.startLine || 1, source.navigation?.snapshot?.());
  }
  if (filesSymbol) {
    const current = filesGuard({symbol:true});
    if (filesMatches) renderFilesMatches(); else void loadFilesMatches();
    if (filesMini) await renderFilesMini(current);
    else if (["function", "method"].includes(filesSymbol.symbol.kind)) void loadFilesMini(current);
  }
  if (!workspaceSource && filesFile && filesBasis === "indexed") $("source-path").textContent = "Source read interrupted. Select the file again to read its cached snapshot.";
  if (filesBasis === "working") {
    if (filesDiff) window.TrellisFiles?.diff($("files-diff"), filesDiff);
    else $("files-diff").textContent = "Diff read interrupted. Select Working tree again to measure it.";
  }
  if (!filesCatalogLoaded && status?.revision) await loadFiles(true);
  if (!filesGitStatus) await refreshGit();
}
window.TrellisShell?.setSectionCallback?.(name => { navigationSerial++; diagramSerial++; void setSection(name); });
function appendFilesOutline(li, file) {
  const state = fileStates.get(file.path);
  if (!state?.outlineOpen) return;
  const list = element("ul"); li.append(list);
  const guard = filesGuard(), serial = state.serial;
  const current = () => guard() && fileStates.get(file.path) === state && serial === state.serial;
  if (state.outlineLoading) list.append(element("li", "Loading symbols…", "detail"));
  else if (state.outlineError) list.append(button(`Retry symbols · ${state.outlineError}`, () => { if (current()) return loadFilesOutline(file, 0); }));
  else {
    window.TrellisFiles?.outline(list, state.outlineItems || [], filesSymbol, selectFilesSymbol, current);
    if (!state.outlineItems?.length) list.append(element("li", "No measured symbols. Cached source remains readable.", "detail"));
    if (state.outlineNext != null) { const next = state.outlineNext; list.append(button("More symbols", () => { if (current()) return loadFilesOutline(file, next); })); }
    for (const warning of state.outlineWarnings || []) list.append(element("li", warning, "warning"));
    if (state.outlineTruncated) list.append(element("li", "Outline continues on the next page.", "detail"));
  }
}
async function loadFilesOutline(file, offset = 0) {
  if (!status?.revision || filesFile?.path !== file.path) return;
  let state = fileStates.get(file.path);
  if (!state) { state = {open:false, serial:0}; fileStates.set(file.path, state); }
  const current = filesGuard(), serial = ++state.serial, pin = IndexPin.copy(status.revision);
  const active = () => current() && fileStates.get(file.path) === state && serial === state.serial;
  state.outlineOpen = true; state.outlineLoading = true; state.outlineError = null; renderFiles();
  try {
    const data = await api(`/api/symbols?path=${encodeURIComponent(file.path)}&${IndexPin.query(pin)}&offset=${offset}&limit=200`, "GET", undefined, active);
    if (!active()) return;
    if (!IndexPin.equal(data.revision, pin)) { unexpectedPair(undefined, data.revision); return; }
    if (data.path !== file.path || data.items.some(item => item.symbol.path !== file.path)) throw new Error("Outline path mismatch. Nothing displayed.");
    state.outlineItems = offset ? [...(state.outlineItems || []), ...data.items] : data.items;
    state.outlineNext = data.nextOffset; state.outlineWarnings = data.warnings; state.outlineTruncated = data.truncated;
  } catch (error) { if (active()) state.outlineError = error.message; filesFailure(error, "files-state", active); }
  finally { if (active()) { state.outlineLoading = false; renderFiles(); } }
}
async function selectFilesFile(file) {
  if (section !== "files" || !status?.revision) return;
  for (const [path, state] of fileStates) if (path !== file.path) state.outlineOpen = false;
  filesSerial++; filesDiffSerial++; filesFile = {...file}; filesBasis = "indexed"; filesDiff = null; clearFilesInspector();
  $("files-diff").replaceChildren(); $("files-empty").hidden = true; updateFilesBasis();
  const current = filesGuard({symbol:true, basis:true}), pin = IndexPin.copy(status.revision);
  await Promise.all([
    showCachedSource(file.path, null, pin, current).catch(error => filesFailure(error, "source-path", current)),
    loadFilesOutline(file),
  ]);
}
function renderFilesSymbolHeader(item) {
  const symbol = item.symbol, pin = status.revision;
  $("files-symbol-title").textContent = symbol.qualifiedName || symbol.name;
  $("files-symbol-kind").textContent = item.nativeKind;
  $("files-symbol-location").textContent = `${symbol.path}:${symbol.range.startLine}–${symbol.range.endLine} · indexed revision ${IndexPin.label(pin)}`;
  $("files-symbol-header").textContent = window.TrellisFiles?.headerText(item.header) || "Measured header unavailable";
  $("files-symbol-return").textContent = item.header?.resultType ? `Return type · ${item.header.resultType}` : "Return type not recorded";
}
async function selectFilesSymbol(item) {
  if (section !== "files") return;
  if (item.symbol.path !== filesFile?.path) {
    filesFile = files.find(file => file.path === item.symbol.path) || {path:item.symbol.path};
    for (const [path, state] of fileStates) if (path !== item.symbol.path) state.outlineOpen = false;
  }
  filesSerial++; filesDiffSerial++; clearFilesInspector(); filesSymbol = item; filesBasis = "indexed"; filesDiff = null;
  const symbol = item.symbol, pin = IndexPin.copy(status.revision), current = filesGuard({symbol:true});
  renderFilesSymbolHeader(item);
  updateFilesBasis(); renderFiles();
  const callable = ["function", "method"].includes(symbol.kind);
  $("files-mini-state").textContent = callable ? "Loading measured sequence…" : "Sequence not applicable to this noncallable symbol.";
  await Promise.all([
    showCachedSource(symbol.path, symbol.range, pin, filesGuard({symbol:true, basis:true})).catch(error => filesFailure(error, "source-path", current)),
    loadFilesMatches(),
    callable ? loadFilesMini(current) : Promise.resolve(),
  ]);
}
async function loadFilesMatches(continuation = null) {
  if (!filesSymbol) return;
  const item = filesSymbol, pin = IndexPin.copy(status.revision), guard = filesGuard({symbol:true}), serial = ++filesMatchesSerial;
  const current = () => guard() && serial === filesMatchesSerial;
  $("files-matches-state").textContent = "Loading syntax matches…"; $("files-matches-more").hidden = true; $("files-matches-retry").hidden = true;
  if (!continuation) $("files-matches-title").textContent = `References to ${item.symbol.name} · …`;
  try {
    const data = await api("/api/syntax-matches", "POST", {expectedRevision:pin, symbolId:item.symbol.id, ...(continuation ? {continuation} : {})}, current);
    if (!current()) return;
    if (!IndexPin.equal(data.revision, pin)) { unexpectedPair(undefined, data.revision); return; }
    if (data.symbolId !== item.symbol.id || data.evidenceKind !== "syntaxMatches") throw new Error("Syntax match provenance mismatch. Nothing displayed.");
    filesMatches = {...data, items:continuation ? [...(filesMatches?.items || []), ...data.items] : data.items}; renderFilesMatches();
  } catch (error) {
    filesFailure(error, "files-matches-state", current);
    if (current()) { $("files-matches-retry").hidden = false; if (!continuation) $("files-matches-title").textContent = `References to ${item.symbol.name}`; }
  }
}
function renderFilesMatches() {
  $("files-matches").replaceChildren();
  if (!filesMatches || !filesSymbol) return;
  const data = filesMatches, pin = IndexPin.copy(data.revision), current = filesGuard({symbol:true});
  window.TrellisFiles?.matches($("files-matches"), data.items, item => openFilesLocation(item, pin), current);
  // A page is not a total: more pages or incomplete coverage make the count a lower bound.
  const count = !data.scope?.spelling ? "unavailable" : `${data.continuation != null || data.partial ? "≥" : ""}${data.items.length}`;
  $("files-matches-title").textContent = `References to ${data.scope?.spelling || filesSymbol.symbol.name} · ${count}`;
  $("files-matches-state").textContent = `${data.items.length} shown · ${data.scope?.language || "language unavailable"} · source set ${data.scope?.sourceSetId || "unavailable"}${data.scope?.spelling ? " · " + data.scope.spelling : " · spelling unavailable"}${data.partial ? " · Incomplete coverage (skipped files or work budget)" : ""}${data.truncated ? " · more available" : ""}${!data.items.length ? " · No matches returned in the scanned scope; not proof of no references." : ""}${data.warnings?.length ? " · " + data.warnings.join(" · ") : ""}`;
  $("files-matches-more").hidden = data.continuation == null;
}
async function openFilesLocation(item, pin) {
  if (!IndexPin.equal(pin, status?.revision) || section !== "files") return;
  filesSerial++; filesDiffSerial++; clearFilesInspector(); filesFile = files.find(file => file.path === item.path) || {path:item.path}; filesBasis = "indexed"; filesDiff = null;
  $("files-empty").hidden = true; $("files-symbol-title").textContent = "Pinned source location";
  $("files-symbol-location").textContent = `${item.path}:${item.range.startLine} · indexed revision ${IndexPin.label(pin)}`;
  updateFilesBasis(); renderFiles();
  const current = filesGuard({symbol:true, basis:true});
  await showCachedSource(item.path, item.range, pin, current).catch(error => filesFailure(error, "source-path", current));
}
async function loadFilesMini(current) {
  const symbol = filesSymbol.symbol, pin = IndexPin.copy(status.revision);
  try {
    const data = await api("/api/sequence", "POST", {seed:symbol.id, expectedRevision:pin, showAll:false}, current);
    if (!current()) return;
    if (!IndexPin.equal(data.revision, pin)) { unexpectedPair(undefined, data.revision); return; }
    if (data.seed.id !== symbol.id) throw new Error("Sequence provenance mismatch. Nothing displayed.");
    filesMini = data; await renderFilesMini(current);
  } catch (error) { filesFailure(error, "files-mini-state", current); }
}
function sequenceTargetAction(data, current) {
  const context = navigationContext(), revision = IndexPin.copy(data.revision);
  return (declaration, step) => {
    if (!current() || !navigationCurrent(context) || !IndexPin.equal(revision, status?.revision)) return;
    const pending = [...data.steps];
    let found = false;
    while (pending.length) {
      const item = pending.pop();
      if (item === step) { found = true; break; }
      pending.push(...(item.children || []), ...(item.alternate || []));
    }
    if (!found || window.TrellisSequence?.target(data, step)?.declaration !== declaration) return;
    if (["function", "method"].includes(declaration.kind)) return perform(() => navigateDiagram(declaration, revision));
    return perform(() => section === "files" ? openFilesLocation(declaration, revision) : showSource(declaration, revision));
  };
}
async function renderFilesMini(current) {
  if (!filesMini || !filesSymbol) return;
  const data = filesMini, revision = data.revision, expanded = new Set(), container = $("files-mini-sequence");
  const readSource = step => { if (current()) return openFilesLocation(step, revision); };
  const options = {showDetails:false, isCurrent:current, onOpenTarget:sequenceTargetAction(data, current)};
  options.onUnsupported = reason => { if (current()) { window.TrellisSequence.render(container, data, readSource, expanded, options); $("files-mini-state").textContent += ` · Pele fell back: ${reason}`; } };
  const outcome = window.TrellisPeleSequence ? await window.TrellisPeleSequence.render(container, data, readSource, expanded, options) : {status:"unsupported", reason:"Pele asset unavailable"};
  if (!current() || outcome.status === "stale") return;
  if (outcome.status !== "rendered") window.TrellisSequence.render(container, data, readSource, expanded, options);
  $("files-mini-state").textContent = `Indexed revision ${IndexPin.label(revision)} · ${data.hiddenSteps} incidental steps hidden${data.truncated ? " · truncated" : ""}${!data.steps.length ? " · No visible behavior steps" : ""}${data.warnings?.length ? " · " + data.warnings.join(" · ") : ""}${outcome.status !== "rendered" ? " · Pele fell back: " + outcome.reason : ""}`;
  $("files-open-full").disabled = false;
}
async function openFilesFull() {
  if (section !== "files" || !filesMini || !filesSymbol || !IndexPin.equal(filesMini.revision, status?.revision)) return;
  return navigateDiagram(filesSymbol.symbol, filesMini.revision, "sequence", filesMini);
}
function updateFilesBasis() {
  if (!$("files-working")) return;
  const available = selectedGit?.state === "available", working = filesBasis === "working";
  $("files-working").hidden = !available; $("files-working").disabled = !filesFile;
  $("files-indexed").disabled = !filesFile || filesFile.changeOnly === true;
  $("files-indexed").setAttribute("aria-pressed", String(!working)); $("files-working").setAttribute("aria-pressed", String(working));
  $("files-git-state").textContent = available ? "" : `Working tree unavailable · ${selectedGit?.reason || "Git status unavailable"}`;
  $("files-basis-note").textContent = status?.revision ? working ? `Uncommitted vs HEAD · Not indexed. Navigation and diagrams use indexed revision ${IndexPin.label(status.revision)}.` : `Cached source · indexed revision ${IndexPin.label(status.revision)}. Navigation and diagrams use this basis.` : "Select an indexed checkout to read cached source.";
  $("files-diff").hidden = !working; $("source").hidden = working;
  if ($("files-empty")) $("files-empty").hidden = !!filesFile || !!workspaceSource;
}
async function setFilesBasis(basis) {
  if (section !== "files" || !filesFile || !["indexed", "working"].includes(basis) || (basis === "indexed" && filesFile.changeOnly)) return;
  if (basis === "working" && selectedGit?.state !== "available") return;
  filesBasis = basis; const serial = ++filesDiffSerial;
  window.TrellisNavigation?.reset(); filesDiff = null; $("files-diff").replaceChildren(); updateFilesBasis();
  if (basis === "indexed") {
    const current = filesGuard({basis:true});
    if (workspaceSource?.data.file.path === filesFile.path) paintCachedSource(workspaceSource.data, workspaceSource.range, current, workspaceSource.range?.startLine || 1, workspaceSource.navigation?.snapshot?.());
    else await showCachedSource(filesFile.path, filesSymbol?.symbol.range || null, status.revision, current).catch(error => filesFailure(error, "source-path", current));
    return;
  }
  const path = filesFile.path, guard = filesGuard({basis:true}), observation = ++gitSerial;
  const current = () => guard() && serial === filesDiffSerial && observation === gitSerial;
  $("files-diff").append(element("p", "Measuring uncommitted changes against HEAD…"));
  let statusMeasured = false;
  try {
    const statusData = await api("/api/git", "GET", undefined, current);
    if (!current()) return;
    filesGitStatus = statusData; showSelectedGit(statusData.git); statusMeasured = true;
    if (statusData.git.state !== "available") { window.TrellisFiles?.diff($("files-diff"), {git:statusData.git, file:null}); return; }
    if (!statusData.changes?.some(change => change.path === path)) { window.TrellisFiles?.diff($("files-diff"), {git:statusData.git, file:null}); return; }
    const data = await api(`/api/git/diff?path=${encodeURIComponent(path)}`, "GET", undefined, current);
    if (!current()) return;
    if (data.file && data.file.path !== path) throw new Error("Git diff path changed. Refresh to measure again.");
    filesDiff = data;
    window.TrellisFiles?.diff($("files-diff"), data); renderFiles();
  } catch (error) {
    if (!current()) return;
    if (!statusMeasured) { filesGitStatus = null; showSelectedGit(null); }
    filesFailure(error, "files-diff", guard);
  }
}
function appendChangedOnlyPaths(tree, filter) {
  const indexed = new Set(files.map(file => file.path)), markers = gitChangesByPath();
  const changes = filesGitStatus?.changes?.filter(change => !indexed.has(change.path) && change.path.toLowerCase().includes(filter)) || [];
  if (!changes.length) return;
  const group = element("li", "Uncommitted paths · not indexed"), list = element("ul"); group.append(list); tree.append(group);
  const session = epoch, root = selectedRootKey, surface = sectionSerial, pin = status?.revision && IndexPin.copy(status.revision);
  const current = () => session === epoch && root === selectedRootKey && surface === sectionSerial && section === "files" && IndexPin.equal(pin, status?.revision);
  for (const change of changes) {
    const row = element("li"), pick = button(`${change.status} · ${change.path}`, async () => {
      if (!current()) return;
      filesSerial++; filesDiffSerial++; clearFilesInspector(); clearSource(); filesFile = {path:change.path, changeOnly:true};
      $("source-path").textContent = `${change.path} · not in the loaded indexed catalog; no indexed outline or navigation`;
      await setFilesBasis("working");
    });
    pick.className = "files-change-pick"; pick.setAttribute("data-path", change.path);
    appendGitMarker(pick, change.path, markers); row.append(pick); list.append(row);
  }
}
$("files-indexed")?.addEventListener("click", () => setFilesBasis("indexed"));
$("files-working")?.addEventListener("click", () => setFilesBasis("working"));
$("files-open-full")?.addEventListener("click", openFilesFull);
$("files-matches-more")?.addEventListener("click", () => { if (filesMatches?.continuation) return loadFilesMatches(filesMatches.continuation); });
$("files-matches-retry")?.addEventListener("click", () => loadFilesMatches());

// File browser has independent request generations, including failures.
let browseSerial = 0, diagramSerial = 0, files = [], nextFileOffset = null;
let selectedMethod = null, catalogSerial = 0, treeSerial = 0, directoryRefreshSerial = 0, retryCatalogReset = true;
const fileStates = new Map(), closedDirectories = new Set();
let treeMode = false, treeRoot = null, indexedWorkspace = "";
const directories = new Map();
function clearBrowse(message = "Expand a file and select a method.") {
  clearFilesEvidence();
  window.TrellisClasses?.reset();
  architecture?.reset();
  treemap?.reset();
  if ($("method-class")) $("method-class").disabled = true;
  window.TrellisShell?.resetInspector();
  if ($("sequence-warning-count")) $("sequence-warning-count").textContent = "0";
  viewerMetrics?.reset();
  diagramSequence = null;
  browseSerial++; diagramSerial++; catalogSerial++; directoryRefreshSerial++; files = []; nextFileOffset = null; selectedMethod = null;
  clearFileFilterResults();
  directories.clear(); treeMode = false; treeRoot = null; indexedWorkspace = "";
  $("index").textContent = "Index workspace";
  fileStates.clear(); closedDirectories.clear(); $("method-source").disabled = true;
  $("file-tree").replaceChildren(); $("sequence-diagram").replaceChildren(); $("sequence-warnings").replaceChildren();
  $("reveal-method").hidden = true; $("retry-files").hidden = true;
  $("files-state").textContent = ""; $("sequence-state").textContent = message; $("more-files").hidden = true;
}
function browseGuard() {
  const serial = browseSerial, session = epoch, revision = status?.revision && IndexPin.copy(status.revision);
  return () => serial === browseSerial && session === epoch && IndexPin.equal(revision, status?.revision);
}
async function browseRequest(action, current, state) {
  try { await action(); }
  catch (error) {
    if (!current() || error.name === "AbortError") return;
    if (IndexPin.isConflict(error)) { void refreshStatus().catch(() => {}); clearBrowse("Index changed. Refresh status before selecting a method."); clearSource(); }
    $(state).textContent = error.message;
  }
}
async function loadFiles(reset = false) {
  if (!status) return;
  const serial = ++catalogSerial, guard = browseGuard(), surface = sectionSerial;
  const current = () => guard() && serial === catalogSerial && surface === sectionSerial;
  const revision = IndexPin.copy(status.revision), offset = reset ? 0 : nextFileOffset;
  if (offset == null) return;
  $("files-state").textContent = "Loading indexed files…"; $("retry-files").hidden = true;
  let loaded = false; retryCatalogReset = reset;
  await browseRequest(async () => {
    const data = await api(`/api/files?${IndexPin.query(revision)}&offset=${offset}&limit=200`, "GET", undefined, current);
    if (!current()) return;
    if (!IndexPin.equal(data.revision, revision)) { unexpectedPair(undefined, data.revision); throw new Error("File catalog revision does not match. Refresh status."); }
    if (reset && section === "files") files = [];
    const hadFiles = files.length > 0;
    const known = new Set(files.map(f => f.path));
    files.push(...data.items.filter(f => !known.has(f.path)));
    if (!reset || !hadFiles) nextFileOffset = data.nextOffset;
    filesCatalogLoaded = true; renderFiles(); loaded = true;
  }, current, "files-state");
  if (current()) $("retry-files").hidden = loaded;
}
// Dot-entries and folders without indexed files stay out of the way unless asked for (#391).
const hiddenFilesKey = () => `trellis.files.showHidden.v1:${selectedRootKey}`;
let hiddenFilesRoot, hiddenFilesShown = false;
function showHiddenFiles() {
  if (hiddenFilesRoot !== selectedRootKey) {
    hiddenFilesRoot = selectedRootKey;
    try { hiddenFilesShown = window.localStorage.getItem(hiddenFilesKey()) === "true"; } catch { hiddenFilesShown = false; }
  }
  return hiddenFilesShown;
}
function dotPath(path) { return path.split("/").some(part => part.startsWith(".")); }
function hiddenTreeEntry(item) {
  if (item.name.startsWith(".")) return true;
  if (item.kind !== "directory" || !treeRoot || !indexedWorkspace) return false;
  // Only inside the index workspace does the indexed snapshot prove a folder has no indexed files.
  const selection = metricDirectorySelection(item.path);
  return !!selection && viewerMetrics?.indexedDirectory(selection.path) === false;
}
function gitChangesByPath() {
  if (filesGitStatus?.git?.state !== "available" || !Array.isArray(filesGitStatus.changes)) return new Map();
  const paths = new Map();
  for (const change of filesGitStatus.changes) paths.set(change.path, change);
  for (const change of filesGitStatus.changes) {
    if (change.status === "renamed" && change.previousPath && !paths.has(change.previousPath)) paths.set(change.previousPath, change);
  }
  return paths;
}
function appendGitMarker(node, path, changes) {
  const change = changes.get(path);
  const labels = {modified:["M", "Modified"], added:["A", "Added"], deleted:["D", "Deleted"], renamed:["R", "Renamed"]};
  const label = labels[change?.status];
  if (!label) return;
  const marker = element("span", label[0], "git-marker");
  const rename = change.status === "renamed" ? path === change.previousPath ? ` · to ${change.path}` : ` · from ${change.previousPath}` : "";
  marker.title = `${label[1]} · uncommitted vs HEAD${rename}`;
  marker.setAttribute("aria-label", marker.title);
  marker.setAttribute("data-status", change.status);
  marker.setAttribute("data-path", path);
  node.append(marker);
}
function renderFiles() {
  $("show-hidden-files").checked = showHiddenFiles();
  const filter = $("file-filter").value.trim().toLowerCase(), filtering = !!filter;
  if (section !== "files" && treeMode && !filtering) { renderDirectoryTree(); return; }
  const generation = ++treeSerial;
  const tree = $("file-tree"); tree.replaceChildren();
  // A filter shows only the server's whole-catalog matches for the current query.
  const listed = filtering ? filterResults?.query === filter ? filterResults.items : [] : files;
  const visible = listed.filter(f => showHiddenFiles() || !dotPath(f.path));
  const changes = gitChangesByPath(), metricRows = [];
  const dirs = new Map([["", tree]]);
  for (const file of visible) {
    const parts = file.path.split("/"); let parent = tree, key = "";
    for (const part of parts.slice(0,-1)) {
      key += part + "/";
      if (!dirs.has(key)) {
        const li = element("li"), details = element("details"), summary = element("summary", part), list = element("ul");
        const directory = key;
        let lastOpen = !!filter || !closedDirectories.has(directory);
        details.open = lastOpen;
        details.addEventListener("toggle", () => {
          // Native toggle events are queued, including opens on replaced trees.
          if (generation !== treeSerial || filter || details.open === lastOpen) return;
          lastOpen = details.open;
          if (details.open) closedDirectories.delete(directory); else closedDirectories.add(directory);
        });
        details.append(summary, list); li.append(details); parent.append(li); dirs.set(key, list);
      }
      parent = dirs.get(key);
    }
    const state = fileStates.get(file.path), li = element("li");
    const fileCurrent = browseGuard(), fileSurface = sectionSerial;
    const selection = {scope:"file", path:file.path};
    const pick = button(`${state?.open ? "▾" : "▸"} ${parts.at(-1)}${section === "files" ? "" : " · " + file.methodCount}`, () => { if (!fileCurrent() || fileSurface !== sectionSerial) return; if (section === "files") return selectFilesFile(file); inspectTreeSelection(selection); return toggleFile(file); });
    pick.className = "file-pick"; pick.title = file.path;
    appendGitMarker(pick, file.path, changes);
    if (section !== "files") attachClassMenu(pick, {path:file.path});
    pick.setAttribute("aria-expanded", String(section === "files" ? !!state?.outlineOpen : !!state?.open));
    if (section !== "files") metricRows.push(viewerMetrics?.decorate(pick, selection)); li.append(pick);
    if (section === "files") appendFilesOutline(li, file); else appendMethods(li, file);
    parent.append(li);
  }
  viewerMetrics?.renderLegend(metricRows.filter(Boolean));
  $("browse-root").hidden = true;
  $("reveal-method").hidden = !selectedMethod;
  if (filtering) {
    const searching = filterResults?.query !== filter;
    if (!searching && !visible.length) tree.append(element("li", "No indexed files match.", "detail"));
    $("files-state").textContent = filterError || (searching ? "Searching indexed files…"
      : `${visible.length} matching indexed ${visible.length === 1 ? "file" : "files"}${filterResults.next != null ? " · more available" : ""}`);
    $("more-files").hidden = searching || filterResults.next == null;
    $("more-files").textContent = "Load more matches";
  } else {
    if (!visible.length) tree.append(element("li", files.length ? "No indexed files to show." : "No indexed files yet. Use Index workspace above, then browse the tree.", "detail"));
    $("files-state").textContent = `${visible.length} of ${files.length} loaded files${nextFileOffset != null ? " · more available" : ""}`;
    $("more-files").hidden = nextFileOffset == null;
    $("more-files").textContent = "Load more files";
  }
  if (section === "files") appendChangedOnlyPaths(tree, filter);
}
async function toggleFile(file) {
  let state = fileStates.get(file.path);
  if (!state) { state = {open:false, serial:0}; fileStates.set(file.path, state); }
  state.open = !state.open; const serial = ++state.serial;
  if (!state.open || state.items) { state.loading = false; renderFiles(); return; }
  state.loading = true; state.error = null; renderFiles();
  const guard = browseGuard(), revision = IndexPin.copy(status.revision);
  const current = () => guard() && fileStates.get(file.path) === state && state.serial === serial && state.open;
  try {
    const data = await api(`/api/methods?${IndexPin.query(revision)}&path=${encodeURIComponent(file.path)}`);
    if (!current()) return;
    if (!IndexPin.equal(data.revision, revision)) { unexpectedPair(undefined, data.revision); throw new Error("Methods revision mismatch. Refresh status."); }
    state.items = data.items; state.truncated = data.truncated;
  } catch (error) {
    if (!current() || error.name === "AbortError") return;
    if (IndexPin.isConflict(error)) { void refreshStatus().catch(() => {}); clearBrowse("Index changed. Refresh status."); clearSource(); return; }
    state.error = error.message;
  } finally { if (current()) { state.loading = false; renderFiles(); } }
}
async function selectMethod(symbol, admitted = null, callerCurrent = null) {
  if (!callerCurrent) {
    const context = navigationContext(), ticket = ++navigationSerial;
    callerCurrent = () => ticket === navigationSerial && navigationCurrent(context);
  }
  if (!callerCurrent()) return;
  sourceInspector = null;
  viewerMetrics?.clearInspection();
  window.TrellisShell?.showView("sequence");
  if ($("method-class")) $("method-class").disabled = !classLanguage(symbol.path);
  selectedMethod = symbol; $("method-source").disabled = false;
  renderSequenceStart();
  clearSource(); renderFiles();
  await loadSequence(admitted, false, callerCurrent);
  if (callerCurrent() && selectedMethod === symbol && window.matchMedia?.("(max-width: 900px)").matches) $("sequence-title").scrollIntoView?.({block:"start"});
}
async function loadSequence(admitted = null, preserveSource = false, callerCurrent = () => true) {
  viewerMetrics?.clearInspection();
  window.TrellisShell?.resetInspector();
  if ($("sequence-warning-count")) $("sequence-warning-count").textContent = "0";
  if (!selectedMethod || !status) return;
  const serial = ++diagramSerial, guard = browseGuard(), symbol = selectedMethod, revision = IndexPin.copy(status.revision);
  const surface = sectionSerial;
  let published = false;
  const current = () => (published || callerCurrent()) && guard() && section === "diagrams" && surface === sectionSerial && serial === diagramSerial && selectedMethod === symbol;
  if (!preserveSource) clearSource();
  $("sequence-diagram").replaceChildren(); $("sequence-warnings").replaceChildren();
  $("sequence-state").textContent = `Loading ${symbol.name}…`;
  await browseRequest(async () => {
    const data = admitted || await api("/api/sequence", "POST", {seed:symbol.id, expectedRevision:revision, showAll:!!$("all-steps").checked}, current);
    if (!current()) return;
    if (!IndexPin.equal(data.revision, revision)) { unexpectedPair(undefined, data.revision); throw new Error("Sequence provenance mismatch. Nothing displayed."); }
    if (data.seed.id !== symbol.id) throw new Error("Sequence provenance mismatch. Nothing displayed.");
    diagramSequence = data;
    const readSource = step => { if (current()) return perform(() => showSource(step, revision)); };
    const options = {showDetails: !!$("all-steps").checked, isCurrent: () => current() && IndexPin.equal(status?.revision, revision)};
    options.onOpenTarget = sequenceTargetAction(data, options.isCurrent);
    // A later group redraw can fail after the initial load succeeded. Preserve
    // the whole pinned DTO rather than leaving an inert, silently folded group.
    options.onUnsupported = reason => {
      if (!options.isCurrent()) return;
      window.TrellisSequence.render($("sequence-diagram"), data, readSource, expanded, options);
      $("sequence-state").textContent = $("sequence-state").textContent.replace(/ · Pele rendered$/, ` · Pele fell back: ${reason}`);
    };
    if (window.TrellisShell) options.onSelect = step => {
      if (!current()) return;
      sourceInspector = {symbol:step, sequence:data}; viewerMetrics?.clearInspection(); window.TrellisShell?.selectStep(step, data, () => readSource(step), options.onOpenTarget);
      const entry = window.TrellisSequence?.entry(step) || step, evidence = entry.callEvidence, pkg = evidence?.externalPackage;
      const open = evidence?.boundaryReason === "external" && pkg && libraryButton({ecosystem:pkg.manager, name:pkg.name, version:pkg.version, origin:`external call ${entry.label}`});
      if (open) $("inspector-detail")?.prepend(open);
    };
    const diagram = $("sequence-diagram");
    const expanded = new Set();
    const outcome = window.TrellisPeleSequence
      ? await window.TrellisPeleSequence.render(diagram, data, readSource, expanded, options)
      : {status:"unsupported", reason:"Pele asset unavailable"};
    if (!current() || !IndexPin.equal(status?.revision, revision)) return;
    if (outcome.status === "stale") return;
    if (outcome.status !== "rendered") {
      window.TrellisSequence.render(diagram, data, readSource, expanded, options);
    }
    if ($("sequence-warning-count")) $("sequence-warning-count").textContent = String((data.warnings || []).length);
    $("sequence-state").textContent = `${symbol.name} · ${symbol.path} · revision ${IndexPin.label(revision)} · ${data.hiddenSteps} incidental steps hidden${data.truncated ? " · truncated" : ""}${!data.steps.length ? " · No visible behavior steps. Read method source for context." : ""}${outcome.status !== "rendered" ? ` · Pele fell back: ${outcome.reason}` : " · Pele rendered"}`;
    for (const warning of data.warnings || []) $("sequence-warnings").append(element("li", warning, "warning"));
    published = true;
  }, current, "sequence-state");
}
// Bounded by the server: at most 256 bytes and one 200-row page per request.
let filterResults = null, filterError = "", filterSerial = 0, filterTimer = null;
const filterQueryBytes = 256;
function clearFileFilterResults() { filterSerial++; clearTimeout(filterTimer); filterTimer = null; filterResults = null; filterError = ""; }
async function searchFiles(offset = 0) {
  const raw = $("file-filter").value.trim(), query = raw.toLowerCase(), serial = ++filterSerial;
  filterError = "";
  if (!offset) filterResults = null;
  if (!query) { filterResults = null; renderFiles(); return; }
  if (!status?.revision) { filterResults = {query, items:[], next:null}; filterError = "No usable index to search."; renderFiles(); return; }
  if (new TextEncoder().encode(raw).length > filterQueryBytes) { filterError = "Filter is too long."; renderFiles(); return; }
  const guard = browseGuard(), revision = IndexPin.copy(status.revision);
  const current = () => guard() && serial === filterSerial && $("file-filter").value.trim().toLowerCase() === query;
  renderFiles();
  try {
    const data = await api(`/api/files?${IndexPin.query(revision)}&q=${encodeURIComponent(raw)}&offset=${offset}&limit=200`, "GET", undefined, current);
    if (!current()) return;
    if (!IndexPin.equal(data.revision, revision)) { unexpectedPair(undefined, data.revision); return; }
    const earlier = offset && filterResults?.query === query ? filterResults.items : [];
    filterResults = {query, items:[...earlier, ...data.items], next:data.nextOffset};
  } catch (error) {
    if (!current() || error.name === "AbortError") return;
    // A stale pin rebinds like other browse reads; a failed later page keeps the matches already shown.
    if (IndexPin.isConflict(error)) { unexpectedPair("Index changed. Refresh status before filtering again."); return; }
    const earlier = offset && filterResults?.query === query ? filterResults.items : [];
    filterResults = {query, items:earlier, next:null}; filterError = `Filter unavailable: ${error.message}`;
  }
  renderFiles();
}
$("file-filter").addEventListener("input", () => {
  // Old matches never stay on screen under a new query.
  clearFileFilterResults(); renderFiles();
  if (!$("file-filter").value.trim()) return;
  filterTimer = setTimeout(() => { filterTimer = null; void searchFiles(); }, 150);
});
$("show-hidden-files").addEventListener("change", event => {
  showHiddenFiles(); hiddenFilesShown = event.target.checked;
  try { window.localStorage.setItem(hiddenFilesKey(), String(hiddenFilesShown)); } catch { /* Not remembered; this page still applies it. */ }
  renderFiles();
});
$("all-methods").addEventListener("change", renderFiles);
$("all-steps").addEventListener("change", () => loadSequence());
$("more-files").addEventListener("click", () => perform(() => $("file-filter").value.trim() && filterResults?.next != null ? searchFiles(filterResults.next) : loadFiles(), $("more-files")));

$("method-source").addEventListener("click", () => { if (selectedMethod && status) perform(() => showSource(selectedMethod, status.revision)); });

$("retry-files").addEventListener("click", () => perform(() => loadFiles(retryCatalogReset), $("retry-files")));
// The live tree path of an indexed path, or null outside the browse root.
function treePathOf(indexedPath) {
  if (!treeRoot || !indexedWorkspace) return null;
  const absolute = `${indexedWorkspace.replace(/\/$/, "")}/${indexedPath}`, base = treeRoot.replace(/\/$/, "");
  return absolute.startsWith(base + "/") ? absolute.slice(base.length + 1) : null;
}
// Reveal opens (loading as needed) every live ancestor folder, even ones never opened, e.g. after
// the method was found through the whole-catalog filter.
async function revealSelectedMethod() {
  const method = selectedMethod;
  if (!method) return;
  const guard = browseGuard(), current = () => guard() && selectedMethod === method;
  $("file-filter").value = ""; clearFileFilterResults();
  // Without a known root pair, an already-listed entry still names its live path.
  const listed = () => { for (const state of directories.values()) { const entry = state.items?.find(item => item.indexedPath === method.path); if (entry) return entry.path; } return null; };
  const relative = treeMode ? treePathOf(method.path) ?? listed() : null;
  // Page a folder's listing until it holds `path`, or the listing ends or fails.
  const pageUntil = async (folder, path) => {
    const state = directoryState(folder, true);
    while (!state.items?.some(item => item.path === path) && (state.items === null || state.nextOffset != null)) {
      const before = state.items?.length ?? -1;
      await loadDirectory(folder, state.items === null);
      if (!current()) return false;
      if (state.error || (state.items?.length ?? -1) === before) break;
    }
    return true;
  };
  if (relative) {
    let parent = "";
    directoryState("", true).open = true;
    for (const part of relative.split("/").slice(0, -1)) {
      const path = parent ? `${parent}/${part}` : part;
      if (!await pageUntil(parent, path)) return;
      directoryState(path, true).open = true;
      parent = path;
    }
    // The file itself may be on a later page of its folder, loaded or not.
    if (!await pageUntil(parent, relative)) return;
  }
  const parts = method.path.split("/"); let directory = "";
  for (const part of parts.slice(0, -1)) { directory += part + "/"; closedDirectories.delete(directory); }
  if (!fileStates.get(method.path)?.open) { await toggleFile({path:method.path}); if (!current()) return; }
  $("all-methods").checked = true;
  renderFiles();
  const pick = $("file-tree").querySelector(".method-pick.selected");
  pick?.focus({preventScroll:true}); pick?.scrollIntoView?.({block:"nearest"});
}
$("reveal-method").addEventListener("click", () => perform(revealSelectedMethod, $("reveal-method")));

function appendMethods(li, file) {
  const state = fileStates.get(file.path);
    if (state?.open) {
      const list = element("ul");
      if (state.loading) list.append(element("li", "Loading methods…", "detail"));
      else if (state.error) { list.append(element("li", state.error, "warning")); list.append(button("Retry methods", () => { state.open = false; return toggleFile(file); })); }
      else {
        const shown = (state.items || []).filter(m => $("all-methods").checked || m.consequential !== false);
        for (const method of shown) {
          const symbol = method.symbol, row = element("li");
          const choose = button(`${symbol.name} · L${symbol.range.startLine}`, () => selectMethod(symbol));
          choose.className = "method-pick" + (selectedMethod?.id === symbol.id ? " selected" : "");
          choose.setAttribute("aria-pressed", String(selectedMethod?.id === symbol.id)); choose.title = method.reason;
          attachClassMenu(choose, {seed:symbol.id, path:symbol.path});
          row.append(choose); list.append(row);
        }
        const hidden = (state.items || []).length - shown.length;
        if (!shown.length) list.append(element("li", state.items?.length ? "No methods match. Try Show all methods." : "No indexed methods in this file.", "detail"));
        if (hidden) list.append(element("li", `${hidden} methods hidden by conservative heuristic.`, "detail"));
        if (state.truncated) list.append(element("li", "Method list truncated by index limits.", "warning"));
      }
      li.append(list);
    }
}

// Filesystem metadata is separate from the immutable indexed source catalog.
async function loadTreeRoot() {
  // A filter kept across a new pin searches the new catalog.
  if ($("file-filter").value.trim()) void searchFiles();
  if (section === "files") return loadFiles(true);
  treeMode = true;
  await loadDirectory("", true);
}
function openDirectoryAnchors(path = "", found = [], visited = new Set()) {
  if (visited.has(path)) return found;
  visited.add(path);
  const state = directories.get(path);
  for (const item of state?.items || []) {
    if (item.kind !== "directory") continue;
    const child = directories.get(item.path);
    if (!child?.open) continue;
    found.push(item.path);
    openDirectoryAnchors(compactDirectory(item).last.path, found, visited);
  }
  return found;
}
async function refreshTree() {
  if (section === "files") return loadFiles(true);
  if (!treeMode) return loadTreeRoot();
  const serial = ++directoryRefreshSerial, guard = browseGuard();
  const anchors = openDirectoryAnchors();
  const paths = [...directories.keys()];
  await Promise.all(paths.map(path => loadDirectory(path, true)));
  if (!guard() || serial !== directoryRefreshSerial) return;
  for (const path of anchors) {
    if (!guard() || serial !== directoryRefreshSerial) return;
    if (directories.get(path)?.open) await expandDirectoryChain(path);
  }
}
const compactDirectoryDepth = 32;
const viewerMetrics = window.TrellisMetrics?.create({
  request: path => api(path),
  context: () => ({rootKey:selectedRootKey, session:epoch, pin:status?.revision, treeRoot, indexedWorkspace}),
  onConflict: pin => unexpectedPair("Index changed. Refresh status before inspecting metrics.", pin),
  onChange: () => { renderFiles(); updateViewAvailability(); renderSequenceStart(); },
  onOpenCallable: (row, pin) => perform(() => openCallableSequence(row, pin)),
});
const treemap = window.TrellisTreemap?.create({
  request: (path, current) => api(path, "GET", undefined, current),
  context: () => ({rootKey:selectedRootKey, session:epoch, pin:status?.revision, workspaceRoot:status?.workspaceRoot}),
  readSource: (site, pin, current) => perform(() => showCachedSource(site.path, site.range, pin, current)),
  onConflict: () => unexpectedPair("Treemap snapshot changed. Refresh status before reopening Treemap."),
  onInspect: () => { viewerMetrics?.clearInspection(); architecture?.clearInspection(); clearSource(); },
});
const architecture = window.TrellisArchitecture?.create({
  request: (path, current, options = {}) => api(path, options.method || "GET", options.body, current),
  context: () => ({rootKey:selectedRootKey, session:epoch, pin:status?.revision}),
  readSource: (site, pin, current) => perform(() => showCachedSource(site.path, site.range, pin, current)),
  onConflict: () => unexpectedPair("Architecture snapshot changed. Refresh status before reopening Architecture."),
  openTreemap: (identity, pin) => {
    if (!IndexPin.equal(pin, status?.revision)) return;
    window.TrellisShell?.showView("treemap");
    void treemap?.openFile(identity, pin);
  },
  libraryButton: identity => identity.standardLibrary ? null : libraryButton({ecosystem:identity.language, name:identity.package, origin:"Architecture reference"}),
  onInspect: () => { viewerMetrics?.clearInspection(); clearSource(); },
  // A rules edit can change which layers are application layers (#524).
  onRulesChanged: () => { void viewerMetrics?.refresh(); },
});
window.TrellisShell?.setViewCallback?.(name => {
  if (!routeViewChange && name !== "sequence") {
    navigationSerial++; diagramSerial++;
    if (navigationEntries.get(window.history?.state?.trellisNavigation)?.kind === "class") window.TrellisClasses?.reset();
  }
  architecture?.clearInspection(); treemap?.clearInspection();
  if (name === "architecture" && section === "diagrams") void architecture?.refresh();
  else architecture?.reset();
  if (name === "treemap" && section === "diagrams") void treemap?.refresh();
  else treemap?.reset();
});
window.TrellisShell?.setInspectorClearCallback?.(() => { sourceInspector = null; viewerMetrics?.clearInspection(); architecture?.clearInspection(); treemap?.clearInspection(); });
function metricDirectorySelection(path) {
  if (!treeRoot || !indexedWorkspace) return null;
  const absolute = `${treeRoot.replace(/\/$/, "")}${path ? "/" + path : ""}`;
  const base = indexedWorkspace.replace(/\/$/, "");
  const relative = absolute === base ? "" : absolute.startsWith(base + "/") ? absolute.slice(base.length + 1) : null;
  return relative == null ? null : {scope:"directory", path:relative};
}
function inspectTreeSelection(selection) {
  architecture?.clearInspection();
  treemap?.clearInspection();
  if (selection) void viewerMetrics?.inspect(selection);
  else viewerMetrics?.clearInspection();
}
function directoryState(path, open = false) {
  let state = directories.get(path);
  if (!state) { state = {open, items:null, serial:0, expansionSerial:0}; directories.set(path, state); }
  return state;
}
function completeSoleDirectory(state) {
  return !!state && !state.loading && !state.error && state.nextOffset === null && !state.truncated &&
    state.items?.length === 1 && state.items[0].kind === "directory";
}
function compactDirectory(item) {
  const items = [item];
  while (items.length < compactDirectoryDepth) {
    const state = directories.get(items.at(-1).path);
    if (!completeSoleDirectory(state)) break;
    items.push(state.items[0]);
  }
  return {items, first:items[0], last:items.at(-1)};
}
async function expandDirectoryChain(path) {
  const state = directories.get(path), guard = browseGuard();
  if (!state?.open) return;
  const expansion = state.expansionSerial = (state.expansionSerial || 0) + 1;
  const current = () => guard() && directories.get(path) === state && state.open && state.expansionSerial === expansion;
  let nextPath = path;
  for (let depth = 0; depth < compactDirectoryDepth; depth++) {
    if (!current()) return;
    const next = directoryState(nextPath, true);
    next.open = true;
    if (next.items === null) {
      if (next.loading && next.pending) await next.pending;
      else await loadDirectory(nextPath, true);
      if (!current() || directories.get(nextPath) !== next) return;
    }
    if (!completeSoleDirectory(next)) return;
    nextPath = next.items[0].path;
  }
}
async function toggleDirectory(path) {
  const state = directoryState(path);
  state.open = !state.open;
  if (!state.open) state.expansionSerial = (state.expansionSerial || 0) + 1;
  renderDirectoryTree();
  if (state.open) await expandDirectoryChain(path);
}
async function retryDirectory(path, anchorPath) {
  const anchor = directories.get(anchorPath), guard = browseGuard();
  const expansion = anchor?.expansionSerial;
  await loadDirectory(path, directories.get(path)?.retryReset);
  if (!anchorPath || !guard() || directories.get(anchorPath) !== anchor || !anchor.open ||
      anchor.expansionSerial !== expansion || directories.get(path)?.error) return;
  await expandDirectoryChain(anchorPath);
}
async function loadDirectory(path, reset = false) {
  if (!status) return;
  const state = directoryState(path, true);
  const offset = reset ? 0 : state.nextOffset;
  if (offset == null) return;
  const guard = browseGuard(), serial = ++state.serial;
  const current = () => guard() && directories.get(path) === state && serial === state.serial;
  state.loading = true; state.error = null; state.retryReset = reset; renderDirectoryTree();
  const pending = (async () => {
    try {
      const data = await api(`/api/tree?path=${encodeURIComponent(path)}&offset=${offset}&limit=200`);
      if (!current()) return;
      if (!IndexPin.equal(data.revision, status.revision)) { unexpectedPair(undefined, data.revision); throw new Error("Index changed. Refresh status before browsing methods."); }
      if (data.path !== path) throw new Error("Directory response does not match the requested path.");
      if (treeRoot !== null && treeRoot !== data.root) {
        clearBrowse("Browser root changed. Choose a method again.");
        treeMode = true; await loadTreeRoot(); return;
      }
      treeRoot = data.root; indexedWorkspace = data.indexedWorkspace;
      // Re-read already loaded pages on refresh, without hiding a later-page selection.
      const targetCount = reset ? (state.items?.length || 0) : 0;
      while (data.items.length < targetCount && data.nextOffset != null) {
        const next = data.nextOffset;
        const page = await api(`/api/tree?path=${encodeURIComponent(path)}&offset=${next}&limit=200`);
        if (!current()) return;
        if (page.root !== data.root || page.path !== path || !IndexPin.equal(page.revision, data.revision) || (page.nextOffset != null && page.nextOffset <= next))
          { unexpectedPair(undefined, page.revision); throw new Error("Directory changed while refreshing. Retry folder."); }
        data.items.push(...page.items); data.nextOffset = page.nextOffset; data.truncated ||= page.truncated;
      }
      // Keep per-file and child expansion state.
      const known = new Set((reset ? [] : state.items || []).map(item => item.path));
      state.items = [...(reset ? [] : state.items || []), ...data.items.filter(item => !known.has(item.path))];
      state.nextOffset = data.nextOffset; state.truncated = data.truncated;
    } catch (error) {
      if (current() && error.name !== "AbortError") state.error = error.message;
    } finally {
      if (current()) { state.loading = false; renderDirectoryTree(); }
    }
  })();
  state.pending = pending;
  try { await pending; }
  finally { if (state.pending === pending) state.pending = null; }
}
// Files of a type Trellis does not index collapse into one quiet, countable marker per folder (#425).
const unsupportedSourceReason = "Unsupported source type";
function unsupportedTreeFile(item) {
  return item.kind === "file" && item.indexedPath == null && item.unindexedReason === unsupportedSourceReason;
}
function unsupportedFilesMarker(items, changes) {
  const row = element("li", undefined, "unsupported-files"), details = element("details");
  details.append(element("summary", `${items.length} ${items.length === 1 ? "file" : "files"} · unsupported source type · not indexed`));
  const list = element("ul");
  for (const item of items) {
    const name = element("li", item.name, "unindexed-file");
    appendGitMarker(name, item.path, changes); list.append(name);
  }
  details.append(list); row.append(details);
  return row;
}
function renderDirectoryTree() {
  treeSerial++;
  const tree = $("file-tree"); tree.replaceChildren();
  const scopeMismatch = treeRoot && indexedWorkspace && treeRoot !== indexedWorkspace;
  $("browse-root").textContent = scopeMismatch
    ? `Browsing: ${treeRoot} · Index workspace: ${indexedWorkspace}. Indexing applies only to the index workspace.`
    : treeRoot || "Working directory";
  $("browse-root").hidden = !scopeMismatch;
  $("index").textContent = scopeMismatch
    ? `Index ${indexedWorkspace.split("/").filter(Boolean).at(-1) || indexedWorkspace}`
    : "Index workspace";
  // A non-empty filter shows only whole-catalog matches, even while folders (re)load.
  if ($("file-filter").value.trim()) { renderFiles(); return; }
  $("reveal-method").hidden = !selectedMethod;
  $("retry-files").hidden = true; $("more-files").hidden = true;
  let count = 0, hidden = 0;
  const metricRows = [], showHidden = showHiddenFiles();
  const changes = treeRoot === selectedWorkspaceRoot ? gitChangesByPath() : new Map();
  function rows(path, list, anchorPath = path) {
    const state = directories.get(path);
    if (!state) return;
    const unsupported = [];
    for (const item of state.items || []) {
      if (!showHidden && hiddenTreeEntry(item)) { hidden++; continue; }
      count++;
      if (unsupportedTreeFile(item)) { unsupported.push(item); continue; }
      const row = element("li");
      if (item.kind === "directory") {
        const compact = compactDirectory(item), child = directories.get(item.path);
        const open = !!child?.open;
        const label = compact.items.map(entry => entry.name).join("/") + "/";
        const fullPath = compact.last.path + "/";
        const selection = metricDirectorySelection(compact.last.path);
        const pick = button(`${open ? "▾" : "▸"} ${label}`, () => { inspectTreeSelection(selection); return toggleDirectory(item.path); });
        pick.className = "file-pick directory-pick"; pick.title = fullPath;
        pick.setAttribute("aria-label", `${open ? "Collapse" : "Expand"} folder ${fullPath}`);
        pick.setAttribute("aria-expanded", String(open));
        metricRows.push(viewerMetrics?.decorate(pick, selection));
        row.append(pick);
        if (open) { const children = element("ul"); rows(compact.last.path, children, item.path); row.append(children); }
      } else if (item.kind === "file" && item.indexedPath != null) {
        const file = {path:item.indexedPath, methodCount:item.methodCount};
        const open = !!fileStates.get(file.path)?.open;
        const selection = {scope:"file", path:item.indexedPath};
        const pick = button(`${open ? "▾" : "▸"} ${item.name} · ${item.methodCount ?? 0}`, () => { inspectTreeSelection(selection); return toggleFile(file); });
        pick.className = "file-pick"; pick.title = item.path;
        appendGitMarker(pick, item.path, changes);
        attachClassMenu(pick, {path:file.path});
        pick.setAttribute("aria-expanded", String(open));
        metricRows.push(viewerMetrics?.decorate(pick, selection));
        row.append(pick); appendMethods(row, file);
      } else {
        const name = element("span", item.name, "unindexed-file");
        if (item.kind === "file") appendGitMarker(name, item.path, changes);
        row.append(name, element("span",
          item.kind === "symlink" ? "Symbolic link · not followed" : item.kind === "file" ? (item.unindexedReason || "Not indexed in current workspace") : "Special entry · metadata only", "detail"));
      }
      list.append(row);
    }
    if (unsupported.length) list.append(unsupportedFilesMarker(unsupported, changes));
    if (state.loading) list.append(element("li", "Loading folder…", "detail"));
    if (state.error) {
      const row = element("li", state.error, "warning");
      row.append(button("Retry folder", () => retryDirectory(path, anchorPath))); list.append(row);
    }
    if (!state.loading && !state.error && !state.items?.length) list.append(element("li", "Empty folder.", "detail"));
    if (state.nextOffset != null) {
      const row = element("li"), more = button("Load more entries", () => loadDirectory(path));
      more.disabled = !!state.loading; row.append(more); list.append(row);
    }
    if (state.truncated) list.append(element("li", "Directory scan limit reached. Some entries are omitted.", "warning"));
  }
  rows("", tree);
  viewerMetrics?.renderLegend(metricRows.filter(Boolean));
  $("files-state").textContent = hidden ? `${hidden} hidden or unindexed ${hidden === 1 ? "entry" : "entries"} · ⋯ to show` : "";
}

// External libraries are candidate definitions, never workspace evidence.
let externalSerial = 0, externalFileSerial = 0, externalSnapshot = null, externalCatalogId = null;
let externalWindowStart = 0, externalSelectedRange = null;
function resetExternalWindow() {
  externalWindowStart = 0; externalSelectedRange = null;
  $("external-source-hint").hidden = true;
  $("external-previous").disabled = true; $("external-next").disabled = true;
}
function clearExternalSources() {
  externalSerial++; externalFileSerial++; externalSnapshot = null; externalCatalogId = null; resetExternalWindow();
  $("external-definitions").replaceChildren();
  $("external-source").replaceChildren(); $("external-warnings").replaceChildren();
  $("external-source-path").textContent = "Choose an indexed definition to read a candidate snapshot.";
  $("external-filter").value = ""; $("external-filter").disabled = true;
}
function externalButton(text, action) {
  const node = element("button", text); node.type = "button";
  node.addEventListener("click", () => { if (!node.disabled) return action(); });
  return node;
}
function externalGuard() {
  const session = epoch, serial = externalSerial;
  return () => session === epoch && serial === externalSerial;
}
function renderExternalDefinitions() {
  $("external-definitions").replaceChildren();
  if (!externalSnapshot) return;
  const snapshot = externalSnapshot, filter = $("external-filter").value.toLowerCase();
  const byId = new Map(snapshot.definitions.map(item => [item.id, item]));
  function label(item) {
    const names = [item.name], seen = new Set([item.id]);
    let parent = byId.get(item.parent);
    while (parent && !seen.has(parent.id)) {
      names.unshift(parent.name); seen.add(parent.id); parent = byId.get(parent.parent);
    }
    return names.join("::");
  }
  const definitions = snapshot.definitions.filter(item => label(item).toLowerCase().includes(filter));
  for (const definition of definitions) {
    const li = element("li");
    li.append(externalButton(`${label(definition)} · ${definition.kind} · line ${definition.range.startLine}`, () => {
      if (externalSnapshot === snapshot) renderExternalSource(definition.range);
    })); $("external-definitions").append(li);
  }
  if (!definitions.length) $("external-definitions").append(element("li", "No candidate definitions match."));
}
function renderExternalSource(range = externalSelectedRange, windowStart) {
  if (!externalSnapshot) return;
  const fragment = document.createDocumentFragment();
  // Scan offsets without allocating an array for every line of a 2 MiB file.
  const text = externalSnapshot.file.text, limit = 600;
  let total = 1, cursor = 0, newline;
  while ((newline = text.indexOf("\n", cursor)) !== -1) { total++; cursor = newline + 1; }
  const start = Math.max(0, Math.min(total - 1, windowStart ?? ((range?.startLine || 1) - 21)));
  const end = Math.min(total, start + limit);
  externalWindowStart = start; externalSelectedRange = range;
  $("external-previous").disabled = start === 0; $("external-next").disabled = end === total;
  if (start > 0 || end < total) fragment.append(element("span", `Showing lines ${start + 1}–${end} of ${total}. Use Previous/Next lines or select a definition; full text remains cached.`, "source-line warning"));
  cursor = 0;
  for (let index = 0; index < start; index++) cursor = text.indexOf("\n", cursor) + 1;
  for (let index = start; index < end; index++) {
    const number = index + 1, line = element("span", undefined, "source-line");
    newline = text.indexOf("\n", cursor);
    const content = text.slice(cursor, newline === -1 ? text.length : newline);
    cursor = newline === -1 ? text.length : newline + 1;
    if (range && number >= range.startLine && number <= range.endLine) line.classList.add("highlight");
    line.append(element("span", String(number), "line-number"), document.createTextNode(content)); fragment.append(line);
  }
  $("external-source-hint").textContent = "Workspace diagrams are unavailable for this external candidate snapshot.";
  $("external-source-hint").hidden = false;
  $("external-source").replaceChildren(fragment);
  if (range) {
    $("external-source").focus({preventScroll:true});
    const highlight = $("external-source").querySelector(".highlight");
    if (highlight) {
      $("external-source").scrollTop = highlight.offsetTop - $("external-source").offsetTop - 80;
      highlight.scrollIntoView?.({block:"start", inline:"nearest"});
    }
  }
}
$("external-filter")?.addEventListener("input", renderExternalDefinitions);

$("external-previous")?.addEventListener("click", () => { if (!$("external-previous").disabled) renderExternalSource(externalSelectedRange, externalWindowStart - 600); });
$("external-next")?.addEventListener("click", () => { if (!$("external-next").disabled) renderExternalSource(externalSelectedRange, externalWindowStart + 600); });

// Metadata-only automatic library catalog. No source or provider request is implicit.
let dependencyStatusSerial = 0, dependencySerial = 0, dependencySymbolsSerial = 0;
let dependencyCatalog = null, dependencyPackage = null, dependencyOffset = 0, dependencyNext = null;
let dependencyQuery = "", dependencyChoice = null;
function clearDependencySelection() {
  dependencySymbolsSerial++; dependencyPackage = null; dependencyOffset = 0; dependencyNext = null;
  dependencyQuery = "";
  $("dependency-symbols").replaceChildren();
  $("dependency-symbol-state").textContent = "Select a package. No source file knowledge needed.";
  $("dependency-filter").value = ""; $("dependency-filter").disabled = true;
  $("dependency-search").disabled = true;
  $("dependency-previous").disabled = true; $("dependency-next").disabled = true;
  if (externalCatalogId !== null) {
    externalFileSerial++; externalCatalogId = null; externalSnapshot = null; resetExternalWindow();
    $("external-source").replaceChildren(); $("external-definitions").replaceChildren(); $("external-warnings").replaceChildren();
    $("external-source-path").textContent = "Choose an indexed definition to read a candidate snapshot.";
    $("external-filter").disabled = true; $("external-filter").value = "";
  }
}
function clearDependencyCatalog() {
  dependencyStatusSerial++; dependencySerial++; dependencyCatalog = null; dependencyChoice = null;
  clearDependencySelection();
  $("dependency-packages").replaceChildren(); $("dependency-warnings").replaceChildren();
  $("dependency-state").textContent = "Open a library from an external call or reference.";
}
function dependencyGuard() {
  const session = epoch, serial = dependencySerial, revision = status?.revision && IndexPin.copy(status.revision), workspace = status?.workspaceRoot;
  return () => session === epoch && serial === dependencySerial && IndexPin.equal(revision, status?.revision) && workspace === status?.workspaceRoot;
}
async function refreshDependencies() {
  if (!token || !status) return;
  const serial = ++dependencyStatusSerial, guard = dependencyGuard();
  const current = () => guard() && serial === dependencyStatusSerial;
  try {
    const data = await api("/api/dependencies");
    if (!current()) return;
    if (!IndexPin.equal(data.workspaceRevision, status.revision)) { unexpectedPair(undefined, data.workspaceRevision); throw new Error("Library catalog belongs to another workspace revision. Refresh workspace status."); }
    if (!["disabled", "loading", "ready", "failed"].includes(data.state)) throw new Error("Library catalog status unavailable.");
    const catalogChanged = dependencyCatalog?.catalogId !== data.catalogId;
    if (catalogChanged || data.state !== "ready") {
      dependencySerial++; clearDependencySelection();
    }
    if (catalogChanged && dependencyChoice) { dependencyChoice = null; $("dependency-state").textContent = "Library catalog changed. Open the library again from its call or reference."; }
    dependencyCatalog = data;
    $("dependency-warnings").replaceChildren();
    for (const warning of data.warnings || []) $("dependency-warnings").append(element("li", warning, "warning"));
    renderDependencyPackages();
  } catch (error) {
    if ((!current() && !/Library catalog belongs/.test(error.message)) || error.name === "AbortError") return;
    const wasOpen = !!dependencyChoice;
    clearDependencyCatalog();
    if (wasOpen) $("dependency-state").textContent = `Library catalog unavailable. ${error.message} Refresh status (⋯ → Refresh) to retry.`;
  }
}
function renderDependencyPackages() {
  $("dependency-packages").replaceChildren();
  // Only the packages an in-context reference matched; several versions need a choice.
  for (const pkg of dependencyChoice || []) {
    const li = element("li"), pick = externalButton(`${pkg.name} ${pkg.version}`, () => selectDependencyPackage(pkg));
    pick.disabled = dependencyCatalog.state !== "ready" || !dependencyCatalog.catalogId;
    pick.setAttribute("aria-pressed", String(dependencyPackage?.id === pkg.id));
    if (dependencyPackage?.id === pkg.id) pick.className = "selected";
    pick.append(element("span", `${pkg.ecosystem} · ${pkg.source} · source: ${pkg.sourceState} · index: ${pkg.indexState}`, "detail"));
    li.append(pick);
    // Package notes stay folded so the definitions remain in view.
    if (pkg.aliases?.length || pkg.warnings?.length) {
      const notes = element("details");
      notes.append(element("summary", `Package notes (${(pkg.warnings || []).length})`));
      if (pkg.aliases?.length) notes.append(element("p", `Aliases: ${pkg.aliases.join(", ")}`, "detail"));
      for (const warning of pkg.warnings || []) notes.append(element("p", warning, "detail"));
      li.append(notes);
    }
    $("dependency-packages").append(li);
  }
}
async function selectDependencyPackage(pkg) {
  if (dependencyCatalog?.state !== "ready" || !dependencyCatalog.packages.some(item => item.id === pkg.id)) return;
  clearDependencySelection(); dependencyPackage = pkg;
  $("dependency-filter").disabled = false; $("dependency-search").disabled = false;
  renderDependencyPackages();
  await loadDependencySymbols(0);
}
async function loadDependencySymbols(offset = 0, query = dependencyQuery) {
  if (!dependencyPackage || !dependencyCatalog?.catalogId) return;
  const catalogId = dependencyCatalog.catalogId, pkg = dependencyPackage, guard = dependencyGuard(), serial = ++dependencySymbolsSerial;
  const current = () => guard() && serial === dependencySymbolsSerial && catalogId === dependencyCatalog?.catalogId && pkg === dependencyPackage;
  $("dependency-symbols").replaceChildren();
  $("dependency-symbol-state").textContent = `Loading indexed definitions in ${pkg.name}…`;
  $("dependency-previous").disabled = true; $("dependency-next").disabled = true;
  try {
    const data = await api(`/api/dependencies/symbols?catalogId=${encodeURIComponent(catalogId)}&packageId=${encodeURIComponent(pkg.id)}&q=${encodeURIComponent(query)}&offset=${offset}&limit=100`);
    if (!current()) return;
    if (!IndexPin.equal(data.workspaceRevision, status.revision)) { unexpectedPair(undefined, data.workspaceRevision); throw new Error("Library definition revision changed. Refresh workspace status."); }
    if (data.catalogId !== catalogId || data.items.some(item => item.packageId !== pkg.id)) throw new Error("Library definition provenance mismatch. Refresh status (⋯ → Refresh), then open the library again.");
    dependencyOffset = offset; dependencyNext = data.nextOffset; dependencyQuery = query;
    for (const symbol of data.items) {
      const li = element("li"), pick = externalButton(`${symbol.qualifiedName || symbol.name} · ${symbol.kind}`, () => {
        if (current()) return loadDependencySource(symbol);
      });
      pick.append(element("span", `${symbol.signature || symbol.name} · ${symbol.path}:${symbol.range.startLine}`, "detail"));
      if (symbol.ownerExpression) pick.append(element("span", `Lexical owner: ${symbol.ownerExpression} (not resolved)`, "detail"));
      li.append(pick); $("dependency-symbols").append(li);
    }
    $("dependency-symbol-state").textContent = data.items.length ? `${pkg.name} · definitions ${offset + 1}–${offset + data.items.length}${data.nextOffset != null ? " · more available" : ""} · syntax candidates` : `No indexed definitions match in ${pkg.name}. Source: ${pkg.sourceState}; index: ${pkg.indexState}.`;
    $("dependency-previous").disabled = offset === 0;
    $("dependency-next").disabled = data.nextOffset == null;
  } catch (error) {
    if ((!current() && !/Library definition revision/.test(error.message)) || error.name === "AbortError") return;
    $("dependency-symbol-state").textContent = `${error.message} Use Filter to retry, or refresh status (⋯ → Refresh) and open the library again if the catalog changed.`;
  }
}
async function loadDependencySource(symbol) {
  if (!dependencyCatalog?.catalogId || dependencyPackage?.id !== symbol.packageId) return;
  const catalogId = dependencyCatalog.catalogId, guard = dependencyGuard(), externalCurrent = externalGuard(), serial = ++externalFileSerial;
  const current = () => guard() && externalCurrent() && serial === externalFileSerial && catalogId === dependencyCatalog?.catalogId;
  externalCatalogId = catalogId; externalSnapshot = null; resetExternalWindow();
  $("external-source").replaceChildren(); $("external-definitions").replaceChildren(); $("external-warnings").replaceChildren();
  $("external-filter").disabled = true;
  $("external-source-path").textContent = `Loading candidate ${symbol.qualifiedName || symbol.name}…`;
  $("dependency-symbol-state").textContent = `Loading candidate ${symbol.qualifiedName || symbol.name}…`;
  try {
    const data = await api(`/api/dependencies/source?catalogId=${encodeURIComponent(catalogId)}&sourceRef=${encodeURIComponent(symbol.sourceRef)}`);
    if (!current()) return;
    if (data.id !== symbol.sourceRef || data.rootId !== symbol.packageId || data.path !== symbol.path || !data.hash || !data.definitions.some(item => item.id === symbol.id)) throw new Error("Candidate source provenance mismatch. Nothing displayed.");
    externalSnapshot = data;
    $("external-source-path").textContent = `${data.rootLabel} · ${data.path} · hash ${data.hash} · immutable candidate snapshot · terminal library boundary`;
    for (const warning of data.warnings || []) $("external-warnings").append(element("li", warning, "warning"));
    $("external-filter").disabled = false; $("external-filter").value = "";
    $("dependency-symbol-state").textContent = `Candidate snapshot opened: ${symbol.qualifiedName || symbol.name}. Not workspace evidence.`;
    window.TrellisShell?.showSource("library");
    renderExternalDefinitions(); renderExternalSource(symbol.range);
  } catch (error) {
    if (current() && error.name !== "AbortError") {
      const message = `${error.message} Refresh status (⋯ → Refresh), then open the library again from its call or reference.`;
      $("external-source-path").textContent = message; $("dependency-symbol-state").textContent = message;
    }
  }
}
// In-context library entry (#450). A reference or external call names a package; the
// catalog offers it only when that exact name (or Cargo alias) is indexed and ready.
const libraryEcosystems = {rust:"cargo", cargo:"cargo"};
function libraryPackages(reference) {
  const ecosystem = libraryEcosystems[reference?.ecosystem], name = reference?.name;
  if (!ecosystem || typeof name !== "string" || !name || dependencyCatalog?.state !== "ready" || !dependencyCatalog.catalogId) return [];
  const crate = value => value.replaceAll("-", "_");
  const matches = dependencyCatalog.packages.filter(pkg => pkg.ecosystem === ecosystem && ["complete", "partial"].includes(pkg.indexState) &&
    (pkg.name === name || crate(pkg.name) === crate(name) || (pkg.aliases || []).includes(crate(name))));
  const exact = reference.version ? matches.filter(pkg => pkg.version === reference.version) : [];
  return exact.length ? exact : matches;
}
function libraryAction(reference) {
  const packages = libraryPackages(reference);
  if (!packages.length) return null;
  const catalogId = dependencyCatalog.catalogId, guard = dependencyGuard();
  return () => {
    if (!guard() || dependencyCatalog?.catalogId !== catalogId) return;
    return perform(() => openLibrary(packages, reference));
  };
}
async function openLibrary(packages, reference) {
  dependencyChoice = packages;
  clearDependencySelection();
  $("dependency-state").textContent = packages.length === 1
    ? `${packages[0].name} ${packages[0].version} · source ${packages[0].sourceState} · index ${packages[0].indexState} · from ${reference.origin}`
    : `${packages.length} indexed packages match ${reference.name} · from ${reference.origin}. Choose a version.`;
  $("dependency-symbol-state").textContent = packages.length === 1 ? "" : "Select a package version.";
  renderDependencyPackages();
  window.TrellisShell?.showSource("library");
  if (packages.length === 1) await selectDependencyPackage(packages[0]);
}
function libraryButton(reference) {
  const open = libraryAction(reference);
  if (!open) return null;
  const node = element("button", "Open in library"); node.type = "button";
  node.title = `Indexed declarations of ${reference.name}: syntax candidates, not a resolved target.`;
  node.addEventListener("click", open);
  return node;
}
$("dependency-search-form")?.addEventListener("submit", event => { event.preventDefault(); return loadDependencySymbols(0, $("dependency-filter").value.trim()); });
$("dependency-previous")?.addEventListener("click", () => { if (!$("dependency-previous").disabled) return loadDependencySymbols(Math.max(0, dependencyOffset - 100)); });
$("dependency-next")?.addEventListener("click", () => { if (!$("dependency-next").disabled && dependencyNext != null) return loadDependencySymbols(dependencyNext); });


// Classes is shown only when the checkout's application layers have production code in a
// language the class catalog extracts (#450, #524): the workbench metrics projection's
// stored roles (which leave out fixtures and authored hidden layers such as tooling)
// joined with the catalog's supportedLanguages, read once per pin after that code is known.
// Unknown (loading or failed) hides the tab; explicit class menus still work.
let classSupport = null;
async function refreshClassSupport(pin) {
  const guard = browseGuard(), attempt = {pin, languages:null};
  classSupport = attempt;
  try {
    const query = new URLSearchParams({q:"", limit:"1", indexGeneration:pin.indexGeneration, indexRevision:String(pin.indexRevision)});
    const data = await api(`/api/classes?${query}`);
    if (classSupport !== attempt) return;
    // A superseded read is forgotten so the next projection asks again; a real failure
    // keeps the null record for this pin, which bounds requests.
    if (!guard()) { classSupport = null; return; }
    if (!IndexPin.equal(data?.revision, pin) || !Array.isArray(data.supportedLanguages)) return;
    attempt.languages = new Set(data.supportedLanguages.filter(language => typeof language === "string"));
  } catch (error) {
    if (error?.name === "AbortError" && classSupport === attempt) classSupport = null;
    return;
  }
  updateViewAvailability();
}
function updateViewAvailability() {
  const languages = viewerMetrics?.productionLanguages?.(), pin = status?.revision;
  const known = classSupport && IndexPin.equal(classSupport.pin, pin);
  if (languages?.size && pin && token && !known) void refreshClassSupport(IndexPin.copy(pin));
  const supported = known ? classSupport.languages : null;
  window.TrellisShell?.setViewAvailable?.("classes", !!languages && !!supported && [...supported].some(language => languages.has(language)));
}
// With no method selected, Sequence offers the highest-cognitive named callables of the
// current metrics projection as starting points (#450): application layers only, since
// that population leaves out fixtures and hidden tooling (#524). Repository text stays text.
const SEQUENCE_STARTS = 5;
function renderSequenceStart() {
  const top = !selectedMethod && token && status ? viewerMetrics?.topCallables?.() : null;
  const rows = (top?.rows || []).filter(row => typeof row.name === "string" && row.name && typeof row.sid === "string").slice(0, SEQUENCE_STARTS);
  $("sequence-start-list")?.replaceChildren();
  if ($("sequence-start")) $("sequence-start").hidden = !rows.length;
  if (!rows.length) return;
  $("sequence-start-note").textContent = `Highest cognitive complexity · ${top.population}. Or select a method in the tree, the source margin or the inspector.`;
  for (const row of rows) {
    const item = element("li"), pick = element("button", row.name); pick.type = "button";
    pick.append(element("span", `${row.path} · ${row.kind} · cognitive ${row.metrics?.cognitive ?? "—"}`, "detail"));
    pick.addEventListener("click", () => perform(() => openCallableSequence(row, top.pin)));
    item.append(pick); $("sequence-start-list").append(item);
  }
}
// A metrics row names an exact native sid at a pin; the sequence API admits it or fails.
async function openCallableSequence(row, pin) {
  if (!token || !IndexPin.equal(pin, status?.revision)) throw new Error("Metrics snapshot changed. Refresh status.");
  const context = navigationContext(), ticket = ++navigationSerial;
  const current = () => ticket === navigationSerial && navigationCurrent(context);
  $("sequence-state").textContent = `Loading ${row.name}…`;
  const data = await api("/api/sequence", "POST", {seed:row.sid, expectedRevision:pin, showAll:false}, current);
  if (!current()) return;
  if (!IndexPin.equal(data?.revision, pin) || data?.seed?.id !== row.sid) throw new Error("Sequence provenance mismatch. Nothing displayed.");
  // The shared navigation owner records history and returns to Diagrams.
  await navigateDiagram(data.seed, pin, "sequence", data);
}

// Class diagrams are an independent, cached-source projection. They never infer
// a sequence participant's type or call a provider.
function classLanguage(path) { return /\.(?:java|py|js|mjs|cjs|rs)$/i.test(path || ""); }
async function openClasses(options = {}) {
  architecture?.clearInspection();
  treemap?.clearInspection();
  if (!token || !status || !window.TrellisClasses) return;
  viewerMetrics?.clearInspection();
  clearSource(); window.TrellisShell?.resetInspector();
  showNavigationView("classes");
  return window.TrellisClasses.open(options.seed ? {seed:options.seed} : options);
}
function attachClassMenu(node, options) {
  if (!classLanguage(options.path) || !window.TrellisClasses?.showContextMenu) return;
  const current = browseGuard();
  const show = event => {
    if (!current() || !token || !status) return;
    event.preventDefault();
    window.TrellisClasses.showContextMenu(event, [{
      label: options.seed ? "Show enclosing class diagram" : "Show class diagram",
      run: () => { if (current()) return perform(() => openClasses(options)); },
    }]);
  };
  node.addEventListener("contextmenu", show);
  node.addEventListener("keydown", event => {
    if (event.key === "ContextMenu" || (event.key === "F10" && event.shiftKey)) show(event);
  });
}
function inspectSourceSymbol(symbol, revision) {
  if (!sourceWitness(symbol) || !IndexPin.equal(revision, status?.revision)) throw new Error("Source inspection is stale. Refresh the indexed view.");
  const context = navigationContext(), serial = sourceSerial;
  const inspector = {symbol}; sourceInspector = inspector;
  const current = () => navigationCurrent(context) && sourceInspector === inspector && sourceSerial === serial;
  return window.TrellisShell?.inspectSymbol?.(symbol, revision, () => {
    if (current()) return perform(() => section === "files" ? openFilesLocation(symbol, revision) : showSource(symbol, revision));
  });
}
window.TrellisNavigation?.init({
  request: (path, options = {}) => api(path, options.method || "GET", options.body),
  onStale: unexpectedPair,
  currentRevision: () => status?.revision,
  currentSession: () => `${epoch}:${status?.workspaceRoot || ""}`,
  openClass: (symbol, revision) => perform(() => navigateDiagram(symbol, revision, "class")),
  selectMethod: (symbol, revision) => perform(() => navigateDiagram(symbol, revision)),
  inspectSymbol: (symbol, revision) => perform(() => inspectSourceSymbol(symbol, revision)),
  openSource: (symbol, revision) => perform(() => section === "files" ? openFilesLocation(symbol, revision) : showSource(symbol, revision)),
  showMenu: (event, actions, options) => window.TrellisClasses?.showContextMenu(event, actions, options),
});
function initClassView() {
  window.TrellisClasses?.init({
    request: (path, options = {}) => api(path, options.method || "GET", options.body),
    onStale: unexpectedPair,
    currentRevision: () => status?.revision,
    currentSession: () => `${epoch}:${status?.workspaceRoot || ""}`,
    onChange: () => { viewerMetrics?.clearInspection(); clearSource(); window.TrellisShell?.resetInspector(); },
    readSource: (item, revision) => perform(() => showSource(item, revision)),
    selectMethod: symbol => perform(() => selectMethod(symbol)),
    navigateMember: (event, selector, options) => window.TrellisNavigation?.open(event, selector, options),
  });
}
initClassView();
$("method-class")?.addEventListener("click", () => {
  if (selectedMethod && classLanguage(selectedMethod.path)) return perform(() => openClasses({seed:selectedMethod.id}));
});
