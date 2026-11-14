"use strict";
// Git and index status of the selected checkout, and index job polling. Declarations only; app.js wires them.
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
