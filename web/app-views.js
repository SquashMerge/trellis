"use strict";
// Section switching, the app switcher, Classes availability and the class entry points. Declarations only; app.js wires them.
// Already in the section: focus what is on screen. In Diagrams that is the tab of the view on screen, never a fixed one (#913).
function focusSection(name, view) {
  const target = name === "files" ? filesBasis === "working" ? $("files-diff") : workspaceSource ? $("source") : $("file-filter") : $(`view-${view || currentDiagramView()}`);
  target?.focus();
}
function interruptOutlineReads() {
  for (const state of fileStates.values()) if (state.outlineLoading) {
    state.serial++; state.outlineLoading = false; state.outlineError = "Symbol read interrupted. Retry to read this indexed outline.";
  }
}
// Leaves the section on screen for `name`. False when there is nothing to enter: an unknown name, no root, or the same section.
function switchSection(name, view) {
  if (!["files", "diagrams"].includes(name) || !selectedRootKey) return false;
  if (section === name) { focusSection(name, view); return false; }
  architecture?.reset();
  treemap?.reset();
  section = name; sectionSerial++; sourceSerial++; filesDiffSerial++;
  interruptOutlineReads();
  window.TrellisNavigation?.reset(); window.TrellisContextMenu?.closeContextMenu?.();
  window.TrellisShell?.setSection(name);
  return true;
}
// The Diagrams surface before its view refreshes. Returns the repaint of the cached source, which runs after the
// refresh and only if this entry is still the one on screen.
function showDiagrams(view) {
  // A navigation names its target view: the last-used tab neither shows nor refreshes (#451).
  if (view) {
    // The shell focused the last-used tab on entry; keep focus on the tab that is now shown.
    const tabFocused = document.activeElement?.closest?.("#diagram-tabs");
    showNavigationView(view);
    if (tabFocused) $(`view-${view}`)?.focus();
  }
  $("source").hidden = false; $("files-diff").hidden = true;
  if (!workspaceSource) window.TrellisShell?.closeSource();
  renderFiles();
  const cached = workspaceSource, surface = sectionSerial, session = epoch, root = selectedRootKey, sourceAtStart = sourceSerial;
  const current = () => section === "diagrams" && surface === sectionSerial && session === epoch && root === selectedRootKey && sourceAtStart === sourceSerial && IndexPin.equal(cached?.data.revision, status?.revision);
  return () => { if (cached && current()) paintCachedSource(cached.data, cached.range, current, cached.range?.startLine || 1, cached.navigation?.snapshot?.()); };
}
// Section changes keep validated evidence but never choose or read a file.
function showFilesSource() {
  viewerMetrics?.clearInspection();
  updateFilesBasis(); renderFiles();
  if (!workspaceSource && filesFile && !filesFile.changeOnly && status?.revision) {
    const cached = sourceCache.get(`${IndexPin.key(status.revision)}:${filesFile.path}`);
    if (cached) workspaceSource = {data:cached, range:filesSymbol?.symbol.range || null};
  }
  if (workspaceSource && filesBasis === "indexed") {
    const source = workspaceSource;
    paintCachedSource(source.data, source.range, filesGuard({basis:true}), source.range?.startLine || 1, source.navigation?.snapshot?.());
  }
}
// What a section change interrupted in Files: the source read, or the working-tree diff.
function showFilesInterrupted() {
  if (!workspaceSource && filesFile && filesBasis === "indexed") $("source-path").textContent = "Source read interrupted. Select the file again to read its cached snapshot.";
  if (filesBasis === "working") {
    if (filesDiff) window.TrellisFiles?.diff($("files-diff"), filesDiff);
    else $("files-diff").textContent = "Diff read interrupted. Select Working tree again to measure it.";
  }
}
async function setSection(name, view = null) {
  if (!switchSection(name, view)) return;
  if (name !== "files") {
    const repaintSource = showDiagrams(view);
    if (!$("treemap-panel").hidden) await treemap?.refresh();
    else if (!$("architecture-panel").hidden) void architecture?.refresh();
    else if (!$("deviations-panel").hidden) void architecture?.showDeviations();
    else if (diagramSequence && selectedMethod?.id === diagramSequence.seed.id) await loadSequence(diagramSequence, true);
    repaintSource();
    return;
  }
  showFilesSource();
  if (filesSymbol) {
    const current = filesGuard({symbol:true});
    if (filesMatches) renderFilesMatches(); else void loadFilesMatches();
    if (filesMini) await renderFilesMini(current);
    else if (["function", "method"].includes(filesSymbol.symbol.kind)) void loadFilesMini(current);
  }
  showFilesInterrupted();
  if (!filesCatalogLoaded && status?.revision) await loadFiles(true);
  if (!filesGitStatus) await refreshGit();
}
// ---- The app switcher of Treemap, Classes and the Files tree (#852). ----
// One page-wide choice, shared with Architecture: All apps or one app. It is memory only and a new
// checkout starts at All apps. The apps and each file's app come from the Architecture summary at
// the shown pin and rules (the assignment the rules check uses); its read is the cached one
// Architecture makes. Without a ready summary of two or more apps there is no switcher and every
// view shows the whole checkout.
const appInventoryKey = () => token && selectedRootKey && status?.revision ? `${epoch}:${selectedRootKey}:${IndexPin.key(status.revision)}:${rulesEpoch}` : null;
// The assignment of the shown pin and rules; null while it is read, or when there is none.
const appAssignment = () => appInventory.key !== null && appInventory.key === appInventoryKey() ? appInventory.assigned : null;
// The chosen app at this checkout, or null for All apps (also when the assignment no longer has it).
const chosenApp = () => Apps ? Apps.chosen(appAssignment(), appSelection.get(selectedRootKey)) : null;
// A view's file test: null shows every file.
function appFileFilter() {
  const assigned = appAssignment(), name = chosenApp();
  return name === null ? null : path => Apps.holdsFile(assigned, name, path);
}
// The choice as a view holds it: null for All apps, else its name, file test and the assignment
// it tests against (so a view can tell a new assignment of the same name from the one it listed).
function appChoice() {
  const holds = appFileFilter();
  return holds && {name:chosenApp(), holds, assigned:appAssignment()};
}
// Show an indexed path: its own app, or All apps when it is in none.
function showAppOf(path) {
  const assigned = appAssignment();
  if (assigned) appSelection.set(selectedRootKey, Apps.chosen(assigned, Apps.appOf(assigned, path)));
}
function appFolderFilter() {
  const assigned = appAssignment(), name = chosenApp();
  return name === null ? null : path => Apps.holdsFolder(assigned, name, path);
}
const APP_SWITCHERS = ["treemap-apps", "classes-apps", "files-apps", "deviations-apps"];
function drawAppSwitchers() {
  const assigned = appAssignment(), name = chosenApp();
  for (const id of APP_SWITCHERS) {
    const target = $(id);
    if (!target) continue;
    const refocus = !target.hidden && !!target.contains?.(document.activeElement);
    const choices = Apps ? Apps.choices(assigned) : [];
    target.replaceChildren(); target.hidden = !choices.length;
    for (const choice of choices) {
      const pick = element("button", choice.text); pick.type = "button";
      pick.setAttribute("aria-pressed", String(choice.name === name));
      pick.addEventListener("click", () => chooseApp(choice.name));
      target.append(pick);
      if (refocus && choice.name === name) pick.focus?.();
    }
  }
}
function chooseApp(name) {
  if (!appSelection || !appAssignment() || Apps.chosen(appAssignment(), name) !== name || name === chosenApp()) return;
  appSelection.set(selectedRootKey, name);
}
// Every view follows a new choice or a new assignment: the switchers, the Files tree, Treemap,
// Classes' list and Architecture. Only a listed class search reads, since its list is paged.
function appScopeChanged() {
  drawAppSwitchers();
  if (treeMode || files.length || filesCatalogLoaded || $("file-filter").value.trim()) renderFiles();
  treemap?.scopeChanged();
  window.TrellisClasses?.scopeChanged?.();
  void architecture?.scopeChanged();
}
function appPause(delay) { return new Promise(resolve => setTimeout(resolve, delay)); }
async function refreshAppInventory() {
  const key = appInventoryKey();
  if (!Apps || key === appInventory.key) return;
  const had = appInventory.key !== null && appInventory.assigned !== null;
  appInventory = {key, assigned:null};
  if (had) appScopeChanged(); else drawAppSwitchers();
  if (key === null) return;
  const pin = IndexPin.copy(status.revision), current = () => appInventoryKey() === key && appInventory.key === key;
  const query = new URLSearchParams({aggregate:"max", role:"production", workbench:"true", indexGeneration:pin.indexGeneration, indexRevision:String(pin.indexRevision)});
  let assigned = null;
  try {
    for (;;) {
      const payload = (await architectureRequest(`/api/architecture?${query}`, current))?.payload;
      if (!current()) return;
      if (!payload || !IndexPin.equal(payload, pin)) return;
      if (payload.state !== "building") { assigned = Apps.assignment(payload); break; }
      await appPause(Number.isSafeInteger(payload.retryAfterMs) && payload.retryAfterMs > 0 ? payload.retryAfterMs : 250);
      if (!current()) return;
    }
  } catch (_) {
    // No summary (no rules file, rejected rules, a failed read): no switcher. Architecture reports why.
    return;
  }
  if (!current()) return;
  appInventory = {key, assigned};
  // As in Architecture, a summary that cannot show the chosen app ends the choice: it never returns with the apps.
  const held = appSelection.get(selectedRootKey);
  if (held !== null && Apps.chosen(assigned, held) === null) { appSelection.set(selectedRootKey, null); return; }
  if (assigned) appScopeChanged();
}
// Classes is shown only when the checkout's application layers have production code in a
// language the class catalog extracts (#450, #524): the workbench metrics projection's
// stored roles (which leave out fixtures and authored hidden layers such as tooling)
// joined with the catalog's supportedLanguages, read once per pin after that code is known.
// Unknown (loading or failed) keeps the checkout's last answer, and with none hides the tab;
// explicit class menus still work. That answer arrives only after metrics, which can take
// seconds, so a new revision holds the tab steady and this browser remembers it for reloads (#559).
// A stale hint costs little: Classes still opens and says what it finds.
let classSupport = null, classTab = null;
const classTabKey = root => `trellis.classes.tab.v1:${root}`;
function lastClassTab() {
  if (!selectedRootKey) return false;
  if (classTab?.root !== selectedRootKey) {
    let saved = null;
    try { saved = localStorage.getItem(classTabKey(selectedRootKey)); } catch (_) { /* No hint; the answer still comes. */ }
    classTab = {root:selectedRootKey, available:saved === "true"};
  }
  return classTab.available;
}
function rememberClassTab(available) {
  if (!selectedRootKey || available === lastClassTab()) return;
  classTab = {root:selectedRootKey, available};
  try { localStorage.setItem(classTabKey(selectedRootKey), String(available)); } catch (_) { /* This page still holds it. */ }
}
async function refreshClassSupport(pin) {
  const guard = browseGuard(), attempt = {pin, languages:null};
  classSupport = attempt;
  try {
    const query = new URLSearchParams({q:"", limit:"1", indexGeneration:pin.indexGeneration, indexRevision:String(pin.indexRevision)});
    const data = await api(`/api/classes?${query}`);
    if (classSupport !== attempt) return;
    // A superseded read is forgotten so the next projection asks again; a real failure
    // keeps the null record for this pin, which bounds requests.
    if (!guard()) { classSupport = null; return; }
    if (!IndexPin.equal(data?.revision, pin) || !Array.isArray(data.supportedLanguages)) return;
    attempt.languages = new Set(data.supportedLanguages.filter(language => typeof language === "string"));
  } catch (error) {
    if (error?.name === "AbortError" && classSupport === attempt) classSupport = null;
    return;
  }
  updateViewAvailability();
}
function updateViewAvailability() {
  const languages = viewerMetrics?.productionLanguages?.(), pin = status?.revision;
  const known = classSupport && IndexPin.equal(classSupport.pin, pin);
  if (languages?.size && pin && token && !known) void refreshClassSupport(IndexPin.copy(pin));
  const supported = known ? classSupport.languages : null;
  if (languages && (!languages.size || supported)) rememberClassTab([...supported || []].some(language => languages.has(language)));
  window.TrellisShell?.setViewAvailable?.("classes", lastClassTab());
}
// Class diagrams are an independent, cached-source projection. They never infer
// a sequence participant's type or call a provider.
function classLanguage(path) { return /\.(?:java|py|js|mjs|cjs|rs|ts|tsx|mts|cts)$/i.test(path || ""); }
async function openClasses(options = {}) {
  architecture?.clearInspection();
  treemap?.clearInspection();
  if (!token || !status || !window.TrellisClasses) return;
  viewerMetrics?.clearInspection();
  clearSource(); window.TrellisShell?.resetInspector();
  showNavigationView("classes");
  classesSeed = options.seed || null;
  return window.TrellisClasses.open(options.seed ? {seed:options.seed} : options);
}
function attachClassMenu(node, options) {
  if (!classLanguage(options.path) || !window.TrellisContextMenu?.showContextMenu) return;
  const current = browseGuard();
  const show = event => {
    if (!current() || !token || !status) return;
    event.preventDefault();
    window.TrellisContextMenu.showContextMenu(event, [{
      label: options.seed ? "Show enclosing class diagram" : "Show class diagram",
      run: () => { if (current()) return perform(() => openClasses(options)); },
    }]);
  };
  node.addEventListener("contextmenu", show);
  node.addEventListener("keydown", event => {
    if (event.key === "ContextMenu" || (event.key === "F10" && event.shiftKey)) show(event);
  });
}
function inspectSourceSymbol(symbol, revision) {
  if (!window.TrellisSourceWitness(symbol) || !IndexPin.equal(revision, status?.revision)) throw new Error("Source inspection is stale. Refresh the indexed view.");
  const context = navigationContext(), serial = sourceSerial;
  const inspector = {symbol}; sourceInspector = inspector;
  const current = () => navigationCurrent(context) && sourceInspector === inspector && sourceSerial === serial;
  return window.TrellisShell?.inspectSymbol?.(symbol, revision, () => {
    if (current()) return perform(() => section === "files" ? openFilesLocation(symbol, revision) : showSource(symbol, revision));
  });
}
