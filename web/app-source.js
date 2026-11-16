"use strict";
// The source pane, Refresh, and external library sources. Declarations only; app.js wires them.
async function showSource(item, revision) {
  if (!window.TrellisSourceWitness(item)) throw new Error("Measured source path and range unavailable.");
  return showCachedSource(item.path, item.range, revision);
}
let workspaceSource = null;
function paintCachedSource(data, range, current, startLine = range?.startLine || 1, metadata) {
  workspaceSource = {data, range};
  $("source-path").textContent = `${data.file.path} · ${data.file.language} · ${data.file.text.split("\n").length} lines · revision ${IndexPin.label(data.revision)} · cached snapshot`;
  const fragment = document.createDocumentFragment();
  const lastLine = range && range.endLine > range.startLine && range.endColumn === 1 ? range.endLine - 1 : range?.endLine;
  const lines = data.file.text.split("\n"), syntax = section === "files" ? window.TrellisFiles?.lineClass : null;
  lines.forEach((text, index) => {
    const number = index + 1, line = element("span", undefined, "source-line");
    if (range && number >= range.startLine && number <= lastLine) line.classList.add("highlight");
    const kind = syntax?.(data.file.language, text, lines.length);
    if (kind) line.classList.add(kind);
    line.append(element("span", String(number), "line-number"), document.createTextNode(text)); fragment.append(line);
  });
  $("source").replaceChildren(fragment); $("source").hidden = false;
  window.TrellisShell?.showSource("workspace");
  const binding = window.TrellisNavigation?.attachSource($("source"), {
    path:data.file.path, revision:data.revision, startLine, metadataSnapshot:metadata,
    isCurrent: () => current() && !!token && IndexPin.equal(status?.revision, data.revision) &&
      !(section === "files" && filesBasis !== "indexed") && !$("source-dock")?.hidden && !$("workspace-source-panel")?.hidden,
  });
  workspaceSource.navigation = binding;
  return binding;
}
async function showCachedSource(path, range, revision, callerCurrent = () => true) {
  navigationSerial++;
  IndexPin.copy(revision);
  if (!status?.revision || !IndexPin.equal(revision, status.revision)) throw new Error("This source has no current paired snapshot. Refresh the view first.");
  const serial = ++sourceSerial, session = epoch, root = selectedRootKey;
  const current = () => callerCurrent() && serial === sourceSerial && session === epoch && root === selectedRootKey && IndexPin.equal(status?.revision, revision);
  const key = `${IndexPin.key(revision)}:${path}`;
  window.TrellisNavigation?.reset(); workspaceSource = null;
  $("source-path").textContent = `Loading ${path}…`; $("source").replaceChildren();
  const cached = sourceCache.get(key);
  const data = cached || await api(`/api/source?path=${encodeURIComponent(path)}&${IndexPin.query(revision)}`, "GET", undefined, current);
  if (cached && current()) observePinnedHead();
  if (!current()) return;
  if (!IndexPin.equal(data.revision, revision)) { unexpectedPair("Source snapshot changed. Refresh status.", data.revision); return; }
  if (data.file.path !== path) throw new Error("Source path mismatch. Nothing displayed.");
  sourceCache.set(key, data);
  if (section === "diagrams") {
    // An explicit diagram source read replaces the shared source selection.
    filesSerial++; filesDiffSerial++; clearFilesInspector();
    filesFile = files.find(file => file.path === path) || {path}; filesBasis = "indexed"; filesDiff = null;
  }
  paintCachedSource(data, range, current);
  $("source").focus({preventScroll:true});
  const highlight = $("source").querySelector(".highlight");
  if (highlight) {
    $("source").scrollTop = highlight.offsetTop - $("source").offsetTop - 80;
    highlight.scrollIntoView?.({block:"start", inline:"nearest"});
  }
  return true;
}
async function refreshView() {
  resetHeadObservation(); clearRevisionReads(false);
  const revision = status?.revision, session = epoch, root = selectedRootKey;
  const file = filesFile, basis = filesBasis;
  clearSource(); filesDiffSerial++; filesDiff = null; $("files-diff")?.replaceChildren();
  await refreshGit(); if (session !== epoch || root !== selectedRootKey) return;
  await refreshStatus();
  if (jobUnavailable) {
    // Retrying status also retries the job lookup that failed during selection.
    const lookup = await lookupCurrentJob();
    if (session !== epoch || root !== selectedRootKey) return;
    if (lookup.error) throw lookup.error;
    $("checkout-state").textContent = storageWarning();
  }
  if (IndexPin.equal(revision, status?.revision)) {
    void viewerMetrics?.refresh(); void refreshAppInventory(); await refreshTree();
    if (section === "files" && file) {
      if (file.changeOnly) { filesFile = file; await setFilesBasis("working"); }
      else { await selectFilesFile(file); if (basis === "working") await setFilesBasis("working"); }
    }
  }
  if (section !== "files" && !$("treemap-panel").hidden) await treemap?.refresh();
  // Not awaited: Architecture may be building, and this view shows that itself (#556).
  else if (section !== "files" && (!$("architecture-panel").hidden || !$("deviations-panel").hidden)) void architecture?.refresh();
  // The view on screen is reloaded, never an earlier Sequence behind it (#906).
  else if (section !== "files" && currentDiagramView() === "classes") await window.TrellisClasses?.retry?.();
  else if (section !== "files" && selectedMethod) await loadSequence();
  schedulePoll();
}
// External libraries are candidate definitions, never workspace evidence.
let externalSerial = 0, externalFileSerial = 0, externalSnapshot = null, externalCatalogId = null;
let externalWindowStart = 0, externalSelectedRange = null;
function resetExternalWindow() {
  externalWindowStart = 0; externalSelectedRange = null;
  $("external-source-hint").hidden = true;
  $("external-previous").disabled = true; $("external-next").disabled = true;
}
function clearExternalSources() {
  externalSerial++; externalFileSerial++; externalSnapshot = null; externalCatalogId = null; resetExternalWindow();
  $("external-definitions").replaceChildren();
  $("external-source").replaceChildren(); $("external-warnings").replaceChildren();
  $("external-source-path").textContent = "Choose an indexed definition to read a candidate snapshot.";
  $("external-filter").value = ""; $("external-filter").disabled = true;
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
  $("external-source-hint").textContent = "Workspace diagrams are unavailable for this external candidate snapshot.";
  $("external-source-hint").hidden = false;
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
