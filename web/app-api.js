"use strict";
// DOM helpers, the guarded API client and the revision-bound cached reads. Declarations only; app.js wires them.
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
  const simple = new Set(["status", "status/head", "git", "git/diff", "metrics", "architecture", "architecture/details", "architecture/rules", "architecture/rules/preview", "architecture/rules/apply", "index", "tree", "files", "methods", "symbols", "syntax-matches", "sequence", "classes", "class-diagram", "navigation", "source", "jobs/current", "dependencies", "dependencies/refresh", "dependencies/symbols", "dependencies/source", "rust-sources", "rust-sources/tree", "rust-sources/file"]);
  if (route === "architecture/rules" && method !== "GET" || ["architecture/rules/preview", "architecture/rules/apply"].includes(route) && method !== "POST") throw new Error("Invalid rules request method.");
  if (route === "syntax-matches" && method !== "POST") throw new Error("Syntax match requests must be POST.");
  if (["git", "git/diff", "metrics", "architecture", "architecture/details", "symbols"].includes(route) && method !== "GET") throw new Error("Read-only API requests must be GET.");
  const item = /^jobs\/[^/?]+(?:\/cancel)?$/.test(route);
  if (!simple.has(route) && !item || /[#\s]/.test(suffix) || suffix.includes("//")) throw new Error("Unknown checkout API route.");
  if (!root || !uniqueSelectableRoot(root)) throw new Error("Choose an available checkout first.");
  return `/api/checkouts/${encodeURIComponent(root)}/${suffix}`;
}
// `conditional` ({etag, data}) revalidates a GET: a 304 answers with the remembered data,
// and a fresh answer replaces both (#666).
async function api(path, method = "GET", body, callerCurrent = () => true, conditional = null) {
  const session = epoch, root = selectedRootKey, sourceAtStart = sourceSerial;
  const url = selectedUrl(path, method, root);
  const guarded = path.startsWith("/api/source?");
  const operationCurrent = operationGuard();
  const current = () => callerCurrent() && session === epoch && root === selectedRootKey && (!guarded || (operationCurrent() && sourceAtStart === sourceSerial));
  let response, data, timer;
  const controller = new AbortController(); pendingRequests.add(controller);
  const timeoutError = new Error("Request timed out. Trellis retries once the daemon answers again.");
  timeoutError.name = "TimeoutError";
  const deadline = 15000;
  try {
    const timeout = new Promise((_, reject) => {
      timer = setTimeout(() => { reject(timeoutError); controller.abort(); }, deadline);
    });
    await Promise.race([timeout, (async () => {
      response = await fetch(url, {method, cache: "no-store", credentials: "omit", redirect: "error", signal: controller.signal,
        headers: {Authorization: `Bearer ${token}`, ...(body === undefined ? {} : {"Content-Type": "application/json"}),
          ...(conditional?.etag ? {"If-None-Match": conditional.etag} : {})},
        body: body === undefined ? undefined : JSON.stringify(body)});
      if (!current()) throw aborted();
      // An error answer may carry no JSON at all (a proxy's or a dying server's HTML, or an empty
      // body); it is still classified by its status below (#906).
      data = response.status === 204 ? null : response.status === 304 && conditional?.etag ? conditional.data
        : response.ok ? await response.json() : await response.json().catch(() => null);
    })()]);
  } catch (error) {
    if (!current()) throw aborted();
    // A deadline or a failed fetch (no response) is a stall, not an answer (#906).
    if (error === timeoutError || (!response && error?.name === "TypeError")) { error.transient = true; noteStall(); }
    throw error;
  }
  finally { clearTimeout(timer); pendingRequests.delete(controller); }
  // Check before any HTTP error side effects, including invalidation on 409.
  if (!current()) throw aborted();
  if (response.status === 304 && conditional?.etag) return data;
  if (!response.ok) {
    const error = new Error(data?.error?.message || `Request failed (${response.status})`);
    error.status = response.status;
    error.code = data?.error?.code;
    error.reason = data?.error?.reason;
    if (error.code === "invalid_rules") error.diagnostic = data.error;
    error.workspaceRoot = response.headers?.get?.("X-Trellis-Workspace") || null;
    // A typed refusal to read one path is not an authentication rejection, so it keeps the
    // sign-in and the saved token (#906).
    if (error.status === 401 || (error.status === 403 && !PATH_FORBIDDEN.has(error.code))) disconnect(`Authentication failed (${error.code || error.status}). Connect with a daemon token again.`);
    else if (root && error.code === "workspace_selection_failed") invalidateSelection("Checkout identity changed. Choose an available checkout again.");
    error.transient = TRANSIENT_CODES.has(error.code) || (!error.code && [502, 503, 504].includes(error.status));
    if (error.transient) noteStall();
    throw error;
  }
  if (conditional) {
    conditional.etag = response.headers?.get?.("ETag") || null;
    conditional.data = data;
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
// Architecture and metrics reads are cached per index revision on this page (#527): they are
// derived from the revision and the reviewed rules file, so a revision change, Refresh, or a rules
// edit applied here drops them. An in-flight read is shared; each caller gets its own copy.
const revisionReads = new Map();
let revisionReadsKey = null;
let architectureRulesHash = null;
// Advances whenever the cached reads are dropped, so Architecture can tell a held snapshot from one
// the rules have since moved past (#789).
let rulesEpoch = 0;
function clearRevisionReads(rereadApps = true) {
  revisionReads.clear(); revisionReadsKey = null; architectureRulesHash = null; rulesEpoch++;
  // The rules may assign files to other apps: the switchers read the new assignment. Refresh
  // reads it itself, once the status says which pin it is for.
  if (rereadApps) void Promise.resolve().then(refreshAppInventory);
}
async function revisionRead(path, current = () => true) {
  const route = path.split("?", 1)[0];
  if (!["/api/architecture", "/api/metrics"].includes(route) || !status?.revision) return api(path, "GET", undefined, current);
  const key = `${epoch}:${selectedRootKey}:${IndexPin.key(status.revision)}`;
  if (revisionReadsKey !== key) { revisionReads.clear(); revisionReadsKey = key; }
  let entry = revisionReads.get(path);
  const hit = !!entry;
  if (!entry) {
    entry = api(path, "GET", undefined);
    revisionReads.set(path, entry);
    // A failed read and a `building` answer (#556) are not results of this revision.
    const forget = () => { if (revisionReads.get(path) === entry) revisionReads.delete(path); };
    entry.then(data => { if (data?.payload?.state === "building") forget(); }, forget);
  }
  const data = await entry;
  if (!current()) throw aborted();
  // A cached read still compares the shown revision with the head.
  if (hit) observePinnedResponse(path, "GET", undefined, data);
  return typeof structuredClone === "function" ? structuredClone(data) : data;
}
// The architecture read: the rules file is read live, so it can change at the same revision. Any
// sign that it did drops the cached reads, so "Reload Architecture" and every recovery read the
// current file: a rules_conflict from any read, a rules read whose hash differs from the cached
// summary's, and any rules preview or apply.
async function architectureRequest(path, current, options = {}) {
  const route = path.split("?", 1)[0], method = options.method || "GET";
  try {
    const data = method === "GET" ? await revisionRead(path, current) : await api(path, method, options.body, current);
    const hash = data?.payload?.rulesFile?.contentHash;
    if (route === "/api/architecture" && hash) {
      // A summary under other rules than the last one seen: the rules changed at this pin, so the
      // cached reads and the shared app assignment are dropped and read again (#852).
      const changed = !!architectureRulesHash && hash !== architectureRulesHash;
      if (changed) clearRevisionReads();
      architectureRulesHash = hash;
    } else if (route === "/api/architecture/rules" && hash && architectureRulesHash && hash !== architectureRulesHash) clearRevisionReads();
    return data;
  } catch (error) {
    if (error?.code === "rules_conflict") clearRevisionReads();
    throw error;
  } finally { if (method === "POST" && route.startsWith("/api/architecture/rules/")) clearRevisionReads(); }
}
