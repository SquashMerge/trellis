"use strict";
// The file catalog and the directory tree: browse, filter, hidden entries, reveal and method lists. Declarations only; app.js wires them.
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
  drawAppSwitchers();
  $("index").textContent = "Index workspace";
  fileStates.clear(); closedDirectories.clear(); $("method-source").disabled = true;
  $("file-tree").replaceChildren(); $("sequence-diagram").replaceChildren(); $("sequence-warnings").replaceChildren(); if ($("sequence-incidental")) $("sequence-incidental").hidden = true;
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
  // One chosen app shows its files only (#852); the others are counted, never dropped silently.
  const inApp = appFileFilter(), shownFiles = listed.filter(f => showHiddenFiles() || !dotPath(f.path));
  const visible = inApp ? shownFiles.filter(f => inApp(f.path)) : shownFiles, outside = shownFiles.length - visible.length;
  const appNote = outside ? ` · ${outside} in other apps or no app · All apps shows them` : "";
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
      : `${visible.length} matching indexed ${visible.length === 1 ? "file" : "files"}${appNote}${filterResults.next != null ? " · more available" : ""}`);
    $("more-files").hidden = searching || filterResults.next == null;
    $("more-files").textContent = "Load more matches";
  } else {
    if (!visible.length) tree.append(element("li", files.length ? "No indexed files to show." : "No indexed files yet. Use Index workspace above, then browse the tree.", "detail"));
    $("files-state").textContent = `${visible.length} of ${files.length} loaded files${appNote}${nextFileOffset != null ? " · more available" : ""}`;
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
async function selectMethod(symbol, admitted = null, callerCurrent = null, boundary = null) {
  if (!callerCurrent) {
    const context = navigationContext(), ticket = ++navigationSerial;
    callerCurrent = () => ticket === navigationSerial && navigationCurrent(context);
  }
  if (!callerCurrent()) return;
  sourceInspector = null;
  viewerMetrics?.clearInspection();
  window.TrellisShell?.showView("sequence");
  if ($("method-class")) $("method-class").disabled = !classLanguage(symbol.path);
  selectedMethod = symbol; sequenceBoundary = boundary; $("method-source").disabled = false;
  renderSequenceStart();
  clearSource(); renderFiles();
  await loadSequence(admitted, false, callerCurrent);
  if (callerCurrent() && selectedMethod === symbol && window.matchMedia?.("(max-width: 900px)").matches) $("sequence-title").scrollIntoView?.({block:"start"});
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
  let count = 0, hidden = 0, outside = 0;
  const metricRows = [], showHidden = showHiddenFiles();
  // One chosen app (#852) shows the folders and files in it, by their path in the index workspace.
  // A folder above that workspace stays when the workspace lies below it.
  const inApp = appFileFilter(), folderInApp = appFolderFilter();
  const indexedPathOf = item => item.indexedPath ?? metricDirectorySelection(item.path)?.path ?? null;
  const aboveWorkspace = item => !!treeRoot && !!indexedWorkspace && `${indexedWorkspace.replace(/\/$/, "")}/`.startsWith(`${treeRoot.replace(/\/$/, "")}/${item.path}/`);
  const appHides = item => {
    if (!inApp) return false;
    const path = indexedPathOf(item);
    if (path === null) return !(item.kind === "directory" && aboveWorkspace(item));
    return item.kind === "directory" ? !folderInApp(path) : !inApp(path);
  };
  const changes = treeRoot === selectedWorkspaceRoot ? gitChangesByPath() : new Map();
  function rows(path, list, anchorPath = path) {
    const state = directories.get(path);
    if (!state) return;
    const unsupported = [];
    for (const item of state.items || []) {
      if (!showHidden && hiddenTreeEntry(item)) { hidden++; continue; }
      if (appHides(item)) { outside++; continue; }
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
  const notes = [hidden ? `${hidden} hidden or unindexed ${hidden === 1 ? "entry" : "entries"} · ⋯ to show` : "",
    outside ? `${outside} ${outside === 1 ? "entry" : "entries"} outside ${chosenApp()} · All apps shows ${outside === 1 ? "it" : "them"}` : ""];
  $("files-state").textContent = notes.filter(Boolean).join(" · ");
}
