"use strict";
// The composition root. Every statement the app runs at load is here, in the order it always ran: listeners, observers,
// view construction and shell callbacks. The declarations they use are in the app-*.js scripts, which load first.
window.addEventListener?.("popstate", () => { void readNavigationLocation(); });
window.addEventListener?.("hashchange", () => { void readNavigationLocation(); });

$("agent-shown-back").addEventListener("click", () => {
  const shown = agentShown;
  hideAgentNote();
  // Only the entry the agent opened goes back; after another navigation, Back is the browser's.
  if (shown?.key && window.history?.state?.trellisNavigation === shown.key) window.history.go(-1);
});
$("agent-shown-reload").addEventListener("click", () => {
  const request = agentShown?.request;
  hideAgentNote();
  if (request) void perform(async () => { await refreshView(); if (IndexPin.equal(request.revision, status?.revision)) await showAgentView(request); }, $("agent-shown-reload"));
});
$("agent-shown-close").addEventListener("click", hideAgentNote);
$("follow-agent").addEventListener("click", () => {
  followPinned = !followPinned;
  try { localStorage.setItem("trellis.followPinned", String(followPinned)); } catch (_) { /* a convenience only */ }
  renderFollowPin();
});
renderFollowPin();

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

$("refresh").addEventListener("click", () => perform(refreshView, $("refresh")));
$("reload-head").addEventListener("click", () => perform(refreshView, $("reload-head")));
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


window.TrellisShell?.setSectionCallback?.(name => { navigationSerial++; diagramSerial++; void setSection(name); });
// The shell names no view (#1034): a section change drops revision-bound navigation through this.
window.TrellisShell?.setSectionResetCallback?.(() => { window.TrellisNavigation?.reset(); });
if (typeof MutationObserver === "function" && $("files-mini-sequence")) {
  // Pele redraws the preview in place when a group is toggled; fit each drawing.
  new MutationObserver(() => fitFilesMini()).observe($("files-mini-sequence"), {childList:true});
}
if (typeof ResizeObserver === "function" && $("files-mini-sequence")?.parentElement) {
  // Only a width change can change the fit; showing the floor changes the height alone.
  let fittedWidth = null;
  new ResizeObserver(entries => {
    const width = Math.round(entries.at(-1)?.contentRect?.width || 0);
    if (width === fittedWidth) return;
    fittedWidth = width; fitFilesMini();
  }).observe($("files-mini-sequence").parentElement);
}
$("files-mini-floor-open")?.addEventListener("click", () => perform(openFilesFull, $("files-mini-floor-open")));
$("files-indexed")?.addEventListener("click", () => setFilesBasis("indexed"));
$("files-working")?.addEventListener("click", () => setFilesBasis("working"));
$("files-open-full")?.addEventListener("click", openFilesFull);
$("files-matches-more")?.addEventListener("click", () => { if (filesMatches?.continuation) return loadFilesMatches(filesMatches.continuation); });
$("files-matches-retry")?.addEventListener("click", () => loadFilesMatches());

$("sequence-open-retry").addEventListener("click", () => {
  const retry = sequenceOpen?.retry;
  if (retry) void perform(retry);
});
$("sequence-open-close").addEventListener("click", clearSequenceOpen);
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
$("sequence-incidental")?.addEventListener("click", () => { $("all-steps").checked = !$("all-steps").checked; void loadSequence(); });
$("sequence-fit")?.addEventListener("click", () => {
  sequenceFitted = $("sequence-diagram").toggleAttribute("data-fit") ? selectedMethod?.id : null;
  renderSequenceChrome();
});
$("sequence-diagram")?.addEventListener("scroll", positionSequenceColumns, {passive:true});
if (typeof MutationObserver === "function" && $("sequence-diagram")) new MutationObserver(renderSequenceChrome).observe($("sequence-diagram"), {childList:true});
if (typeof ResizeObserver === "function" && $("sequence-diagram")) new ResizeObserver(renderSequenceChrome).observe($("sequence-diagram"));
$("more-files").addEventListener("click", () => perform(() => $("file-filter").value.trim() && filterResults?.next != null ? searchFiles(filterResults.next) : loadFiles(), $("more-files")));

$("method-source").addEventListener("click", () => { if (selectedMethod && status) perform(() => showSource(selectedMethod, status.revision)); });

$("retry-files").addEventListener("click", () => perform(() => loadFiles(retryCatalogReset), $("retry-files")));
$("reveal-method").addEventListener("click", () => perform(revealSelectedMethod, $("reveal-method")));

