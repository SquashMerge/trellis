"use strict";
const $ = id => document.getElementById(id);
let token = "", epoch = 0, selectedRootKey = null, selectedWorkspaceRoot = null, selectedGit = null, gitSerial = 0, checkoutBusy = false, checkouts = [], sourceSerial = 0;
const pendingRequests = new Set();
let status = null;
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
  const simple = new Set(["status", "git", "index", "tree", "files", "methods", "sequence", "classes", "class-diagram", "navigation", "source", "jobs/current", "dependencies", "dependencies/refresh", "dependencies/symbols", "dependencies/source", "rust-sources", "rust-sources/tree", "rust-sources/file"]);
  if (route === "git" && method !== "GET") throw new Error("Git API requests must be GET.");
  const item = /^jobs\/[^/?]+(?:\/cancel)?$/.test(route);
  if (!simple.has(route) && !item || /[#\s]/.test(suffix) || suffix.includes("//")) throw new Error("Unknown checkout API route.");
  if (!root || !uniqueSelectableRoot(root)) throw new Error("Choose an available checkout first.");
  return `/api/checkouts/${encodeURIComponent(root)}/${suffix}`;
}
async function api(path, method = "GET", body) {
  const session = epoch, root = selectedRootKey, sourceAtStart = sourceSerial;
  const url = selectedUrl(path, method, root);
  const guarded = path.startsWith("/api/source?");
  const operationCurrent = operationGuard();
  const current = () => session === epoch && root === selectedRootKey && (!guarded || (operationCurrent() && sourceAtStart === sourceSerial));
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
    error.workspaceRoot = response.headers?.get?.("X-Trellis-Workspace") || null;
    if (error.status === 401 || error.status === 403) disconnect(`Authentication failed (${error.code || error.status}). Connect with a daemon token again.`);
    else if (root && error.code === "workspace_selection_failed") invalidateSelection("Checkout identity changed. Choose an available checkout again.");
    throw error;
  }
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
function stale(message) { $("stale").textContent = message; $("stale").hidden = false; }
function clearSource() {
  sourceSerial++; window.TrellisNavigation?.reset(); $("source").replaceChildren();
  $("source-path").textContent = "Choose a call to read its cached source snapshot.";
  if (!$("source-dock")?.hidden) window.TrellisShell?.closeSource?.();
}
function changedCount(value) { return Array.isArray(value) ? value.length : Number(value || 0); }
function showSelectedGit(git) {
  selectedGit = git || null;
  window.TrellisShell?.updateCheckout(selectedWorkspaceRoot, selectedGit);
  if (!status) window.TrellisShell?.updateWorkspace(null);
}
async function refreshGit() {
  const serial = ++gitSerial, session = epoch, root = selectedRootKey;
  try {
    const data = await api("/api/git");
    if (serial === gitSerial && session === epoch && root === selectedRootKey) showSelectedGit(data?.git);
  } catch (error) {
    if (serial === gitSerial && session === epoch && root === selectedRootKey && error.name !== "AbortError") showSelectedGit(null);
  }
}
async function refreshStatus(followup = false) {
  statusRefreshDepth++;
  try {
  const serial = ++statusSerial, session = epoch;
  let data;
  try { data = await api("/api/status"); }
  catch (error) {
    if (error.code === "index_not_ready" && serial === statusSerial && session === epoch && selectedRootKey) {
      const root = selectedRootKey, workspaceRoot = error.workspaceRoot || status?.workspaceRoot;
      const listedRoot = selectedWorkspaceRoot, git = selectedGit;
      resetCheckout();
      selectedRootKey = root; selectedWorkspaceRoot = listedRoot;
      showSelectedGit(git);
      $("checkout-select").value = root;
      $("checkout-root").textContent = workspaceRoot || "Selected checkout · verified root unavailable until status responds";
      $("workspace").hidden = false; $("refresh").disabled = false; $("index").disabled = false;
      window.TrellisShell?.setConnected(true);
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
  if (status && browseChanged) { clearBrowse("Index changed. Choose a method from the refreshed files."); sourceCache.clear(); clearSource(); }
  if (browseChanged) clearDependencyCatalog();
  status = {...data, revision:nextPin};
  data = status;
  window.TrellisShell?.updateWorkspace(nextPin ? status : null);
  $("catching-up").hidden = data.catchingUp !== true || !nextPin;
  if (browseChanged && nextPin) await loadTreeRoot();
  if (serial !== statusSerial || session !== epoch || status !== data) return;
  const stats = status.stats || {};
  $("status").textContent = `${status.workspaceRoot} · ${status.revision ? `Revision ${IndexPin.label(status.revision)}` : "Index not ready"} · ${stats.files ?? 0} files · ${stats.symbols ?? 0} symbols · ${stats.calls ?? 0} calls · Semantic proof unavailable · SCIP (if present): display only · Indexed: ${status.indexedAt ? new Date(Number(status.indexedAt)).toLocaleString() : "not yet"}`;
  $("diagnostics").textContent = describe(status.diagnostics || []);
  $("stale").hidden = true;
  if (!nextPin) stale("Index not ready. Retry status after this checkout finishes indexing.");
  if (changedCount(stats.changedFiles)) stale(`${changedCount(stats.changedFiles)} inputs differ from the imported SCIP manifest. Syntax remains source-only; imported SCIP is display metadata, never semantic proof.`);
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
  clearBrowse(message); clearSource(); sourceCache.clear(); clearDependencyCatalog();
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
  epoch++; gitSerial++; selectedRootKey = null; selectedWorkspaceRoot = null; selectedGit = null; checkoutBusy = false;
  for (const controller of pendingRequests) controller.abort();
  clearTimeout(pollTimer); pollTimer = null; statusSerial++;
  pairRefreshPending = false; pairRefreshQueued = null;
  clearDependencyCatalog(); clearExternalSources(); clearBrowse(); clearSource(); sourceCache.clear();
  status = null; job = null; waitingForJob = false;
  ["status", "diagnostics", "job"].forEach(id => $(id).textContent = "");
  $("file-filter").value = "";
  $("stale").hidden = true; $("catching-up").hidden = true;
  $("checkout-root").textContent = "No checkout selected";
  $("workspace").hidden = true; $("index").disabled = true; $("cancel").hidden = true;
  window.TrellisShell?.setConnected(false);
}
function disconnect(message) {
  tokenFileSerial++; tokenSaveFailed = false; const forgotten = forgetStoredToken();
  resetCheckout(); checkouts = []; token = "";
  $("remember-token").checked = false; $("token-file").value = ""; $("token").value = "";
  $("checkout-picker").hidden = true; $("checkout-select").replaceChildren(checkoutOption("Choose a checkout", ""));
  $("logout").hidden = true; $("connect-form").hidden = false;
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
    checkouts = next;
    if (selectedRootKey && (!sameRoot || (!busy && !selectable(selected)))) {
      invalidateSelection("Selected checkout is no longer available. Choose again."); return;
    }
    const select = $("checkout-select"), choice = selectedRootKey || "";
    select.replaceChildren(checkoutOption("Choose a checkout", ""));
    for (const row of checkouts) {
      const option = checkoutOption(`${row.workspaceRoot || row.rootKey} · ${row.state} · ${window.TrellisShell.formatGit(row.git)}`, row.rootKey);
      option.disabled = !selectable(row); select.append(option);
    }
    select.value = choice;
    if (choice && !busy && gitAtStart === gitSerial) { gitSerial++; showSelectedGit(selected.git); }
    select.disabled = !checkouts.some(selectable);
    $("checkout-state").textContent = busy ? "Selected checkout is temporarily busy. Retry the checkout list."
      : !checkouts.length ? "No checkouts found. Register a checkout, then retry."
      : !checkouts.some(selectable) ? "No available checkouts. Unavailable or corrupt entries cannot be selected. Retry the list."
      : choice ? "Checkout selected." : "Choose a checkout to inspect. No checkout is selected automatically.";
    $("checkout-retry").hidden = !busy && checkouts.length > 0 && checkouts.every(selectable);
    if (recovering) await selectCheckout(choice);
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
  selectedRootKey = key;
  const selected = checkouts.find(row => row.rootKey === key);
  selectedWorkspaceRoot = selected.workspaceRoot;
  showSelectedGit(selected.git);
  $("error").hidden = true; $("error").textContent = "";
  $("checkout-root").textContent = "Verifying selected checkout…";
  $("checkout-state").textContent = "Loading selected checkout…";
  $("workspace").hidden = false; $("refresh").disabled = false; $("index").disabled = false;
  window.TrellisShell?.setConnected(true);
  const session = epoch;
  try {
    await refreshStatus();
    if (session !== epoch) return;
    job = await api("/api/jobs/current").catch(error => { if (error.name !== "AbortError") throw error; return null; });
    if (session !== epoch) return;
    waitingForJob = !!job && activeJob(job);
    if (job) { showJob(); schedulePoll(); }
    $("checkout-state").textContent = "Checkout ready.";
    $("notice").textContent = "Browse folders below. Indexed files expand to methods."
      + (tokenSaveFailed ? " Browser storage is unavailable; token was not saved." : "");
  } catch (error) {
    if (session !== epoch || error.name === "AbortError") return;
    const cold = error.code === "index_not_ready";
    $("checkout-root").textContent = error.workspaceRoot || status?.workspaceRoot || "Selected checkout · verified root unavailable until status responds";
    $("checkout-state").textContent = cold ? "Index not ready. Retry status after indexing starts." : `Checkout unavailable: ${error.message} Retry status; your selection is unchanged.`;
    $("status").textContent = cold ? "No validated index head is available yet." : "Selected checkout status unavailable.";
    $("refresh").disabled = false;
    $("stale").hidden = false; $("stale").textContent = $("checkout-state").textContent;
  }
}
$("checkout-select").addEventListener("change", event => { void selectCheckout(event.target.value); });
$("checkout-retry").addEventListener("click", async () => {
  const [listed] = await Promise.all([loadCheckouts(), loadDaemonStatus()]);
  if (listed && token && $("remember-token").checked) {
    try { localStorage.setItem(tokenStorageKey, token); tokenSaveFailed = false; }
    catch (_) { tokenSaveFailed = true; $("checkout-state").textContent += " Browser storage is unavailable; token was not saved."; }
  }
});
form("connect-form", async () => {
  const supplied = $("token").value.trim();
  if (!safeTokenText(supplied)) throw new Error("Paste a daemon token or load daemon.token first.");
  tokenFileSerial++; tokenSaveFailed = false; token = supplied; $("token").value = ""; $("token-file").value = "";
  $("error").hidden = true; $("error").textContent = "";
  $("checkout-picker").hidden = false; $("connect-form").hidden = true; $("logout").hidden = false;
  document.body.classList.toggle("daemon-connected", true);
  const [listed] = await Promise.all([loadCheckouts(), loadDaemonStatus()]);
  if (!token) return;
  let remembered = false;
  if (listed && $("remember-token").checked) {
    try { localStorage.setItem(tokenStorageKey, token); remembered = true; }
    catch (_) { tokenSaveFailed = true; }
  } else if (listed) forgetStoredToken();
  $("notice").textContent = listed ? `Connected to the daemon on ${window.location?.host || "this address"}. Choose a checkout before browsing.`
    + (remembered ? " Token saved on this browser." : tokenSaveFailed ? " Browser storage is unavailable; token was not saved." : "")
    : "Checkout list unavailable. Retry the list; no checkout has been selected.";
  $("checkout-select").focus();
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
  IndexPin.copy(revision);
  if (!sourceWitness(item)) throw new Error("Measured source path and range unavailable.");
  if (!status?.revision || !IndexPin.equal(revision, status.revision)) throw new Error("This source has no current paired snapshot. Refresh the view first.");
  const serial = ++sourceSerial; const key = `${IndexPin.key(revision)}:${item.path}`;
  window.TrellisNavigation?.reset();
  $("source-path").textContent = `Loading ${item.path}…`; $("source").replaceChildren();
  const data = sourceCache.get(key) || await api(`/api/source?path=${encodeURIComponent(item.path)}&${IndexPin.query(revision)}`);
  if (serial !== sourceSerial || !IndexPin.equal(status?.revision, revision)) return;
  if (!IndexPin.equal(data.revision, revision) || !IndexPin.equal(status?.revision, revision)) { unexpectedPair("Source snapshot changed. Refresh status.", data.revision); throw new Error("Source snapshot changed. Refresh status."); }
  sourceCache.set(key, data);
  $("source-path").textContent = `${data.file.path} · revision ${IndexPin.label(data.revision)} · cached snapshot`;
  const fragment = document.createDocumentFragment();
  data.file.text.split("\n").forEach((text, index) => {
    const number = index + 1; const line = element("span", undefined, "source-line");
    if (number >= item.range.startLine && number <= item.range.endLine) line.classList.add("highlight");
    line.append(element("span", String(number), "line-number"), document.createTextNode(text)); fragment.append(line);
  });
  $("source").replaceChildren(fragment);
  window.TrellisShell?.showSource("workspace");
  window.TrellisNavigation?.attachSource($("source"), {
    path:data.file.path, revision:data.revision, startLine:item.range.startLine,
    isCurrent: () => serial === sourceSerial && !!token && IndexPin.equal(status?.revision, revision) &&
      !$("source-dock")?.hidden && !$("workspace-source-panel")?.hidden,
  });
  $("source").focus({preventScroll:true});
  $("source").scrollIntoView?.({block:"nearest"});
  const highlight = $("source").querySelector(".highlight");
  if (highlight) {
    $("source").scrollTop = highlight.offsetTop - $("source").offsetTop - 80;
    // The workbench's source panel can own vertical scrolling, not the pre.
    highlight.scrollIntoView?.({block:"start", inline:"nearest"});
  }
}
$("refresh").addEventListener("click", () => perform(async () => { clearSource(); const revision = status?.revision, session = epoch, root = selectedRootKey; await refreshGit(); if (session !== epoch || root !== selectedRootKey) return; await refreshStatus(); if (IndexPin.equal(revision, status?.revision)) await refreshTree(); if (selectedMethod) await loadSequence(); schedulePoll(); }, $("refresh")));
function activeJob(value) { return ["queued", "running"].includes(value.state); }
function showJob() {
  if (!job) { $("job").textContent = ""; $("cancel").hidden = true; $("index").disabled = false; return; }
  $("job").textContent = `Index ${job.state}${job.state === "queued" ? " · accepted" : ""}${activeJob(job) && !waitingForJob ? " · no longer waiting here" : ""} · ${job.progress?.phase || ""} · ${job.progress?.completed ?? 0}/${job.progress?.total ?? "?"}${job.error ? ` · ${describe(job.error)}` : ""}`;
  $("cancel").hidden = !waitingForJob || !activeJob(job);
  $("index").disabled = waitingForJob && activeJob(job);
}
function schedulePoll() {
  clearTimeout(pollTimer);
  if (!waitingForJob || !job || !activeJob(job) || !token) return;
  pollTimer = setTimeout(() => perform(async () => {
    job = await api(`/api/jobs/${encodeURIComponent(job.id)}`); showJob();
    if (activeJob(job)) schedulePoll();
    else { waitingForJob = false; showJob(); await refreshStatus(); }
  }), 700);
}
$("index").addEventListener("click", () => perform(async () => {
  window.TrellisClasses?.reset();
  diagramSerial++; window.TrellisShell?.resetInspector(); clearSource();
  job = await api("/api/index", "POST", {}); waitingForJob = true; showJob(); schedulePoll();
}, $("index")).then(() => { if (job) showJob(); }));
$("cancel").addEventListener("click", () => {
  // Stopping this page's polling never cancels a durably accepted request.
  clearTimeout(pollTimer); waitingForJob = false; showJob();
});

// File browser has independent request generations, including failures.
let browseSerial = 0, diagramSerial = 0, files = [], nextFileOffset = null;
let selectedMethod = null, catalogSerial = 0, treeSerial = 0, directoryRefreshSerial = 0, retryCatalogReset = true;
const fileStates = new Map(), closedDirectories = new Set();
let treeMode = false, treeRoot = null, indexedWorkspace = "";
const directories = new Map();
function clearBrowse(message = "Expand a file and select a method.") {
  window.TrellisClasses?.reset();
  if ($("method-class")) $("method-class").disabled = true;
  window.TrellisShell?.resetInspector();
  if ($("sequence-warning-count")) $("sequence-warning-count").textContent = "0";
  browseSerial++; diagramSerial++; catalogSerial++; directoryRefreshSerial++; files = []; nextFileOffset = null; selectedMethod = null;
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
  const serial = ++catalogSerial, guard = browseGuard();
  const current = () => guard() && serial === catalogSerial;
  const revision = IndexPin.copy(status.revision), offset = reset ? 0 : nextFileOffset;
  if (offset == null) return;
  $("files-state").textContent = "Loading indexed files…"; $("retry-files").hidden = true;
  let loaded = false; retryCatalogReset = reset;
  await browseRequest(async () => {
    const data = await api(`/api/files?${IndexPin.query(revision)}&offset=${offset}&limit=200`);
    if (!current()) return;
    if (!IndexPin.equal(data.revision, revision)) { unexpectedPair(undefined, data.revision); throw new Error("File catalog revision does not match. Refresh status."); }
    const hadFiles = files.length > 0;
    const known = new Set(files.map(f => f.path));
    files.push(...data.items.filter(f => !known.has(f.path)));
    if (!reset || !hadFiles) nextFileOffset = data.nextOffset;
    renderFiles(); loaded = true;
  }, current, "files-state");
  if (current()) $("retry-files").hidden = loaded;
}
function renderFiles() {
  if (treeMode) { renderDirectoryTree(); return; }
  const generation = ++treeSerial;
  const tree = $("file-tree"); tree.replaceChildren();
  const filter = $("file-filter").value.toLowerCase();
  const visible = files.filter(f => f.path.toLowerCase().includes(filter));
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
    const pick = button(`${state?.open ? "▾" : "▸"} ${parts.at(-1)} · ${file.methodCount}`, () => toggleFile(file));
    pick.className = "file-pick"; pick.title = file.path;
    attachClassMenu(pick, {path:file.path});
    pick.setAttribute("aria-expanded", String(!!state?.open)); li.append(pick);
    appendMethods(li, file);
    parent.append(li);
  }
  $("browse-root").textContent = status?.workspaceRoot || "Indexed workspace";
  $("reveal-method").hidden = !selectedMethod;
  if (!visible.length) tree.append(element("li", files.length ? "No loaded paths match. Clear the filter or load more files." : "No indexed files yet. Use Index workspace above, then browse the tree.", "detail"));
  $("files-state").textContent = `${visible.length} of ${files.length} loaded files${nextFileOffset != null ? " · more available" : ""}`;
  $("more-files").hidden = nextFileOffset == null;
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
async function selectMethod(symbol) {
  window.TrellisShell?.showView("sequence");
  if ($("method-class")) $("method-class").disabled = !classLanguage(symbol.path);
  selectedMethod = symbol; $("method-source").disabled = false;
  clearSource(); renderFiles();
  await loadSequence();
  if (selectedMethod === symbol && window.matchMedia?.("(max-width: 850px)").matches) $("sequence-title").scrollIntoView?.({block:"start"});
}
async function loadSequence() {
  window.TrellisShell?.resetInspector();
  if ($("sequence-warning-count")) $("sequence-warning-count").textContent = "0";
  if (!selectedMethod || !status) return;
  const serial = ++diagramSerial, guard = browseGuard(), symbol = selectedMethod, revision = IndexPin.copy(status.revision);
  const current = () => guard() && serial === diagramSerial && selectedMethod === symbol;
  clearSource(); $("sequence-diagram").replaceChildren(); $("sequence-warnings").replaceChildren();
  $("sequence-state").textContent = `Loading ${symbol.name}…`;
  await browseRequest(async () => {
    const data = await api("/api/sequence", "POST", {seed:symbol.id, expectedRevision:revision, showAll:!!$("all-steps").checked});
    if (!current()) return;
    if (!IndexPin.equal(data.revision, revision)) { unexpectedPair(undefined, data.revision); throw new Error("Sequence provenance mismatch. Nothing displayed."); }
    if (data.seed.id !== symbol.id) throw new Error("Sequence provenance mismatch. Nothing displayed.");
    const readSource = step => { if (current()) return perform(() => showSource(step, revision)); };
    const options = {showDetails: !!$("all-steps").checked, isCurrent: () => current() && IndexPin.equal(status?.revision, revision)};
    // A later group redraw can fail after the initial load succeeded. Preserve
    // the whole pinned DTO rather than leaving an inert, silently folded group.
    options.onUnsupported = reason => {
      if (!options.isCurrent()) return;
      window.TrellisSequence.render($("sequence-diagram"), data, readSource, expanded, options);
      $("sequence-state").textContent = $("sequence-state").textContent.replace(/ · Pele rendered$/, ` · Pele fell back: ${reason}`);
    };
    if (window.TrellisShell) options.onSelect = step => {
      if (current()) window.TrellisShell?.selectStep(step, data, () => readSource(step));
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
  }, current, "sequence-state");
}
$("file-filter").addEventListener("input", renderFiles);
$("all-methods").addEventListener("change", renderFiles);
$("all-steps").addEventListener("change", () => loadSequence());
$("more-files").addEventListener("click", () => perform(() => loadFiles(), $("more-files")));

$("method-source").addEventListener("click", () => { if (selectedMethod && status) perform(() => showSource(selectedMethod, status.revision)); });

$("retry-files").addEventListener("click", () => perform(() => loadFiles(retryCatalogReset), $("retry-files")));
$("reveal-method").addEventListener("click", () => {
  if (!selectedMethod) return;
  $("file-filter").value = "";
  if (treeMode) {
    for (const state of directories.values()) {
      const entry = state.items?.find(item => item.indexedPath === selectedMethod.path);
      if (!entry) continue;
      let path = "";
      directories.get("").open = true;
      for (const part of entry.path.split("/").slice(0, -1)) {
        path = path ? path + "/" + part : part;
        if (directories.has(path)) directories.get(path).open = true;
      }
    }
  }
  const parts = selectedMethod.path.split("/"); let directory = "";
  for (const part of parts.slice(0, -1)) { directory += part + "/"; closedDirectories.delete(directory); }
  const state = fileStates.get(selectedMethod.path);
  if (state) state.open = true;
  $("all-methods").checked = true;
  renderFiles();
  const pick = $("file-tree").querySelector(".method-pick.selected");
  pick?.focus({preventScroll:true}); pick?.scrollIntoView?.({block:"nearest"});
});

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
function renderDirectoryTree() {
  treeSerial++;
  const tree = $("file-tree"); tree.replaceChildren();
  const scopeMismatch = treeRoot && indexedWorkspace && treeRoot !== indexedWorkspace;
  $("browse-root").textContent = scopeMismatch
    ? `Browsing: ${treeRoot} · Index workspace: ${indexedWorkspace}. Indexing applies only to the index workspace.`
    : treeRoot || "Working directory";
  $("index").textContent = scopeMismatch
    ? `Index ${indexedWorkspace.split("/").filter(Boolean).at(-1) || indexedWorkspace}`
    : "Index workspace";
  $("browse-scope").textContent = indexedWorkspace
    ? `Methods and diagrams come from the indexed workspace: ${indexedWorkspace}. Other files are metadata only.`
    : "Browse the working directory. Only indexed files have methods and diagrams.";
  $("reveal-method").hidden = !selectedMethod;
  $("retry-files").hidden = true; $("more-files").hidden = true;
  const filter = $("file-filter").value.toLowerCase();
  function matches(item) {
    return !filter || item.path.toLowerCase().includes(filter) ||
      (item.kind === "directory" && directories.get(item.path)?.items?.some(matches));
  }
  let count = 0;
  function rows(path, list, anchorPath = path) {
    const state = directories.get(path);
    if (!state) return;
    for (const item of (state.items || []).filter(matches)) {
      count++;
      const row = element("li");
      if (item.kind === "directory") {
        const compact = compactDirectory(item), child = directories.get(item.path);
        const terminal = directories.get(compact.last.path);
        const open = !!child?.open || (!!filter && !!terminal?.items);
        const label = compact.items.map(entry => entry.name).join("/") + "/";
        const fullPath = compact.last.path + "/";
        const pick = button(`${open ? "▾" : "▸"} ${label}`, () => toggleDirectory(item.path));
        pick.className = "file-pick directory-pick"; pick.title = fullPath;
        pick.setAttribute("aria-label", `${open ? "Collapse" : "Expand"} folder ${fullPath}`);
        pick.setAttribute("aria-expanded", String(open));
        row.append(pick);
        if (open) { const children = element("ul"); rows(compact.last.path, children, item.path); row.append(children); }
      } else if (item.kind === "file" && item.indexedPath != null) {
        const file = {path:item.indexedPath, methodCount:item.methodCount};
        const open = !!fileStates.get(file.path)?.open;
        const pick = button(`${open ? "▾" : "▸"} ${item.name} · ${item.methodCount ?? 0}`, () => toggleFile(file));
        pick.className = "file-pick"; pick.title = item.path;
        attachClassMenu(pick, {path:file.path});
        pick.setAttribute("aria-expanded", String(open)); row.append(pick); appendMethods(row, file);
      } else {
        row.append(element("span", item.name, "unindexed-file"), element("span",
          item.kind === "symlink" ? "Symbolic link · not followed" : item.kind === "file" ? (item.unindexedReason || "Not indexed in current workspace") : "Special entry · metadata only", "detail"));
      }
      list.append(row);
    }
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
  $("files-state").textContent = filter ? `${count} matching loaded entries · unopened folders are not searched` : "Expand folders to browse. Choose an indexed file to see methods.";
  if (filter && !count) tree.append(element("li", "No loaded entries match. Clear the filter to browse folders.", "detail"));
}

// External libraries are candidate definitions, never workspace evidence.
let externalSerial = 0, externalFileSerial = 0, externalSnapshot = null, externalCatalogId = null;
let externalWindowStart = 0, externalSelectedRange = null;
function resetExternalWindow() {
  externalWindowStart = 0; externalSelectedRange = null;
  $("external-previous").disabled = true; $("external-next").disabled = true;
}
function clearExternalSources() {
  externalSerial++; externalFileSerial++; externalSnapshot = null; externalCatalogId = null; resetExternalWindow();
  $("external-sources").open = false;
  $("external-roots").replaceChildren(); $("external-definitions").replaceChildren();
  $("external-source").replaceChildren(); $("external-warnings").replaceChildren();
  $("external-source-path").textContent = "Choose an indexed definition or a manual source file to read a candidate snapshot.";
  $("external-state").textContent = "Not loaded. Load configured roots to browse.";
  $("external-filter").value = ""; $("external-filter").disabled = true;
  $("external-load").disabled = false;
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
async function loadExternalRoots() {
  if (!token) return;
  clearExternalSources(); $("external-sources").open = true;
  const current = externalGuard();
  $("external-load").disabled = true; $("external-state").textContent = "Loading configured roots…";
  try {
    const data = await api("/api/rust-sources");
    if (!current()) return;
    $("external-state").textContent = data.roots.length ? "Candidate sources only. Browse a root; select a file to load its text." : "No external Rust source roots configured. Ask the daemon operator to configure --rust-source-root. Browsing is unavailable.";
    for (const root of data.roots) {
      const li = element("li"), list = element("ul", undefined, "plain external-tree");
      const state = {root, path:"", list, open:false, serial:0, items:[], loaded:false};
      const pick = externalButton(`Browse ${root.label}`, () => toggleExternalDirectory(state));
      state.pick = pick; pick.setAttribute("aria-expanded", "false"); list.hidden = true;
      li.append(element("p", `${root.label} · ${root.path}`, "detail"), pick, list);
      $("external-roots").append(li);
    }
  } catch (error) {
    if (current() && error.name !== "AbortError") $("external-state").textContent = error.message;
  } finally { if (current()) $("external-load").disabled = false; }
}
async function toggleExternalDirectory(state) {
  state.open = !state.open; state.list.hidden = !state.open;
  state.pick.setAttribute("aria-expanded", String(state.open));
  if (!state.open) { state.serial++; return; }
  if (!state.loaded) await loadExternalDirectory(state, 0);
}
async function loadExternalDirectory(state, offset) {
  const guard = externalGuard(), serial = ++state.serial;
  const current = () => guard() && state.serial === serial && state.open;
  state.loaded = false;
  state.list.replaceChildren(element("li", "Loading directory metadata…"));
  try {
    const data = await api(`/api/rust-sources/tree?root=${encodeURIComponent(state.root.id)}&path=${encodeURIComponent(state.path)}&offset=${offset}&limit=200`);
    if (!current()) return;
    state.items = offset ? state.items.concat(data.items) : data.items; state.loaded = true;
    state.list.replaceChildren();
    for (const item of state.items) {
      const li = element("li");
      if (item.kind === "directory") {
        const list = element("ul", undefined, "plain external-tree"); list.hidden = true;
        const child = {root:state.root, path:item.path, list, open:false, serial:0, items:[], loaded:false};
        child.pick = externalButton(item.name + "/", () => toggleExternalDirectory(child));
        child.pick.setAttribute("aria-expanded", "false"); li.append(child.pick, list);
      } else if (item.kind === "file" && item.path.endsWith(".rs")) {
        li.append(externalButton(item.name, () => loadExternalFile(state.root, item.path)));
      } else { li.append(element("span", item.name + " · not a readable Rust source file", "muted")); }
      state.list.append(li);
    }
    if (!state.items.length) state.list.append(element("li", "No entries in this directory."));
    if (data.truncated) state.list.append(element("li", "Directory scan truncated; some entries may be omitted.", "warning"));
    if (data.nextOffset != null) state.list.append(externalButton("Load more entries", () => loadExternalDirectory(state, data.nextOffset)));
  } catch (error) {
    if (!current() || error.name === "AbortError") return;
    state.loaded = false;
    state.list.replaceChildren(element("li", error.message, "warning"), externalButton("Retry directory", () => loadExternalDirectory(state, offset)));
  }
}
async function loadExternalFile(root, path) {
  externalCatalogId = null;
  const guard = externalGuard(), serial = ++externalFileSerial;
  const current = () => guard() && serial === externalFileSerial;
  externalSnapshot = null; resetExternalWindow(); $("external-source").replaceChildren(); $("external-definitions").replaceChildren();
  $("external-warnings").replaceChildren(); $("external-filter").disabled = true;
  $("external-source-path").textContent = `Loading candidate ${root.label} · ${path}…`;
  $("external-state").textContent = `Loading candidate ${root.label} · ${path}…`;
  try {
    const data = await api(`/api/rust-sources/file?root=${encodeURIComponent(root.id)}&path=${encodeURIComponent(path)}`);
    if (!current()) return;
    if (data.rootId !== root.id || data.path !== path) throw new Error("Candidate source provenance mismatch. Nothing displayed.");
    externalSnapshot = data;
    $("external-source-path").textContent = `${data.rootLabel} · ${data.path} · hash ${data.hash} · immutable candidate snapshot`;
    for (const warning of data.warnings || []) $("external-warnings").append(element("li", warning, "warning"));
    $("external-filter").disabled = false; $("external-filter").value = "";
    $("external-state").textContent = `Candidate snapshot opened: ${data.rootLabel} · ${data.path}. Not workspace evidence.`;
    window.TrellisShell?.showSource("library");
    renderExternalDefinitions(); renderExternalSource();
  } catch (error) {
    if (current() && error.name !== "AbortError") {
      $("external-source-path").textContent = error.message;
      $("external-state").textContent = `Candidate source failed: ${error.message}`;
    }
  }
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
$("external-load")?.addEventListener("click", loadExternalRoots);
$("external-filter")?.addEventListener("input", renderExternalDefinitions);

$("external-previous")?.addEventListener("click", () => { if (!$("external-previous").disabled) renderExternalSource(externalSelectedRange, externalWindowStart - 600); });
$("external-next")?.addEventListener("click", () => { if (!$("external-next").disabled) renderExternalSource(externalSelectedRange, externalWindowStart + 600); });

// Metadata-only automatic library catalog. No source or provider request is implicit.
let dependencyStatusSerial = 0, dependencySerial = 0, dependencySymbolsSerial = 0;
let dependencyCatalog = null, dependencyPackage = null, dependencyOffset = 0, dependencyNext = null;
let dependencyQuery = "";
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
  dependencyStatusSerial++; dependencySerial++; dependencyCatalog = null;
  clearDependencySelection();
  $("dependency-packages").replaceChildren(); $("dependency-warnings").replaceChildren();
  $("dependency-state").textContent = "Library status not loaded.";
}
function dependencyGuard() {
  const session = epoch, serial = dependencySerial, revision = status?.revision && IndexPin.copy(status.revision), workspace = status?.workspaceRoot;
  return () => session === epoch && serial === dependencySerial && IndexPin.equal(revision, status?.revision) && workspace === status?.workspaceRoot;
}
async function refreshDependencies() {
  if (!token || !status) return;
  const serial = ++dependencyStatusSerial, guard = dependencyGuard();
  const current = () => guard() && serial === dependencyStatusSerial;
  $("dependency-state").textContent = "Loading library status…";
  try {
    const data = await api("/api/dependencies");
    if (!current()) return;
    if (!IndexPin.equal(data.workspaceRevision, status.revision)) { unexpectedPair(undefined, data.workspaceRevision); throw new Error("Library catalog belongs to another workspace revision. Refresh workspace status."); }
    if (!["disabled", "loading", "ready", "failed"].includes(data.state)) throw new Error("Library catalog status unavailable.");
    const catalogChanged = dependencyCatalog?.catalogId !== data.catalogId;
    if (catalogChanged || data.state !== "ready") {
      dependencySerial++; clearDependencySelection();
    }
    dependencyCatalog = data;
    const detail = data.state === "loading" ? "Indexing local declarations. Use Refresh library status to check again." : data.state === "disabled" ? "Automatic library catalog is disabled. Manual roots remain available below." : data.state === "failed" ? "Library indexing failed. Check warnings; refresh status after the next workspace index." : `${data.packages.length} packages · ${data.symbolCount} indexed declarations. Select a package to browse.`;
    $("dependency-state").textContent = `${data.state} · ${detail}`
      + (data.state === "ready" && catalogChanged && selectedMethod ? " Library catalog updated. Reselect the workspace method to refresh syntax-candidate lanes in its diagram." : "");
    $("dependency-warnings").replaceChildren();
    for (const warning of data.warnings || []) $("dependency-warnings").append(element("li", warning, "warning"));
    renderDependencyPackages();
  } catch (error) {
    if ((!current() && !/Library catalog belongs/.test(error.message)) || error.name === "AbortError") return;
    clearDependencyCatalog();
    $("dependency-state").textContent = `Library catalog unavailable. ${error.message} Use Refresh library status to retry. Manual roots remain available below.`;
  }
}
function renderDependencyPackages() {
  $("dependency-packages").replaceChildren();
  for (const pkg of dependencyCatalog?.packages || []) {
    const li = element("li"), pick = externalButton(`${pkg.name} ${pkg.version}`, () => selectDependencyPackage(pkg));
    pick.disabled = dependencyCatalog.state !== "ready" || !dependencyCatalog.catalogId;
    pick.setAttribute("aria-pressed", String(dependencyPackage?.id === pkg.id));
    if (dependencyPackage?.id === pkg.id) pick.className = "selected";
    pick.append(element("span", `${pkg.ecosystem} · ${pkg.source} · source: ${pkg.sourceState} · index: ${pkg.indexState}`, "detail"));
    li.append(pick);
    if (pkg.aliases?.length) li.append(element("span", `Aliases: ${pkg.aliases.join(", ")}`, "detail"));
    for (const warning of pkg.warnings || []) li.append(element("p", warning, "detail"));
    $("dependency-packages").append(li);
  }
  if (dependencyCatalog?.state === "ready" && !dependencyCatalog.packages.length) $("dependency-packages").append(element("li", "No supported library packages found in this workspace."));
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
    if (data.catalogId !== catalogId || data.items.some(item => item.packageId !== pkg.id)) throw new Error("Library definition provenance mismatch. Refresh library status.");
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
    $("dependency-symbol-state").textContent = `${error.message} Use Filter to retry or Refresh library status if the catalog changed.`;
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
      const message = `${error.message} Refresh library status before selecting the definition again.`;
      $("external-source-path").textContent = message; $("dependency-symbol-state").textContent = message;
    }
  }
}
$("dependency-refresh")?.addEventListener("click", refreshDependencies);
$("dependency-search-form")?.addEventListener("submit", event => { event.preventDefault(); return loadDependencySymbols(0, $("dependency-filter").value.trim()); });
$("dependency-previous")?.addEventListener("click", () => { if (!$("dependency-previous").disabled) return loadDependencySymbols(Math.max(0, dependencyOffset - 100)); });
$("dependency-next")?.addEventListener("click", () => { if (!$("dependency-next").disabled && dependencyNext != null) return loadDependencySymbols(dependencyNext); });


// Class diagrams are an independent, cached-source projection. They never infer
// a sequence participant's type or call a provider.
function classLanguage(path) { return /\.(?:java|py)$/i.test(path || ""); }
async function openClasses(options = {}) {
  if (!token || !status || !window.TrellisClasses) return;
  clearSource(); window.TrellisShell?.resetInspector();
  window.TrellisShell?.showView("classes");
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
window.TrellisNavigation?.init({
  request: (path, options = {}) => api(path, options.method || "GET", options.body),
  onStale: unexpectedPair,
  currentRevision: () => status?.revision,
  currentSession: () => `${epoch}:${status?.workspaceRoot || ""}`,
  openClass: symbol => perform(() => openClasses({seed:symbol.id})),
  selectMethod: symbol => perform(() => selectMethod(symbol)),
  openSource: (symbol, revision) => perform(() => showSource(symbol, revision)),
  showMenu: (event, actions, options) => window.TrellisClasses?.showContextMenu(event, actions, options),
});
function initClassView() {
  window.TrellisClasses?.init({
    request: (path, options = {}) => api(path, options.method || "GET", options.body),
    onStale: unexpectedPair,
    currentRevision: () => status?.revision,
    currentSession: () => `${epoch}:${status?.workspaceRoot || ""}`,
    onChange: () => { clearSource(); window.TrellisShell?.resetInspector(); },
    readSource: (item, revision) => perform(() => showSource(item, revision)),
    selectMethod: symbol => perform(() => selectMethod(symbol)),
    navigateMember: (event, selector, options) => window.TrellisNavigation?.open(event, selector, options),
  });
}
initClassView();
$("method-class")?.addEventListener("click", () => {
  if (selectedMethod && classLanguage(selectedMethod.path)) return perform(() => openClasses({seed:selectedMethod.id}));
});
