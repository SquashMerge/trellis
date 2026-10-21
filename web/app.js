"use strict";
const $ = id => document.getElementById(id);
let token = "", epoch = 0, selectedRootKey = null, selectedWorkspaceRoot = null, checkoutBusy = false, checkouts = [], querySerial = 0, sourceSerial = 0, searchSerial = 0;
const pendingRequests = new Set();
let status = null, result = null, seed = null, views = [], annotations = [], editingView = null, editingNote = null;
let statusSerial = 0, savedSerial = 0, statusRefreshDepth = 0, pairRefreshPending = false, pairRefreshObserved = null;
let pairRefreshQueued = null, pairRefreshFollowup = false;
let job = null, pollTimer = null, waitingForJob = false;
let packet = null, focused = null, questionSerial = 0;
let jevStatus = null, jevStatusSerial = 0, jevRunning = false;
let acpStatus = null, acpStatusSerial = 0, acpRunning = false, answerSerial = 0;
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
const own = (value, key) => Object.prototype.hasOwnProperty.call(value, key);
const objectValue = value => !!value && typeof value === "object" && !Array.isArray(value);
const savedReasonText = Object.freeze({
  missing: "The original declaration is missing from this index.",
  headerMismatch: "The declaration header changed, so the original anchor no longer attaches.",
  groupChanged: "The declaration's sibling group changed, so the original anchor no longer attaches.",
  unprovenContinuity: "Continuity after this edit cannot be proved.",
});
function normalizeAnchorResult(value) {
  if (!objectValue(value) || !["attached", "orphaned"].includes(value.status)) return null;
  if (value.status === "attached") {
    if (typeof value.targetId !== "string" || !value.targetId || value.reason !== "none") return null;
    return {status:"attached", targetId:value.targetId, reason:"none"};
  }
  if (value.targetId !== null || !own(savedReasonText, value.reason)) return null;
  return {status:"orphaned", targetId:null, reason:value.reason};
}
function savedInner(value, kind) {
  const key = kind === "view" ? "view" : "annotation";
  const wrapped = objectValue(value) && objectValue(value[key]);
  const item = wrapped ? value[key] : value;
  if (!objectValue(item) || typeof item.id !== "string") return null;
  if (kind === "view" && (typeof item.title !== "string" || !objectValue(item.query) || typeof item.query.seed !== "string")) return null;
  if (kind === "note" && (typeof item.nodeId !== "string" || typeof item.body !== "string")) return null;
  return {key, wrapped, item};
}
function normalizeSavedState(value, kind) {
  const inner = savedInner(value, kind);
  if (!inner) throw new Error(`Invalid saved ${kind} response.`);
  let pair = null, attachment = {availability:"anchorless", result:null}, invalidMetadata = false;
  const hasMetadata = inner.wrapped && own(value, "indexGeneration") && own(value, "indexRevision") && own(value, "attachment");
  if (hasMetadata) {
    if (value.indexGeneration !== null || value.indexRevision !== null) {
      try { pair = IndexPin.copy(value); } catch (_) { invalidMetadata = true; }
    }
    const candidate = value.attachment;
    if (objectValue(candidate) && candidate.availability === "ready" && pair) {
      const normalized = normalizeAnchorResult(candidate.result);
      if (normalized) attachment = {availability:"ready", result:normalized};
      else invalidMetadata = true;
    } else if (objectValue(candidate) && candidate.availability === "anchorless" && candidate.result === null) {
      attachment = {availability:"anchorless", result:null};
    } else if (objectValue(candidate) && candidate.availability === "indexUnavailable" && candidate.result === null &&
               value.indexGeneration === null && value.indexRevision === null) {
      attachment = {availability:"indexUnavailable", result:null};
    } else invalidMetadata = true;
  }
  if (invalidMetadata) { pair = null; attachment = {availability:"anchorless", result:null}; }
  return {
    [inner.key]:inner.item,
    ...(kind === "view" ? {orphanedIds:inner.wrapped && Array.isArray(value.orphanedIds) ? value.orphanedIds.filter(id => typeof id === "string") : []}
      : {orphaned:inner.wrapped && value.orphaned === true}),
    indexGeneration:pair?.indexGeneration ?? null,
    indexRevision:pair?.indexRevision ?? null,
    attachment,
    savedPair:pair,
    originalId:inner.item.id,
    originalTarget:kind === "view" ? inner.item.query.seed : inner.item.nodeId,
    metadataInvalid:invalidMetadata,
  };
}
function normalizeSavedList(value, kind) {
  if (!Array.isArray(value)) throw new Error(`Invalid saved ${kind} list response.`);
  return value.map(item => normalizeSavedState(item, kind));
}
function currentNativePin() {
  if (!status || typeof status.evidenceFormat !== "string" || !status.evidenceFormat) return null;
  try { return IndexPin.copy(status.revision); } catch (_) { return null; }
}
function savedTarget(state) { return state.originalTarget; }
function savedLoadDecision(state, kind) {
  const target = savedTarget(state), attachment = state.attachment;
  if (state.metadataInvalid) return {enabled:false, reason:"Saved attachment metadata is invalid. Refresh before loading."};
  if (attachment.availability === "indexUnavailable") return {enabled:false, reason:"Indexed anchor evidence is unavailable."};
  if (attachment.availability === "anchorless") return {enabled:false, reason:"This legacy item has no durable anchor."};
  const anchored = attachment.result;
  if (anchored.status === "orphaned") return {enabled:false, reason:savedReasonText[anchored.reason]};
  if (anchored.targetId !== target) return {enabled:false, reason:"The saved attachment does not match its original target."};
  if (!state.savedPair || !IndexPin.equal(state.savedPair, status?.revision)) return {enabled:false, reason:"Saved evidence is stale. Refresh saved items at the current index."};
  return {enabled:true, reason:"Attached to the original declaration.", pin:IndexPin.copy(state.savedPair), target};
}
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
  const session = epoch, root = selectedRootKey, queryAtStart = querySerial, questionAtStart = questionSerial;
  const selectedSeed = seed, revision = status?.revision && IndexPin.copy(status.revision);
  return () => session === epoch && root === selectedRootKey && queryAtStart === querySerial && questionAtStart === questionSerial && selectedSeed === seed && IndexPin.equal(revision, status?.revision);
}
function selectedUrl(path, method, root) {
  if (path === "/api/checkouts" || path === "/api/daemon/status") {
    if (method !== "GET") throw new Error("Global API requests must be GET.");
    return path;
  }
  const suffix = path.startsWith("/api/") ? path.slice(5) : "";
  const route = suffix.split("?", 1)[0];
  const simple = new Set(["status", "index", "tree", "files", "methods", "sequence", "classes", "class-diagram", "navigation", "symbols", "symbol", "source", "query", "views", "annotations", "jobs/current", "jev/status", "acp/status", "questions/preview", "dependencies", "dependencies/refresh", "dependencies/symbols", "dependencies/source", "rust-sources", "rust-sources/tree", "rust-sources/file"]);
  const item = /^jobs\/[^/?]+(?:\/cancel)?$/.test(route) || /^(?:views|annotations)\/[^/?]+$/.test(route) || /^questions\/[^/?]+\/(?:jev-request|jev-response|jev-run|acp-answer|selection)$/.test(route);
  if (!simple.has(route) && !item || /[#\s]/.test(suffix) || suffix.includes("//")) throw new Error("Unknown checkout API route.");
  if (!root || !uniqueSelectableRoot(root)) throw new Error("Choose an available checkout first.");
  return `/api/checkouts/${encodeURIComponent(root)}/${suffix}`;
}
async function api(path, method = "GET", body) {
  const session = epoch, root = selectedRootKey, sourceAtStart = sourceSerial;
  const url = selectedUrl(path, method, root);
  const guarded = path === "/api/query" || path.startsWith("/api/query?") || path.startsWith("/api/questions/") || path.startsWith("/api/source?");
  const operationCurrent = operationGuard();
  const current = () => session === epoch && root === selectedRootKey && (!guarded || (operationCurrent() && (!path.startsWith("/api/source?") || sourceAtStart === sourceSerial)));
  let response, data, timer;
  const controller = new AbortController(); pendingRequests.add(controller);
  const timeoutError = new Error("Request timed out. No automatic retry was made. Refresh status before trying again.");
  timeoutError.name = "TimeoutError";
  const deadline = path.endsWith("/acp-answer") ? 150000 : 15000;
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
    if (error.status === 401 || error.status === 403) disconnect("Authentication failed. Connect with a daemon token again.");
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
  catch (error) { if (error.name !== "AbortError" && current()) { if (IndexPin.isConflict(error)) { void refreshStatus().catch(() => {}); window.TrellisShell?.resetInspector(); diagramSerial++; invalidateFocus("Index changed. Preview again after refreshing."); clearSource(); renderResult(); stale("The index revision changed. Refresh this view before reading source."); } $("error").textContent = error.message; $("error").hidden = false; if ($("focus-state").textContent.startsWith("Preparing")) $("focus-state").textContent = "Preview failed. Check the error and try again."; } }
  finally { if (control) control.disabled = false; syncFocusControls(); syncSavedControls(); }
}
function form(id, action) {
  $(id).addEventListener("submit", event => { event.preventDefault(); perform(action, $(id).querySelector("button")); });
}
function stale(message) { $("stale").textContent = message; $("stale").hidden = false; }
function clearSource() {
  sourceSerial++; window.TrellisNavigation?.reset(); $("source").replaceChildren();
  $("source-path").textContent = "Choose a call to read its cached source snapshot.";
  if (!$("source-dock")?.hidden) window.TrellisShell?.closeSource?.();
}
function changedCount(value) { return Array.isArray(value) ? value.length : Number(value || 0); }
async function refreshStatus(followup = false) {
  statusRefreshDepth++;
  try {
  const serial = ++statusSerial, session = epoch;
  let data;
  try { data = await api("/api/status"); }
  catch (error) {
    if (error.code === "index_not_ready" && serial === statusSerial && session === epoch && selectedRootKey) {
      const root = selectedRootKey, workspaceRoot = error.workspaceRoot || status?.workspaceRoot;
      const listedRoot = selectedWorkspaceRoot;
      resetCheckout();
      selectedRootKey = root; selectedWorkspaceRoot = listedRoot;
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
  const hadStatus = !!status;
  if (status && browseChanged) { clearBrowse("Index changed. Choose a method from the refreshed files."); sourceCache.clear(); querySerial++; invalidateFocus("Index workspace or revision changed. Preview again."); clearSource(); }
  if (status && browseChanged) {
    result = null; seed = null; searchSerial++;
    $("symbols").replaceChildren();
    if (workspaceChanged) {
      savedSerial++; views = []; annotations = []; resetView(); resetNote();
      $("views").replaceChildren(); $("annotations").replaceChildren();
    }
    $("seed").textContent = "Select a symbol to inspect its immediate interactions.";
    renderResult();
  }
  if (browseChanged) clearDependencyCatalog();
  status = {...data, revision:nextPin};
  data = status;
  if (hadStatus && browseChanged && !workspaceChanged) { renderViews(); renderNotes(); }
  window.TrellisShell?.updateWorkspace(nextPin ? status : null);
  $("catching-up").hidden = data.catchingUp !== true || !nextPin;
  if (browseChanged && nextPin) await loadTreeRoot();
  if (serial !== statusSerial || session !== epoch || status !== data) return;
  if (result && !IndexPin.equal(result.revision, status.revision)) renderResult();
  const stats = status.stats || {};
  $("status").textContent = `${status.workspaceRoot} · ${status.revision ? `Revision ${IndexPin.label(status.revision)}` : "Index not ready"} · ${stats.files ?? 0} files · ${stats.symbols ?? 0} symbols · ${stats.calls ?? 0} calls · Semantic proof unavailable · SCIP (if present): display only · Indexed: ${status.indexedAt ? new Date(Number(status.indexedAt)).toLocaleString() : "not yet"}`;
  $("diagnostics").textContent = describe(status.diagnostics || []);
  $("stale").hidden = true;
  if (!nextPin) stale("Index not ready. Retry status after this checkout finishes indexing.");
  if (changedCount(stats.changedFiles)) stale(`${changedCount(stats.changedFiles)} inputs differ from the imported SCIP manifest. Syntax remains source-only; imported SCIP is display metadata, never semantic proof.`);
  if (result && !IndexPin.equal(result.revision, status.revision)) stale("This view is from an older revision. Refresh status & view to update it.");
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
  clearBrowse(message); clearSource(); sourceCache.clear(); querySerial++; searchSerial++;
  result = null; seed = null; $("symbols").replaceChildren();
  $("seed").textContent = "Select a symbol to inspect its immediate interactions.";
  invalidateFocus(message); renderResult(); clearDependencyCatalog();
  $("search-state").textContent = message;
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
function requireCurrentPair(value) {
  if (!IndexPin.equal(value?.revision, status?.revision)) { unexpectedPair(undefined, value?.revision); throw new Error("Index snapshot changed. Refresh status."); }
  return value;
}
async function loadSaved() {
  const serial = ++savedSerial, session = epoch, workspace = status?.workspaceRoot;
  const expected = currentNativePin();
  const suffix = expected ? `?${IndexPin.query(expected)}` : "";
  const data = await Promise.all([api(`/api/views${suffix}`), api(`/api/annotations${suffix}`)]);
  if (serial !== savedSerial || session !== epoch || workspace !== status?.workspaceRoot ||
      (expected && !IndexPin.equal(expected, status?.revision))) return;
  views = normalizeSavedList(data[0], "view");
  annotations = normalizeSavedList(data[1], "note");
  if (!expected) {
    const unpinned = state => ({...state, indexGeneration:null, indexRevision:null, savedPair:null,
      attachment:state.attachment.availability === "anchorless" ? state.attachment : {availability:"indexUnavailable", result:null}});
    views = views.map(unpinned); annotations = annotations.map(unpinned);
  }
  renderViews(); renderNotes();
}
const tokenStorageKey = "trellis.daemonToken.v1";
let tokenFileSerial = 0;
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
$("remember-token").addEventListener("change", () => {
  if (!$("remember-token").checked) forgetStoredToken();
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
  epoch++; selectedRootKey = null; selectedWorkspaceRoot = null; checkoutBusy = false;
  for (const controller of pendingRequests) controller.abort();
  clearTimeout(pollTimer); pollTimer = null; statusSerial++; savedSerial++; querySerial++; searchSerial++;
  pairRefreshPending = false; pairRefreshQueued = null;
  clearDependencyCatalog(); clearExternalSources(); clearBrowse(); clearSource(); sourceCache.clear();
  status = null; result = null; seed = null; job = null; waitingForJob = false;
  acpStatusSerial++; acpStatus = null; acpRunning = false; $("acp-status").textContent = "ACP status unavailable.";
  jevStatusSerial++; jevStatus = null; jevRunning = false; $("jev-status").textContent = "Live Jev status unavailable.";
  invalidateFocus(); views = []; annotations = []; resetView(); resetNote();
  ["symbols", "calls", "nodes", "views", "annotations"].forEach(id => $(id).replaceChildren());
  ["status", "diagnostics", "job", "result-meta"].forEach(id => $(id).textContent = "");
  $("seed").textContent = "Select a symbol to inspect its immediate interactions.";
  $("file-filter").value = ""; $("search").value = "";
  $("stale").hidden = true; $("catching-up").hidden = true;
  $("checkout-root").textContent = "No checkout selected";
  $("workspace").hidden = true; $("index").disabled = true; $("cancel").hidden = true;
  window.TrellisShell?.setConnected(false);
}
function disconnect(message) {
  tokenFileSerial++; const forgotten = forgetStoredToken();
  resetCheckout(); checkouts = []; token = "";
  $("remember-token").checked = false; $("token-file").value = ""; $("token").value = "";
  $("checkout-picker").hidden = true; $("checkout-select").replaceChildren(checkoutOption("Choose a checkout", ""));
  $("logout").hidden = true; $("connect-form").hidden = false;
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
  const session = epoch;
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
      const key = selectedRootKey;
      resetCheckout();
      selectedRootKey = key; selectedWorkspaceRoot = verifiedRoot;
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
      const option = checkoutOption(`${row.workspaceRoot || row.rootKey} · ${row.state}`, row.rootKey);
      option.disabled = !selectable(row); select.append(option);
    }
    select.value = choice;
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
  selectedWorkspaceRoot = checkouts.find(row => row.rootKey === key).workspaceRoot;
  $("error").hidden = true; $("error").textContent = "";
  $("checkout-root").textContent = "Verifying selected checkout…";
  $("checkout-state").textContent = "Loading selected checkout…";
  $("workspace").hidden = false; $("refresh").disabled = false; $("index").disabled = false;
  window.TrellisShell?.setConnected(true);
  const session = epoch;
  try {
    await refreshStatus();
    if (session !== epoch) return;
    await loadSaved(); await refreshJevStatus(); await refreshAcpStatus();
    job = await api("/api/jobs/current").catch(error => { if (error.name !== "AbortError") throw error; return null; });
    if (session !== epoch) return;
    waitingForJob = !!job && activeJob(job);
    if (job) { showJob(); schedulePoll(); }
    $("checkout-state").textContent = "Checkout ready.";
    $("notice").textContent = "Browse folders below. Indexed files expand to methods.";
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
    try { localStorage.setItem(tokenStorageKey, token); }
    catch (_) { $("checkout-state").textContent += " Browser storage is unavailable; token was not saved."; }
  }
});
form("connect-form", async () => {
  const supplied = $("token").value.trim();
  if (!safeTokenText(supplied)) throw new Error("Paste a daemon token or load daemon.token first.");
  tokenFileSerial++; token = supplied; $("token").value = "";
  $("error").hidden = true; $("error").textContent = "";
  $("checkout-picker").hidden = false; $("connect-form").hidden = true; $("logout").hidden = false;
  const [listed] = await Promise.all([loadCheckouts(), loadDaemonStatus()]);
  if (!token) return;
  let remembered = false, storageFailed = false;
  if (listed && $("remember-token").checked) {
    try { localStorage.setItem(tokenStorageKey, token); remembered = true; }
    catch (_) { storageFailed = true; }
  } else if (listed) forgetStoredToken();
  $("notice").textContent = listed ? "Connected. Choose a checkout before browsing."
    + (remembered ? " Token saved on this browser." : storageFailed ? " Browser storage is unavailable; token was not saved." : "")
    : "Checkout list unavailable. Retry the list; no checkout has been selected.";
  $("checkout-select").focus();
});
$("logout").addEventListener("click", () => disconnect());

form("search-form", async () => {
  const serial = ++searchSerial;
  $("search-state").textContent = "Searching…";
  const data = await api(`/api/symbols?q=${encodeURIComponent($("search").value)}&limit=80`);
  if (serial !== searchSerial) return;
  requireCurrentPair(data);
  $("symbols").replaceChildren(); $("search-state").textContent = `${data.items.length} symbols · revision ${IndexPin.label(data.revision)}`;
  for (const symbol of data.items) {
    const li = element("li"); const pick = button(symbol.name, () => selectSymbol(symbol)); pick.className = "symbol";
    pick.append(element("span", `${symbol.kind} · ${symbol.path}:${symbol.range.startLine}`, "detail"));
    li.append(pick); $("symbols").append(li);
  }
  if (!data.items.length) $("search-state").textContent = "No symbols found. Try another name or index the workspace.";
});
async function selectSymbol(symbol) {
  diagramSerial++; window.TrellisShell?.resetInspector();
  seed = symbol.id; $("seed").textContent = `${symbol.name} · ${symbol.path}`;
  $("depth").value = "1"; $("callbacks").checked = false; renderNotes(); await runQuery();
}
function query() { return {seed, depth: Number($("depth").value), maxNodes: 40, maxCalls: 200, includeCallbacks: false, excludePaths: []}; }
function savedQueryDto(value) {
  return {
    seed:value.seed,
    depth:Number.isSafeInteger(value.depth) ? value.depth : 1,
    maxNodes:Number.isSafeInteger(value.maxNodes) ? value.maxNodes : 40,
    maxCalls:Number.isSafeInteger(value.maxCalls) ? value.maxCalls : 200,
    includeCallbacks:value.includeCallbacks === true,
    excludePaths:Array.isArray(value.excludePaths) ? value.excludePaths.filter(path => typeof path === "string") : [],
  };
}
function savedViewDto(view, title = view.title) {
  const pins = {};
  if (objectValue(view.pins)) for (const [id, position] of Object.entries(view.pins)) {
    if (objectValue(position)) pins[id] = {x:position.x, y:position.y};
  }
  return {id:view.id, title, query:savedQueryDto(view.query), pins,
    hidden:Array.isArray(view.hidden) ? view.hidden.filter(id => typeof id === "string") : []};
}
function annotationDto(note, titleValue, titleSupplied) {
  return {id:note.id, nodeId:note.nodeId, body:note.body, ...(titleSupplied ? {title:titleValue} : {})};
}
async function runQuery(savedQuery, savedReplayPin = null) {
  if (!seed && !savedQuery?.seed) throw new Error("Select a symbol first.");
  const replayPin = savedReplayPin && IndexPin.copy(savedReplayPin);
  const path = replayPin ? `/api/query?${IndexPin.query(replayPin)}` : "/api/query";
  invalidateFocus(); const serial = ++querySerial; clearSource();
  result = null; $("calls").replaceChildren(); $("nodes").replaceChildren();
  $("result-meta").textContent = "Loading interactions…";
  const data = await api(path, "POST", {...(savedQuery || query()), depth: 1, includeCallbacks: false});
  if (serial !== querySerial) return;
  if (replayPin && !IndexPin.equal(data?.revision, replayPin)) {
    unexpectedPair("Saved replay returned a different index snapshot.", data?.revision);
    throw new Error("Saved replay returned a different index snapshot.");
  }
  result = requireCurrentPair(data); renderResult();
}
form("query-form", () => runQuery());
function renderResult() {
  $("calls").replaceChildren(); $("nodes").replaceChildren();
  const view = focused || result;
  syncFocusControls(); syncSavedControls();
  if (!view) { $("result-meta").textContent = "Select a symbol to inspect outgoing calls."; return; }
  $("result-meta").textContent = `${view.calls.length} measured call sites · ${view.nodes.length} symbols · revision ${IndexPin.label(view.revision)}${view.truncated ? ` · Truncated: ${view.omittedNodes} nodes omitted` : ""}${view.warnings?.length ? ` · ${view.warnings.map(describe).join(" · ")}` : ""}`;
  if (focused) $("focus-counts").textContent = `Supporting: ${view.supportingCount} · Uncertain: ${view.uncertainCount} · Policy-hidden: ${view.policyHiddenCount} · Omitted: ${view.omittedCount}. Only essential calls that pass display limits are shown. Counts may overlap.`;
  const rootNode = (view.nodes || []).find(n => n.id === seed);
  if (rootNode) {
    const rootRow = element("li", undefined, "hierarchy-root");
    rootRow.append(element("strong", rootNode.name || rootNode.id), element("span", `${rootNode.kind} · ${rootNode.path || ""}`, "detail"));
    if (sourceWitness(rootNode)) rootRow.append(button("Read root source", () => showSource(rootNode, view.revision)));
    $("calls").append(rootRow);
  }
  for (const call of view.calls.filter(call => call.caller === seed)) $("calls").append(callRow(call, view));
  if (!view.calls.length) $("calls").append(element("li", focused ? "No essential calls pass the current display policy. Inspect the raw hierarchy or change your focus terms." : "No outgoing call sites in this view. Depth 0 shows only the seed symbol."));
  for (const node of (rootNode ? [rootNode] : [])) {
    const li = element("li", `${node.name || node.id} · ${node.kind || "symbol"}`);
    if (sourceWitness(node)) li.append(button("Read source", () => showSource(node, view.revision)));
    $("nodes").append(li);
  }
}
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
function terminalView(view, selectedSeed, revision) {
  const root = (view.nodes || []).find(node => node.id === selectedSeed);
  const nodes = root ? [{id:root.id, name:root.name, kind:root.kind,
    ...(sourceWitness(root) ? {path:root.path, range:root.range} : {})}] : [];
  const calls = (view.calls || []).filter(call => call.caller === selectedSeed && sourceWitness(call))
    .map(call => ({id:call.id, caller:selectedSeed, path:call.path, range:call.range,
      calleeText:call.calleeText || call.spelling || null,
      regions:(call.regions || []).filter(id => (view.regions || []).some(r => r.id === id && r.path === call.path && sourceWitness(r)))}));
  const regions = (view.regions || []).filter(region => sourceWitness(region) && calls.some(call => call.path === region.path && call.regions.includes(region.id)))
    .map(region => ({id:region.id, kind:region.kind, label:region.label, path:region.path, range:region.range}));
  return {revision:IndexPin.copy(revision), query:{seed:selectedSeed,depth:1,includeCallbacks:false},
    nodes,calls,regions,warnings:[],truncated:false,supportingCount:0,uncertainCount:0,policyHiddenCount:0,omittedCount:0};
}
function callRow(call, view) {
  const nodes = new Map((view.nodes || []).map(node => [node.id, node]));
  const regions = new Map((view.regions || []).map(region => [region.id, region]));
  const li = element("li", undefined, "call-row");
  const witness = sourceWitness(call);
  const heading = element("div", undefined, "call-heading");
  if (witness) {
    const name = button(call.calleeText || call.spelling || "Call site", () => showSource(call, view.revision));
    name.className = "call-name"; name.setAttribute("data-terminal-source", "call"); heading.append(name);
  } else heading.append(element("span", "Call site · source unavailable", "call-name"));
  heading.append(element("span", "Syntax only · target unverified", "badge"));
  li.append(heading);
  if (witness) li.append(element("span", `${call.path}:${call.range.startLine} · from ${nodes.get(call.caller)?.name || call.caller || "unknown"}`, "detail"));
  const evidence = element("details", undefined, "call-evidence");
  evidence.append(element("summary", witness ? `Measured call site · ${call.path}:${call.range.startLine}` : "No measured call-site source"));
  if (call.regions?.length) evidence.append(element("div", `Control context: ${call.regions.map(id => { const r = regions.get(id); return r ? `${r.kind}: ${r.label}` : id; }).join(" · ")}`, "detail"));
  if (witness) evidence.append(button("Read call-site source", () => showSource(call, view.revision)));
  li.append(evidence);
  return li;
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
$("refresh").addEventListener("click", () => perform(async () => { clearSource(); const revision = status?.revision; await refreshStatus(); if (IndexPin.equal(revision, status?.revision)) await refreshTree(); if (selectedMethod) await loadSequence(); else if (seed) await runQuery(); await loadSaved(); await refreshJevStatus(); await refreshAcpStatus(); schedulePoll(); }, $("refresh")));
function savedButton(text, action, kind, id, actionName) {
  const control = button(text, action);
  control.setAttribute("data-saved-kind", kind);
  control.setAttribute("data-saved-id", id);
  control.setAttribute("data-action", actionName);
  return control;
}
function syncSavedControls() {
  const pin = currentNativePin();
  const currentResult = !!result && IndexPin.equal(result.revision, status?.revision);
  $("save-view").disabled = !pin || (!editingView && (!currentResult || !!focused));
  $("save-note").disabled = !pin || (!editingNote && (!currentResult || !seed || result?.query?.seed !== seed));
}
function resetView() {
  editingView = null; $("view-title").value = ""; $("save-view").textContent = "Save raw query"; $("reset-view").hidden = true; syncSavedControls();
}
function beginViewEdit(state) {
  const pin = currentNativePin();
  if (!pin) throw new Error("Index a native snapshot before editing this saved view.");
  const view = state.view, snapshot = savedViewDto(view);
  snapshot.id = state.originalId; snapshot.query.seed = state.originalTarget;
  editingView = {id:state.originalId, target:state.originalTarget, pin, view:snapshot};
  $("view-title").value = view.title; $("save-view").textContent = "Save view title"; $("reset-view").hidden = false;
  $("view-title").focus(); syncSavedControls();
}
async function loadSavedView(state) {
  const decision = savedLoadDecision(state, "view");
  if (!decision.enabled) throw new Error(decision.reason);
  const view = state.view;
  seed = decision.target; $("seed").textContent = `Saved view: ${view.title}`;
  $("depth").value = String(Math.max(0, Math.min(5, Number(view.query.depth ?? 1)))); $("callbacks").checked = false;
  const replay = savedQueryDto(view.query); replay.seed = decision.target;
  renderNotes(); await runQuery(replay, decision.pin);
}
function renderViews() {
  $("views").replaceChildren();
  for (const state of views) {
    const view = state.view, decision = savedLoadDecision(state, "view"), li = element("li");
    li.setAttribute("data-saved-kind", "view"); li.setAttribute("data-saved-id", view.id);
    li.append(element("strong", view.title, "saved-title"));
    const actions = element("div", undefined, "bar saved-actions");
    const load = savedButton("Load", () => loadSavedView(state), "view", view.id, "load");
    load.disabled = !decision.enabled; load.title = decision.reason;
    const editView = savedButton("Edit title", () => beginViewEdit(state), "view", view.id, "edit");
    editView.disabled = !currentNativePin();
    editView.title = editView.disabled ? "Index a native snapshot before editing." : "Edit without loading this view.";
    actions.append(load, editView, savedButton("Delete", async () => {
      await api(`/api/views/${encodeURIComponent(view.id)}`, "DELETE");
      if (editingView?.id === view.id) resetView(); await loadSaved();
    }, "view", view.id, "delete"));
    li.append(actions);
    const reason = element("span", decision.reason, "detail saved-reason");
    reason.id = `saved-view-${view.id}-reason`; load.setAttribute("aria-describedby", reason.id); li.append(reason);
    if (state.orphanedIds.length) li.append(element("span", `${state.orphanedIds.length} saved layout reference${state.orphanedIds.length === 1 ? " is" : "s are"} unavailable.`, "detail"));
    $("views").append(li);
  }
  if (!views.length) $("views").append(element("li", "No saved views yet. Run a raw query, then save it here."));
  syncSavedControls();
}
$("reset-view").addEventListener("click", resetView);
form("save-form", async () => {
  const title = $("view-title").value.trim();
  if (editingView) {
    const edit = editingView, dto = savedViewDto(edit.view, title);
    if (dto.id !== edit.id || dto.query.seed !== edit.target) throw new Error("Saved view identity changed. Cancel and reopen the editor.");
    await api(`/api/views/${encodeURIComponent(edit.id)}?${IndexPin.query(edit.pin)}`, "PUT", dto);
    resetView(); await loadSaved(); return;
  }
  if (focused) throw new Error("Focused selections cannot be saved as raw queries. Return to the raw hierarchy first.");
  const pin = currentNativePin();
  if (!pin || !result || !IndexPin.equal(result.revision, pin)) throw new Error("Run a current pinned query before saving a view.");
  const id = crypto.randomUUID(), dto = savedViewDto({id, title, query:{...result.query, depth:1, includeCallbacks:false}, pins:{}, hidden:[]});
  await api(`/api/views/${encodeURIComponent(id)}?${IndexPin.query(pin)}`, "PUT", dto);
  resetView(); await loadSaved();
});
function resetNote() {
  editingNote = null; $("note-title").value = ""; $("note").value = ""; $("save-note").textContent = "Add note"; $("reset-note").hidden = true; syncSavedControls();
}
$("reset-note").addEventListener("click", resetNote);
function beginNoteEdit(state) {
  const pin = currentNativePin();
  if (!pin) throw new Error("Index a native snapshot before editing this note.");
  const note = state.annotation;
  editingNote = {id:state.originalId, target:state.originalTarget, pin,
    note:annotationDto({...note, id:state.originalId, nodeId:state.originalTarget}, note.title, typeof note.title === "string")};
  $("note-title").value = note.title || ""; $("note").value = note.body;
  $("save-note").textContent = "Save note"; $("reset-note").hidden = false; $("note").focus(); syncSavedControls();
}
async function loadSavedNote(state) {
  const decision = savedLoadDecision(state, "note");
  if (!decision.enabled) throw new Error(decision.reason);
  seed = decision.target; $("seed").textContent = `Saved note target: ${state.annotation.title || state.annotation.id}`;
  $("depth").value = "1"; $("callbacks").checked = false; renderNotes();
  await runQuery({seed:decision.target, depth:1, maxNodes:40, maxCalls:200, includeCallbacks:false, excludePaths:[]}, decision.pin);
}
function renderNotes() {
  $("annotation-target").textContent = editingNote ? `Editing note anchored to ${editingNote.target}. Selection changes will not retarget it.`
    : seed ? `Notes for symbol ${seed}` : "Select a symbol to attach a note. Unavailable notes remain visible.";
  $("annotations").replaceChildren();
  const displayed = annotations.filter(state => state.annotation.nodeId === seed || editingNote?.id === state.annotation.id ||
    state.attachment.availability !== "ready" || state.attachment.result?.status === "orphaned" || !IndexPin.equal(state.savedPair, status?.revision));
  for (const state of displayed) {
    const note = state.annotation, decision = savedLoadDecision(state, "note"), li = element("li");
    li.setAttribute("data-saved-kind", "note"); li.setAttribute("data-saved-id", note.id);
    if (note.title) li.append(element("strong", note.title, "saved-title"));
    li.append(element("p", note.body));
    const actions = element("div", undefined, "bar saved-actions");
    const load = savedButton("Load", () => loadSavedNote(state), "note", note.id, "load");
    load.disabled = !decision.enabled; load.title = decision.reason;
    const editNote = savedButton("Edit", () => beginNoteEdit(state), "note", note.id, "edit");
    editNote.disabled = !currentNativePin();
    editNote.title = editNote.disabled ? "Index a native snapshot before editing." : "Edit without changing the saved target.";
    actions.append(load, editNote, savedButton("Delete", async () => {
      await api(`/api/annotations/${encodeURIComponent(note.id)}`, "DELETE");
      if (editingNote?.id === note.id) resetNote(); await loadSaved();
    }, "note", note.id, "delete"));
    li.append(actions);
    const reason = element("span", decision.reason, "detail saved-reason");
    reason.id = `saved-note-${note.id}-reason`; load.setAttribute("aria-describedby", reason.id); li.append(reason);
    $("annotations").append(li);
  }
  if (!displayed.length) $("annotations").append(element("li", seed ? "No notes for this symbol." : "No saved notes yet."));
  syncSavedControls();
}
form("annotation-form", async () => {
  const title = $("note-title").value.trim(), body = $("note").value;
  if (editingNote) {
    const edit = editingNote;
    const titleSupplied = !!title || own(edit.note, "title");
    const dto = annotationDto({id:edit.id, nodeId:edit.target, body}, title, titleSupplied);
    await api(`/api/annotations/${encodeURIComponent(edit.id)}?${IndexPin.query(edit.pin)}`, "PUT", dto);
    resetNote(); await loadSaved(); return;
  }
  const pin = currentNativePin();
  if (!pin || !seed || !result || result.query?.seed !== seed || !IndexPin.equal(result.revision, pin)) throw new Error("Run a current pinned query for the selected symbol before adding a note.");
  const id = crypto.randomUUID(), dto = annotationDto({id, nodeId:seed, body}, title, !!title);
  await api(`/api/annotations/${encodeURIComponent(id)}?${IndexPin.query(pin)}`, "PUT", dto);
  resetNote(); await loadSaved();
});
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
    else { waitingForJob = false; showJob(); await refreshStatus(); await loadSaved(); }
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

async function refreshJevStatus() {
  const serial = ++jevStatusSerial;
  try {
    const data = await api("/api/jev/status");
    if (serial !== jevStatusSerial) return;
    jevStatus = data;
    const b = data.budget;
    $("jev-status").textContent = data.enabled && b
      ? `Live Jev enabled · $${(b.remainingCents / 100).toFixed(2)} available of $${(b.capCents / 100).toFixed(2)} · $${(b.reservedCents / 100).toFixed(2)} reserved · ${b.attempts} attempts. Conservative reservation accounting, not invoiced cost. Each attempt reserves $0.10, including failures.`
      : "Live Jev disabled. Offline preview and JSON export/import remain available.";
  } catch (error) {
    if (serial !== jevStatusSerial || error.name === "AbortError") return;
    jevStatus = null;
    $("jev-status").textContent = "Live Jev status unavailable. Refresh status before running.";
  }
  syncFocusControls();
}
function syncFocusControls() {
  $("explain-acp").disabled = acpRunning || !safePacket() || !acpStatus?.enabled || !(acpStatus.status?.remainingAttempts > 0);
  $("run-jev").disabled = jevRunning || !safePacket() || !jevStatus?.enabled || !(jevStatus.budget?.remainingCents >= 10);

  $("save-view").disabled = !!focused;
  $("save-view").title = focused ? "Return to the raw hierarchy to save its query" : "Saves raw query settings, not a focused selection";
  $("return-raw").hidden = !focused;
  $("query-form").hidden = !!focused;
  $("focus-counts").hidden = !focused;
  $("packet-actions").hidden = !safePacket();
  $("result-title").textContent = focused ? "Focused outgoing calls" : "Outgoing call hierarchy";
}
function invalidateFocus(message = "Prepare preview offline first, then explicitly Run Jev with that packet. No automatic provider calls.") {
  clearAnswer(); questionSerial++; packet = null; focused = null;
  $("import-jev").value = ""; $("focus-state").textContent = message;
  syncFocusControls();
}
function questionChanged() {
  const hadFocus = !!focused;
  invalidateFocus("Question settings changed. Preview again to prepare a new evidence packet.");
  if (hadFocus) { clearSource(); renderResult(); }
}
$("question-form").addEventListener("input", questionChanged);
$("query-form").addEventListener("change", () => {
  querySerial++; invalidateFocus("Raw query settings changed. Apply the query or preview again."); clearSource(); renderResult();
});
form("question-form", async () => {
  if (!seed || !status) throw new Error("Select an indexed symbol first.");
  invalidateFocus(); clearSource(); renderResult();
  const serial = questionSerial, queryAtStart = querySerial, selectedSeed = seed, revision = IndexPin.copy(status.revision);
  const terms = $("focus-terms").value.split(",").map(s => s.trim()).filter(Boolean);
  if (terms.length > 12) throw new Error("Use no more than 12 focus terms.");
  if (!safeEvidenceFormat()) {
    packet = null; focused = null;
    $("focus-state").textContent = "Source-only preview. Packet actions are unavailable without verified terminal status.";
  }
  $("focus-state").textContent = "Preparing local evidence and offline preview…";
  const data = await api("/api/questions/preview", "POST", {seed, question: $("question").value.trim(), expectedRevision: revision,
    evidenceDepth: Number($("evidence-depth").value), maxVisible: Number($("max-visible").value),
    allowDeeperDisplay: $("allow-deeper").checked, focusTerms: terms});
  if (serial !== questionSerial || queryAtStart !== querySerial || seed !== selectedSeed || !IndexPin.equal(status?.revision, revision)) return;
  if (!IndexPin.equal(data.packet.revision, revision) || !IndexPin.equal(data.view.revision, revision)) { unexpectedPair("Preview revision changed. Refresh and try again.", data.packet.revision); throw new Error("Preview revision changed. Refresh and try again."); }
  if (!safeEvidenceFormat() || hasLexicalProof(data.packet) || hasLexicalProof(data.view)) {
    packet = null; focused = terminalView(data.view, selectedSeed, revision);
    $("focus-state").textContent = `Source-only terminal preview · revision ${IndexPin.label(revision)}. Packet and provider actions unavailable.`;
    renderResult(); return;
  }
  packet = data.packet; focused = terminalView(data.view, selectedSeed, revision);
  $("focus-state").textContent = `Local preview—not Jev/ACP · deterministic term matching · revision ${IndexPin.label(revision)}. ${packet.warnings?.map(describe).join(" · ") || ""}`;
  renderResult();
});
$("return-raw").addEventListener("click", () => { focused = null; questionSerial++; clearAnswer(); clearSource(); renderResult(); });
function safeEvidenceFormat() {
  // This status marker is an explicit frontend cutover contract. Old indexes omit it.
  return status?.evidenceFormat === "terminal-native-graph-v1" &&
    !!status.revision;
}
function safePacket() {
  return !!packet && safeEvidenceFormat() && IndexPin.equal(packet.revision, status.revision) &&
    !hasLexicalProof(packet) && (!focused || (IndexPin.equal(focused.revision, status.revision) && !hasLexicalProof(focused)));
}
function hasLexicalProof(value, seen = new Set()) {
  if (!value || typeof value !== "object") return false;
  if (seen.has(value)) return true;
  seen.add(value);
  if (Object.keys(value).some(key => ["target", "candidateSymbols", "resolution", "callbackArguments", "includeCallbacks", "candidateIds", "derivedFrom"].includes(key))) return true;
  return Object.values(value).some(child => hasLexicalProof(child, seen));
}
function currentPacket() {
  if (!safePacket() || !IndexPin.equal(packet.revision, status?.revision) || packet.request.seed !== seed) throw new Error("Packet actions require a verified terminal snapshot. Reindex before using a provider or export.");
  return packet;
}
$("export-jev").addEventListener("click", () => perform(async () => {
  const current = currentPacket(), serial = questionSerial;
  if (!window.confirm("This JSON includes indexed source code. Export it only if you are allowed to share that code. No provider request will be sent. Download now?")) return;
  const data = await api(`/api/questions/${encodeURIComponent(current.packetId)}/jev-request`);
  if (packet !== current || serial !== questionSerial) return;
  const json = JSON.stringify(data);
  const bytes = new TextEncoder().encode(json);
  if (bytes.byteLength > 176000) throw new Error(`Provider request is ${bytes.byteLength} bytes; the limit is 176000 bytes. Nothing was downloaded. Reduce evidence depth or select a smaller root and preview again.`);
  const blob = new Blob([bytes], {type: "application/json"});
  const url = URL.createObjectURL(blob), link = element("a");
  link.href = url; link.download = "trellis-jev-request.json"; document.body.append(link); link.click(); link.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
  $("focus-state").textContent = "Provider request downloaded with source code. No provider call was made.";
}, $("export-jev")));
$("import-jev").addEventListener("change", () => {
  const file = $("import-jev").files[0]; $("import-jev").value = "";
  if (!file) return;
  perform(async () => {
    const current = currentPacket(), serial = ++questionSerial, session = epoch;
    clearAnswer();
    if (file.size > 2 * 1024 * 1024) throw new Error("Response file is too large (maximum 2 MiB).");
    let data;
    try { data = JSON.parse(await file.text()); } catch { throw new Error("Choose a valid Jev response JSON file."); }
    if (epoch !== session || packet !== current || serial !== questionSerial) return;
    const response = await api(`/api/questions/${encodeURIComponent(current.packetId)}/jev-response`, "POST", data);
    if (packet !== current || serial !== questionSerial) return;
    if (!IndexPin.equal(response.view.revision, status?.revision)) { unexpectedPair(undefined, response.view.revision); throw new Error("Imported Jev revision changed. Refresh status."); }
    clearSource(); focused = response.view;
    $("focus-state").textContent = "Imported Jev · user-supplied, unverified response. No live Jev/ACP call was made.";
    renderResult();
  });
});
syncFocusControls();

$("run-jev").addEventListener("click", () => perform(async () => {
  const current = currentPacket();
  if (!jevStatus?.enabled || jevRunning) return;
  if (!window.confirm("Send the entire prepared evidence packet to Jev? This includes your question and complete indexed source files, not just visible calls. Continue only if you are allowed to share all of this source. This attempt reserves $0.10 even if it fails; reservation accounting is not invoiced cost. Run Jev now?")) return;
  const serial = ++questionSerial, session = epoch;
  clearAnswer();
  jevRunning = true;
  $("focus-state").textContent = "Running Jev with the prepared packet… No automatic retries.";
  syncFocusControls();
  try {
    const response = await api(`/api/questions/${encodeURIComponent(current.packetId)}/jev-run`, "POST", {});
    if (packet !== current || serial !== questionSerial) return;
    if (!IndexPin.equal(response.view.revision, status?.revision)) { unexpectedPair(undefined, response.view.revision); throw new Error("Jev revision changed. Refresh status."); }
    clearSource(); focused = response.view;
    $("focus-state").textContent = `Live Jev · provider selection, not proof of correctness or execution order · revision ${IndexPin.label(response.view.revision)} · ${response.latencyMs} ms.`;
    renderResult();
  } catch (error) {
    if (session === epoch && packet === current && serial === questionSerial && error.name !== "AbortError")
      $("focus-state").textContent = "Jev attempt failed. Any reservation is retained. No automatic retry was made.";
    throw error;
  } finally {
    if (session === epoch) {
      jevRunning = false;
      await refreshJevStatus();
      syncFocusControls();
    }
  }
}, $("run-jev")));

// Status reads do not start inference. Unknown status must fail closed.
async function refreshAcpStatus() {
  const serial = ++acpStatusSerial;
  acpStatus = null;
  $("acp-status").textContent = "Checking ACP allowance…";
  syncFocusControls();
  try {
    const data = await api("/api/acp/status");
    if (serial !== acpStatusSerial) return;
    acpStatus = data;
    const allowance = data.status;
    $("acp-status").textContent = data.enabled && allowance
      ? `ACP · ${allowance.model} · ${allowance.remainingAttempts} of ${allowance.maxAttempts} attempts remaining (${allowance.attempts} used). Claude subscription allowance, separate from Jev. Up to $${allowance.maxEstimatedUsdPerAttempt} estimated per attempt; not a bill or hard spending guarantee.`
      : "ACP disabled. Offline preview and Jev selection remain available.";
  } catch (error) {
    if (serial !== acpStatusSerial || error.name === "AbortError") return;
    acpStatus = null;
    $("acp-status").textContent = "ACP status unavailable. Refresh status before explaining.";
  }
  syncFocusControls();
}
function clearAnswer() {
  answerSerial++;
  $("answer").hidden = true;
  $("answer-content").replaceChildren();
  $("answer-meta").textContent = "";
  $("answer-state").textContent = "";
}
function renderAnswer(response) {
  const content = $("answer-content"); content.replaceChildren();
  function claims(items) {
    for (const claim of items) {
      const row = element("div", undefined, "answer-claim");
      row.append(element("p", claim.text));
      const citations = element("div", undefined, "answer-citations");
      for (const citation of claim.citations) {
        const link = button(`${citation.path}:${citation.startLine}–${citation.endLine}`, () => showSource({path: citation.path, range: {startLine: citation.startLine, endLine: citation.endLine}}, response.revision));
        link.title = citation.quote;
        citations.append(link);
      }
      row.append(citations); content.append(row);
    }
  }
  claims(response.answer.summary);
  if (response.answer.branches.length) { content.append(element("h3", "Branch cases")); claims(response.answer.branches); }
  if (response.answer.limitations.length) {
    content.append(element("h3", "Model caveats · unverified"));
    const list = element("ul");
    for (const caveat of response.answer.limitations) list.append(element("li", caveat));
    content.append(list);
  }
  $("answer-meta").textContent = `Live ACP · Claude · revision ${IndexPin.label(response.revision)} · ${response.latencyMs} ms · ${response.estimatedUsd == null ? "cost estimate unavailable" : `$${response.estimatedUsd} estimated (not billed cost)`}`;
  $("answer").hidden = false;
}
$("explain-acp").addEventListener("click", () => perform(async () => {
  const current = currentPacket();
  if (!acpStatus?.enabled || !(acpStatus.status?.remainingAttempts > 0) || acpRunning) return;
  if (!window.confirm("Send the FULL prepared source evidence to Claude via ACP? This includes your question and complete indexed source files, not just the five displayed calls. Continue only if you are allowed to share all of this source. This uses your Claude subscription and a separate ACP attempt allowance, NOT Jev’s budget. Failed or incomplete attempts still count. Cost estimates are not bills or hard spending guarantees. Explain with ACP now?")) return;
  clearAnswer();
  const serial = answerSerial, session = epoch, operationCurrent = operationGuard();
  const currentRequest = () => session === epoch && serial === answerSerial && packet === current && operationCurrent();
  acpRunning = true; syncFocusControls();
  $("answer-state").textContent = "Explaining with ACP… Full packet sent; no automatic retry.";
  try {
    const response = await api(`/api/questions/${encodeURIComponent(current.packetId)}/acp-answer`, "POST", {});
    if (!currentRequest()) return;
    if (!IndexPin.equal(response.revision, status?.revision)) {
      unexpectedPair("ACP answer provenance does not match this packet. Refresh status.", response.revision);
      $("answer-state").textContent = "ACP answer provenance does not match this packet. Refresh status.";
      $("error").textContent = $("answer-state").textContent; $("error").hidden = false;
      throw new Error("ACP answer provenance does not match this packet. Refresh status.");
    }
    if (response.packetId !== current.packetId || response.answer?.packetId !== current.packetId || response.source !== "liveAcp")
      throw new Error("ACP answer provenance does not match this packet. Nothing displayed.");
    renderAnswer(response);
    $("answer-state").textContent = "";
  } catch (error) {
    if (!currentRequest()) throw aborted();
    if (error.name !== "AbortError") $("answer-state").textContent = error.message || "ACP attempt failed. The attempt allowance is retained. No automatic retry was made.";
    throw error;
  } finally {
    if (session === epoch) {
      acpRunning = false;
      acpStatus = null;
      syncFocusControls();
      await refreshAcpStatus();
    }
  }
}, $("explain-acp")));

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
  selectedMethod = symbol; $("method-source").disabled = false; seed = symbol.id; querySerial++; invalidateFocus(); result = null;
  $("seed").textContent = `${symbol.name} · ${symbol.path}`;
  clearSource(); renderNotes(); renderResult(); renderFiles();
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