appSelection?.subscribe(() => appScopeChanged());
// The views name no shell (#1034): each is handed the shell's inspector for it here. The shell is
// read at call time, as the views read it before.
const viewerMetrics = window.TrellisMetrics?.create({
  inspector: {show:() => window.TrellisShell?.showMetricInspector(), clear:() => window.TrellisShell?.clearMetricInspector()},
  request: path => revisionRead(path),
  context: () => ({rootKey:selectedRootKey, session:epoch, pin:status?.revision, treeRoot, indexedWorkspace}),
  onConflict: pin => unexpectedPair("Index changed. Refresh status before inspecting metrics.", pin),
  onChange: () => { renderFiles(); updateViewAvailability(); renderSequenceStart(); },
  onOpenCallable: (row, pin) => perform(() => openCallableSequence(row, pin)),
});
const treemap = window.TrellisTreemap?.create({
  inspector: {show:() => window.TrellisShell?.showTreemapInspector(), clear:() => window.TrellisShell?.clearTreemapInspector()},
  request: (path, current) => revisionRead(path, current),
  appFilter: appChoice, showApp: showAppOf,
  context: () => ({rootKey:selectedRootKey, session:epoch, pin:status?.revision, workspaceRoot:status?.workspaceRoot}),
  readSource: (site, pin, current) => perform(() => showCachedSource(site.path, site.range, pin, current)),
  onConflict: () => unexpectedPair("Treemap snapshot changed. Refresh status before reopening Treemap."),
  onInspect: () => { viewerMetrics?.clearInspection(); architecture?.clearInspection(); clearSource(); },
});
const architecture = window.TrellisArchitecture?.create({
  inspector: {show:options => window.TrellisShell?.showArchitectureInspector(options), clear:() => window.TrellisShell?.clearArchitectureInspector()},
  request: architectureRequest,
  appScope: appSelection && {get:() => appSelection.get(selectedRootKey), set:name => appSelection.set(selectedRootKey, name)},
  context: () => ({rootKey:selectedRootKey, session:epoch, pin:status?.revision, rulesEpoch}),
  readSource: (site, pin, current) => perform(() => showCachedSource(site.path, site.range, pin, current)),
  openSequence: (caller, sites, pin, saved, active) => perform(() => openArchitectureSequence(caller, sites, pin, saved, active)),
  onConflict: () => unexpectedPair("Architecture snapshot changed. Refresh status before reopening Architecture."),
  openTreemap: (identity, pin) => {
    if (!IndexPin.equal(pin, status?.revision)) return;
    window.TrellisShell?.showView("treemap");
    void treemap?.openFile(identity, pin);
  },
  libraryButton: identity => identity.standardLibrary ? null : libraryButton({ecosystem:identity.language, name:identity.package, origin:"Architecture reference"}),
  onInspect: () => { viewerMetrics?.clearInspection(); clearSource(); },
  navigate: {deviations:filter => openDeviationsTab(filter), diagram:() => showDiagramTab(), filter:filter => deviationsFilterChanged(filter)},
  // A rules edit can change which layers are application layers (#524).
  onRulesChanged: () => { void viewerMetrics?.refresh(); },
});
window.TrellisShell?.setViewCallback?.((name, how = {}) => {
  const from = shownView, pin = status?.revision;
  // A tab chosen into or out of Deviations is a history entry (#965); the entry being left is written first.
  if (how.user && section === "diagrams" && pin && (name === "deviations") !== (from === "deviations")) writeEntry(viewEntry(pin, from));
  shownView = name;
  if (!routeViewChange && name !== "sequence") {
    navigationSerial++; diagramSerial++;
    if (navigationEntries.get(window.history?.state?.trellisNavigation)?.kind === "class") window.TrellisClasses?.reset();
  }
  architecture?.clearInspection(); treemap?.clearInspection();
  // Reopening the tab redraws the held snapshot unless its basis changed; leaving keeps it (#789).
  if (name === "architecture" && section === "diagrams") void architecture?.reopen();
  // A navigation to Deviations shows its own filter; a chosen tab shows the tree as it was left.
  else if (name === "deviations") {
    if (!routeViewChange && section === "diagrams") void architecture?.showDeviations();
  } else architecture?.suspend();
  if (how.user && section === "diagrams" && pin && name !== from && (name === "deviations" || from === "deviations")) {
    const saved = name === "deviations" ? viewEntry(pin, name) : {entry:{kind:"view", view:name}, hash:""};
    rememberNavigation(saved.entry, false, saved.hash);
  }
  if (name === "treemap" && section === "diagrams") void treemap?.refresh();
  else treemap?.reset();
});
window.TrellisShell?.setInspectorClearCallback?.(() => { sourceInspector = null; viewerMetrics?.clearInspection(); architecture?.clearInspection(); treemap?.clearInspection(); });
$("external-filter")?.addEventListener("input", renderExternalDefinitions);

$("external-previous")?.addEventListener("click", () => { if (!$("external-previous").disabled) renderExternalSource(externalSelectedRange, externalWindowStart - 600); });
$("external-next")?.addEventListener("click", () => { if (!$("external-next").disabled) renderExternalSource(externalSelectedRange, externalWindowStart + 600); });

$("dependency-search-form")?.addEventListener("submit", event => { event.preventDefault(); return loadDependencySymbols(0, $("dependency-filter").value.trim()); });
$("dependency-previous")?.addEventListener("click", () => { if (!$("dependency-previous").disabled) return loadDependencySymbols(Math.max(0, dependencyOffset - 100)); });
$("dependency-next")?.addEventListener("click", () => { if (!$("dependency-next").disabled && dependencyNext != null) return loadDependencySymbols(dependencyNext); });


window.TrellisNavigation?.init({
  request: (path, options = {}) => api(path, options.method || "GET", options.body),
  onStale: unexpectedPair,
  currentRevision: () => status?.revision,
  currentSession: () => `${epoch}:${status?.workspaceRoot || ""}`,
  openClass: (symbol, revision) => perform(() => navigateDiagram(symbol, revision, "class")),
  selectMethod: (symbol, revision) => perform(() => navigateDiagram(symbol, revision)),
  inspectSymbol: (symbol, revision) => perform(() => inspectSourceSymbol(symbol, revision)),
  openSource: (symbol, revision) => perform(() => section === "files" ? openFilesLocation(symbol, revision) : showSource(symbol, revision)),
  showMenu: (event, actions, options) => window.TrellisContextMenu?.showContextMenu(event, actions, options),
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
    appFilter: appChoice,
  });
}
initClassView();
$("method-class")?.addEventListener("click", () => {
  if (selectedMethod && classLanguage(selectedMethod.path)) return perform(() => openClasses({seed:selectedMethod.id}));
});
