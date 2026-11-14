"use strict";
// Git and index status, the daemon token, the checkout list and selection, and index job polling. Declarations only; app.js wires them.
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
  observeViewRequest(data.viewRequest);
  scheduleFollow();
  updateFilesBasis();
  const listed = checkouts.find(row => row.rootKey === selectedRootKey);
  if (listed) Object.assign(listed, {revision:nextPin, catchingUp:data.catchingUp, indexError:nextPin ? null : "index_not_ready"});
  window.TrellisShell?.updateWorkspace(nextPin ? status : null);
  $("catching-up").hidden = data.catchingUp !== true || !nextPin;
  updateCheckoutOptions();
  if (browseChanged && nextPin) {
    updateViewAvailability();
    void viewerMetrics?.refresh();
    void refreshAppInventory();
    if (section === "diagrams" && (!$("architecture-panel").hidden || !$("deviations-panel").hidden)) void architecture?.refresh();
    if (section === "diagrams" && !$("treemap-panel").hidden) void treemap?.refresh();
    await loadTreeRoot();
  }
  if (serial !== statusSerial || session !== epoch || status !== data) return;
  const stats = status.stats || {};
  const semantic = (status.semanticLanguages || []).map(row => {
    const documents = row.documents || [];
    const pending = documents.filter(item => item.projection?.semanticStatus === "catchingUp").length;
    const reimport = documents.filter(item => item.projection?.semanticStatus === "needsReimport").length;
    const older = documents.filter(item => item.projection?.semanticStatus === "olderBasis").length;
    const availability = [pending && `${pending} catching up`, reimport && `${reimport} need re-import`, older && `${older} older basis`].filter(Boolean).join(", ");
    const evidence = `${row.language}: semantic evidence ${availability || row.semanticState}`;
    return row.reimport?.state === "needsReimport"
      ? `${evidence}; needs re-import — ${row.reimport.reason}. Re-import a current artifact.` : evidence;
  }).join(" · ") || `Semantic evidence ${stats.semanticState || "unavailable"}`;
  $("status").textContent = `${status.workspaceRoot} · ${status.revision ? `Revision ${IndexPin.label(status.revision)}` : "Index not ready"} · ${stats.files ?? 0} files · ${stats.symbols ?? 0} symbols · ${stats.calls ?? 0} calls · ${semantic} · Indexed: ${status.indexedAt ? new Date(Number(status.indexedAt)).toLocaleString() : "not yet"}`;
  $("diagnostics").textContent = describe(status.diagnostics || []);
  if (job && !activeJob(job)) showJob();
  staleMessage = ""; $("stale").hidden = true;
  if (!nextPin) stale("Index not ready. Retry status after this checkout finishes indexing.");
  if (changedCount(stats.changedFiles)) stale(`${changedCount(stats.changedFiles)} inputs differ from the imported SCIP manifest. Syntax remains source-only; imported SCIP is display metadata, never semantic proof.`);
  renderIndexStatus();
  // The library catalog loads on demand (#527). A status refresh forgets a loaded catalog, so the
  // next in-context open reads it again, and revalidates a library that is open. A rejected
  // catalog must not recursively request status while a pair refresh is pending.
  // A read still in flight is already current; it is kept so waiting entries are filled.
  if (nextPin && !pairRefreshPending) {
    if (!dependencyLoad || dependencyLoad.settled) { dependencyLoad = null; if (dependencyCatalog) dependencyCatalog.stale = true; }
    if (dependencyChoice) void ensureDependencies();
  }
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
