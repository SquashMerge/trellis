"use strict";
// The Files section: outline, symbol, matches, the mini Sequence, and the indexed or working-tree basis. Declarations only; app.js wires them.
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
  for (const id of ["files-symbol-location", "files-symbol-header", "files-symbol-return", "files-mini-sequence", "files-mini-notes", "files-matches", "files-matches-state"]) $(id)?.replaceChildren();
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
function appendFilesOutline(li, file) {
  const state = fileStates.get(file.path);
  if (!state?.outlineOpen) return;
  const list = element("ul"); li.append(list);
  const guard = filesGuard(), serial = state.serial;
  const current = () => guard() && fileStates.get(file.path) === state && serial === state.serial;
  if (state.outlineLoading) list.append(element("li", "Loading symbols…", "detail"));
  else if (state.outlineError) list.append(button(`Retry symbols · ${state.outlineError}`, () => { if (current()) return loadFilesOutline(file, 0); }));
  else {
    // Types and callables; a container's members show when it is expanded (#526).
    state.outlineExpanded ||= new Set();
    // A newly selected member opens its containers once; the reader may close them again.
    const reveal = !!filesSymbol && state.outlineRevealed !== filesSymbol;
    if (reveal) state.outlineRevealed = filesSymbol;
    const shown = window.TrellisFiles?.outline(list, state.outlineItems || [], filesSymbol, selectFilesSymbol, current, {
      expanded:state.outlineExpanded, reveal,
      toggle:(key, open) => {
        if (open) state.outlineExpanded.add(key); else state.outlineExpanded.delete(key);
        renderFiles();
        const pick = Array.from($("file-tree").querySelectorAll(".files-symbol-pick")).find(node => node.getAttribute("data-symbol-id") === key);
        pick?.parentElement?.querySelector(".files-symbol-toggle")?.focus();
      },
    });
    if (!state.outlineItems?.length) list.append(element("li", "No measured symbols. Cached source remains readable.", "detail"));
    else if (!shown) list.append(element("li", "No types or callables. Cached source remains readable.", "detail"));
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
// Paints a pinned source location in Files; true only when it painted.
async function openFilesLocation(item, pin, callerCurrent = () => true) {
  if (!IndexPin.equal(pin, status?.revision) || section !== "files" || !callerCurrent()) return false;
  filesSerial++; filesDiffSerial++; clearFilesInspector(); filesFile = files.find(file => file.path === item.path) || {path:item.path}; filesBasis = "indexed"; filesDiff = null;
  $("files-empty").hidden = true; $("files-symbol-title").textContent = "Pinned source location";
  $("files-symbol-location").textContent = `${item.path}:${item.range.startLine} · indexed revision ${IndexPin.label(pin)}`;
  updateFilesBasis(); renderFiles();
  const guard = filesGuard({symbol:true, basis:true}), current = () => guard() && callerCurrent();
  try { return await showCachedSource(item.path, item.range, pin, current) === true; }
  catch (error) { filesFailure(error, "source-path", current); return false; }
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
  const data = filesMini, revision = data.revision, expanded = window.TrellisSequence.hopGroups(data), container = $("files-mini-sequence");
  const readSource = step => { if (current()) return openFilesLocation(step, revision); };
  const options = {showDetails:false, isCurrent:current, onOpenTarget:sequenceTargetAction(data, current)};
  options.onUnsupported = reason => { if (current()) { window.TrellisSequence.renderError(container, reason); $("files-mini-state").textContent += ` · This Sequence could not be drawn: ${reason}`; } };
  const empty = window.TrellisSequence.emptySummary(data, options);
  const onlyEmpty = empty && !window.TrellisSequence.display(data, options).visible.size;
  const outcome = onlyEmpty ? {status:"empty"} : window.TrellisPeleSequence ? await window.TrellisPeleSequence.render(container, data, readSource, expanded, options) : {status:"unsupported", reason:"Pele asset unavailable"};
  if (!current() || outcome.status === "stale") return;
  if (outcome.status !== "rendered" && outcome.status !== "empty") window.TrellisSequence.renderError(container, outcome.reason);
  if (empty) window.TrellisSequence.renderEmpty(container, empty, () => { if (current()) void perform(() => navigateDiagram(data.seed, revision, "sequence", null, true)); }, !onlyEmpty);
  // One short status line; the sequence's caveats and diagnostics live behind the ⓘ (#526).
  const notes = [...(data.warnings || [])];
  if (data.truncated) notes.push("Truncated by sequence limits; Open full loads the sequence again.");
  if (outcome.status !== "rendered" && !empty) notes.push(`This Sequence could not be drawn: ${outcome.reason}`);
  $("files-mini-notes").replaceChildren(...notes.map(note => element("li", note, "detail")));
  $("files-mini-state").textContent = `Indexed revision ${IndexPin.label(revision)} · ${data.hiddenSteps} incidental steps hidden${data.truncated ? " · truncated" : ""}${!data.steps.length ? " · No visible behavior steps" : ""}${notes.length ? ` · ${notes.length} ${notes.length === 1 ? "note" : "notes"} in ⓘ` : ""}`;
  fitFilesMini();
  $("files-open-full").disabled = false;
}
// The preview scales down to the inspector's width (#451). Text that overflows the drawing's
// own box is included so nothing clips; when the smallest text as actually rendered would be
// under FILES_MINI_MIN_TEXT_PX, the preview says so instead of drawing it (#514).
const FILES_MINI_MIN_TEXT_PX = 9;
// The smallest rendered text size: each label's font size times its own screen scale.
function smallestTextPx(svg) {
  let smallest = Infinity;
  const labels = [...svg.querySelectorAll("text, foreignObject")].slice(0, 2000);
  for (const label of labels) {
    const styled = label.tagName.toLowerCase() === "foreignobject" ? label.querySelector("*") : label;
    const matrix = label.getScreenCTM?.(), size = styled ? parseFloat(getComputedStyle(styled).fontSize) : NaN;
    if (!matrix || !Number.isFinite(size) || !(label.textContent || "").trim()) continue;
    smallest = Math.min(smallest, size * Math.hypot(matrix.a, matrix.b));
  }
  return smallest;
}
function fitFilesMini() {
  const container = $("files-mini-sequence"), svg = container?.querySelector?.(":scope > svg");
  const floor = $("files-mini-floor");
  // Measure the drawing as it lays out in the inspector: shown, at its scaled size.
  container?.removeAttribute?.("data-fit");
  if (!svg || !container.clientWidth || typeof svg.getScreenCTM !== "function") { if (floor) floor.hidden = true; return; }
  const view = svg.viewBox?.baseVal;
  if (view?.width > 0 && view.height > 0 && typeof svg.getBBox === "function") {
    try {
      const drawn = svg.getBBox(), left = Math.min(view.x, drawn.x), top = Math.min(view.y, drawn.y);
      const right = Math.max(view.x + view.width, drawn.x + drawn.width), bottom = Math.max(view.y + view.height, drawn.y + drawn.height);
      if (left < view.x || top < view.y || right > view.x + view.width || bottom > view.y + view.height) {
        svg.setAttribute("viewBox", `${left} ${top} ${right - left} ${bottom - top}`);
        svg.setAttribute("width", String(right - left)); svg.setAttribute("height", String(bottom - top));
      }
    } catch (_) { /* An unrendered drawing keeps its own box. */ }
  }
  const smallest = smallestTextPx(svg);
  const unreadable = smallest < FILES_MINI_MIN_TEXT_PX;
  container.setAttribute("data-fit", unreadable ? "floor" : "scaled");
  if (Number.isFinite(smallest)) container.setAttribute("data-text-px", smallest.toFixed(2)); else container.removeAttribute("data-text-px");
  if (floor) floor.hidden = !unreadable;
}
async function openFilesFull() {
  if (section !== "files" || !filesMini || !filesSymbol || !IndexPin.equal(filesMini.revision, status?.revision)) return;
  // A mini cut by the visible step bound is not what Interactions draw (#663): ask again.
  return navigateDiagram(filesSymbol.symbol, filesMini.revision, "sequence", filesMini.truncated ? null : filesMini);
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
