"use strict";
// Pinned-head observation, agent shows (trellis_show), follow or pin, and stall recovery. Declarations only; app.js wires them.
let headObservation = null;
// The last head digest and its ETag for the selected checkout: an unchanged head is a 304 (#666).
let headRevalidation = {root:null, etag:null, data:null};
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
          if (headRevalidation.root !== root) headRevalidation = {root, etag:null, data:null};
          const data = await api("/api/status/head", "GET", undefined, () => current() && headObservation === observation, headRevalidation);
          if (!current() || headObservation !== observation) return;
          if (data.workspaceRoot !== workspace || !data.revision) throw new Error("Head unavailable");
          const head = IndexPin.copy(data.revision);
          // A later response cannot erase a newer same-generation observation.
          if (!observation.head || head.indexGeneration !== observation.head.indexGeneration || head.indexRevision >= observation.head.indexRevision) observation.head = head;
          observation.failed = false;
          if (IndexPin.equal(head, pin)) observeCatchingUp(data.catchingUp);
          observeViewRequest(data.viewRequest);
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
// The head digest carries `catchingUp`. When the head is the revision this page shows and the
// value no longer matches its status, status is read again, once per changed value, so a
// finished re-run clears Catching up (#906).
let catchingUpAsked = null;
function observeCatchingUp(value) {
  if (typeof value !== "boolean" || !status || value === (status.catchingUp === true)) { catchingUpAsked = null; return; }
  const asked = `${epoch}:${selectedRootKey}:${IndexPin.key(status.revision)}:${value}`;
  if (catchingUpAsked === asked) return;
  catchingUpAsked = asked;
  void refreshStatus().catch(() => {});
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

// Agent shows (#818). Each status poll carries the checkout's latest `trellis_show` request.
// The first one seen for a checkout is a baseline; a newer generation navigates this view, the
// way its deep link would, unless the view is pinned. A pinned view still counts it as seen.
const FOLLOW_MS = 5000;
let followTimer = null, followSeen = null, agentShown = null;
let followPinned = (() => { try { return localStorage.getItem("trellis.followPinned") === "true"; } catch (_) { return false; } })();
function scheduleFollow() {
  clearTimeout(followTimer); followTimer = null;
  if (!token || !selectedRootKey || !status?.revision) return;
  followTimer = setTimeout(() => { followTimer = null; observePinnedHead(); scheduleFollow(); }, FOLLOW_MS);
}
function viewRequestOf(value) {
  if (!objectValue(value) || !Number.isSafeInteger(value.generation) || value.generation < 1) return null;
  let revision;
  try { revision = IndexPin.copy(value.revision); } catch (_) { return null; }
  if (["sequence", "class"].includes(value.view) && typeof value.symbolId === "string" && value.symbolId)
    return {generation:value.generation, view:value.view, symbolId:value.symbolId, revision};
  if (value.view === "source" && typeof value.path === "string" && value.path && Number.isSafeInteger(value.line) && value.line >= 1)
    return {generation:value.generation, view:"source", path:value.path, line:value.line, revision};
  return null;
}
function observeViewRequest(value) {
  if (!token || !selectedRootKey) return;
  const request = value == null ? null : viewRequestOf(value);
  if (value != null && !request) return;
  const scope = `${epoch}:${selectedRootKey}`, generation = request?.generation ?? 0;
  if (!followSeen || followSeen.scope !== scope) { followSeen = {scope, generation}; return; }
  if (generation <= followSeen.generation) return;
  followSeen.generation = generation;
  if (followPinned) return;
  return perform(() => showAgentView(request));
}
// Stall recovery (#906). A read that met a deadline, a failed connection or a busy daemon is
// not left failed or loading: the page runs the menu's Refresh (fresh status, then the visible
// view) after 2 s, doubling to at most 60 s while the daemon still does not answer. One attempt
// runs at a time; a new checkout, session or Disconnect forgets it. A show that a stall stopped
// is shown afterwards if it still owns the view.
const TRANSIENT_CODES = new Set(["storage_busy", "deadline_exceeded", "bounded_work", "rebuild_in_progress"]);
const PATH_FORBIDDEN = new Set(["source_forbidden", "symlink_forbidden", "directory_forbidden"]);
const RECOVERY_FIRST_MS = 2000, RECOVERY_MAX_MS = 60000;
let recovery = null, stalledShow = null, stallCount = 0, recovered = null;
function noteStall() {
  if (!token || !selectedRootKey) return;
  stallCount++;
  const scope = `${epoch}:${selectedRootKey}`;
  if (recovery?.scope === scope) { recovery.again = true; return; }
  // A view refreshed but not awaited (Architecture) can stall again just after an attempt
  // ended: that is the same outage, so the backoff goes on instead of starting over.
  const delay = recovered?.scope === scope && Date.now() - recovered.at <= RECOVERY_MAX_MS
    ? Math.min(recovered.delay * 2, RECOVERY_MAX_MS) : RECOVERY_FIRST_MS;
  clearRecovery();
  recovery = {scope, delay, timer:null, again:false};
  recovery.timer = setTimeout(() => void recover(recovery), recovery.delay);
}
function clearRecovery() { if (recovery) clearTimeout(recovery.timer); recovery = null; stalledShow = null; recovered = null; }
async function recover(attempt) {
  attempt.timer = null;
  if (recovery !== attempt || attempt.scope !== `${epoch}:${selectedRootKey}`) return;
  attempt.again = false;
  // A stopped show still owns the view only while it is the newest show and nothing has moved
  // the view since it stopped: no user navigation, pin, checkout, session or pin change.
  const owns = claim => !!claim && claim === stalledShow && claim.ticket === showSerial && !followPinned &&
    claim.marks === viewMarks() && navigationCurrent(claim.context) && IndexPin.equal(claim.request.revision, status?.revision);
  if (!owns(stalledShow)) stalledShow = null;
  const claim = stalledShow;
  // With a claim, only status is read first: status reads move no view, so a navigation made
  // while it is awaited is still seen by the check below. The show then loads its own view.
  await perform(claim ? () => refreshStatus() : refreshView);
  if (recovery !== attempt) return;
  if (claim && !attempt.again) {
    // The show loads its own view, so its reads decide whether this attempt succeeded. If
    // something else took the view while status was read, the stale show is dropped and the
    // view on screen is refreshed instead.
    if (owns(claim)) { stalledShow = null; await perform(() => showAgentView(claim.request)); }
    else { if (stalledShow === claim) stalledShow = null; await perform(refreshView); }
    if (recovery !== attempt) return;
  }
  if (attempt.again) {
    attempt.delay = Math.min(attempt.delay * 2, RECOVERY_MAX_MS);
    attempt.timer = setTimeout(() => void recover(attempt), attempt.delay);
    return;
  }
  recovery = null; recovered = {scope:attempt.scope, delay:attempt.delay, at:Date.now()};
}
function agentLabel(request) {
  return request.view === "source" ? `${request.path}:${request.line}` : request.view === "class" ? "Class diagram" : "Sequence";
}
// One agent show at a time owns the view (#824). A show is stale once a newer show starts,
// the user navigates (any navigation, diagram, section or source change), the view is pinned,
// or the checkout, session or pin changes; a stale show changes nothing.
let showSerial = 0;
const viewMarks = () => `${navigationSerial}:${diagramSerial}:${sectionSerial}:${sourceSerial}`;
async function showAgentView(request) {
  if (!token || !selectedRootKey || !status?.revision) return;
  const pin = IndexPin.copy(status.revision), context = navigationContext(), ticket = ++showSerial, started = viewMarks();
  const owned = () => ticket === showSerial && !followPinned && viewMarks() === started && navigationCurrent(context);
  // `claim.marks` are the view's marks this show itself left; a source read takes its own.
  const stalls = stallCount, claim = {request, ticket, marks:started, context};
  let applied = false;
  try { applied = await applyAgentView(request, pin, context, ticket, owned, claim); }
  finally {
    // Only a show that still owns the view, and that a stall stopped, waits for recovery.
    if (!applied && stallCount !== stalls && ticket === showSerial && !followPinned && navigationCurrent(context) && claim.marks === viewMarks())
      stalledShow = claim;
  }
}
async function applyAgentView(request, pin, context, ticket, owned, claim) {
  // The deep link's stale-pin rule: never show a target from another snapshot.
  if (!IndexPin.equal(request.revision, pin)) {
    showAgentNote(`The agent showed a ${agentLabel(request)} at index ${IndexPin.label(request.revision)}; this view is at ${IndexPin.label(pin)}.`, request, false);
    return true;
  }
  let title = agentLabel(request), pushed = true;
  if (request.view === "sequence") {
    const data = await api("/api/sequence", "POST", {seed:request.symbolId, expectedRevision:pin, showAll:false, interactions:true}, owned);
    if (!owned()) return;
    if (!IndexPin.equal(data?.revision, pin) || data?.seed?.id !== request.symbolId) throw new Error("Sequence provenance mismatch. Nothing displayed.");
    title = `Sequence · ${data.seed.name || data.seed.id}`;
    rememberVisible(pin);
    await navigateDiagram(data.seed, pin, "sequence", data, false, null, null, true);
  } else if (request.view === "class") {
    rememberVisible(pin);
    const opening = navigateDiagram({id:request.symbolId}, pin, "class", null, false, null, null, true);
    // This show's own navigation has taken its ticket; only a later navigation changes it.
    const navigation = navigationSerial;
    const outcome = await opening;
    // Classes reports its own load failure. Only a stall of this show's own, still-current class
    // read, with nothing navigated since, leaves the show to recovery, which then owns the view
    // the show itself opened. Any other failure or stall drops the show (#906).
    if (outcome?.stalled && navigation === navigationSerial && ticket === showSerial && !followPinned && navigationCurrent(context)) {
      claim.marks = viewMarks(); return false;
    }
    if (navigation !== navigationSerial || ticket !== showSerial) return false;
  } else {
    // The source read takes its own serials, which a user's source or Files choice advances;
    // the read paints only while this show is still the latest and unpinned. Nothing else is
    // published unless it painted and still owns the view.
    const before = visibleEntry(pin);
    const mine = () => ticket === showSerial && !followPinned && navigationCurrent(context);
    const range = {startLine:request.line, endLine:request.line};
    const reading = section === "files" ? openFilesLocation({path:request.path, range}, pin, mine)
      : showCachedSource(request.path, range, pin, mine);
    // The read advanced its own serials before its first await; later changes are someone else's.
    claim.marks = viewMarks();
    const painted = await reading;
    if (painted !== true || !mine()) return;
    writeEntry(before);
    const entry = captureSourceEntry();
    pushed = !!entry && !!window.history?.pushState;
    if (entry) rememberNavigation(entry, false, "#source");
  }
  if (ticket === showSerial && navigationCurrent(context)) { showAgentNote(`Shown by agent · ${title}`, null, pushed && !!window.history?.pushState); return true; }
  return false;
}
// The view visible now as a history entry, so agent Back returns to it rather than to a
// sequence still held from an earlier visit (#824). Written only when the show publishes.
let classesSeed = null;
function visibleEntry(revision, visible = currentDiagramView()) {
  if (section !== "diagrams" || visible === "sequence") return currentEntry(revision, null, visible);
  const source = captureSourceEntry(visible);
  if (source) return {entry:source, hash:"#source"};
  if (visible === "classes" && classesSeed) return {entry:{kind:"class", symbol:{id:classesSeed}}, hash:"#classes"};
  if (visible === "deviations") return viewEntry(revision, visible);
  return {entry:{kind:"view", view:visible}, hash:""};
}
function rememberVisible(revision) { writeEntry(visibleEntry(revision)); }
// The tab on screen, as the shell last showed it.
let shownView = "architecture";
function currentDiagramView() { return shownView; }
// The note says the agent moved this view. Back returns to what was shown before; a request
// from another snapshot instead offers to reload at head and show it.
function showAgentNote(text, staleRequest, backable) {
  const key = backable ? window.history?.state?.trellisNavigation : null;
  agentShown = {key, request: staleRequest};
  $("agent-shown-label").textContent = text;
  $("agent-shown-back").hidden = !key;
  $("agent-shown-reload").hidden = !staleRequest;
  $("agent-shown").hidden = false;
}
function hideAgentNote() {
  agentShown = null;
  $("agent-shown").hidden = true;
}
function renderFollowPin() {
  $("follow-agent").textContent = followPinned ? "Pinned" : "Following agent";
  $("follow-agent").title = followPinned ? "Pinned: the agent's trellis_show does not move this view. Select to follow it again."
    : "Following: the agent's trellis_show moves this view. Select to pin it.";
}
