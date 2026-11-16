"use strict";
// The composition root. Every statement the app runs at load is here, in the order it always ran: listeners, observers,
// view construction and shell callbacks. The declarations they use are in the app-*.js scripts, which load first.
// The token, connect and checkout code is here too, where it was, until #312 lands; it then moves to app-checkout.js.
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
  clearTimeout(followTimer); followTimer = null; followSeen = null; hideAgentNote(); clearSequenceOpen(); clearRecovery();
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
  const projections = (status.semanticLanguages || []).flatMap(row => row.documents || []).map(row => row.projection);
  if (projections.some(projection => projection?.semanticStatus === "catchingUp")) {
    return {state:"catching-up", text:`Semantic catching up · native rev ${IndexPin.label(pin)}`};
  }
  if (projections.some(projection => projection?.semanticStatus === "needsReimport")) {
    return {state:"stale", text:`Semantic needs re-import · native rev ${IndexPin.label(pin)}`};
  }
  if (projections.some(projection => projection?.semanticStatus === "olderBasis")) {
    return {state:"stale", text:`Older semantic basis · native rev ${IndexPin.label(pin)}`};
  }
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
