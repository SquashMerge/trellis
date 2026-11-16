"use strict";
// The automatic library catalog and in-context Open in library. Declarations only; app.js wires them.
// Metadata-only automatic library catalog. No source or provider request is implicit.
let dependencyStatusSerial = 0, dependencySerial = 0, dependencySymbolsSerial = 0;
let dependencyCatalog = null, dependencyPackage = null, dependencyOffset = 0, dependencyNext = null;
let dependencyQuery = "", dependencyChoice = null, dependencyLoad = null;
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
  dependencyStatusSerial++; dependencySerial++; dependencyCatalog = null; dependencyChoice = null; dependencyLoad = null;
  clearDependencySelection();
  $("dependency-packages").replaceChildren(); $("dependency-warnings").replaceChildren();
  $("dependency-state").textContent = "Open a library from an external call or reference.";
}
function dependencyGuard() {
  const session = epoch, serial = dependencySerial, revision = status?.revision && IndexPin.copy(status.revision), workspace = status?.workspaceRoot;
  return () => session === epoch && serial === dependencySerial && IndexPin.equal(revision, status?.revision) && workspace === status?.workspaceRoot;
}
// One catalog read per pin, made when an in-context "Open in library" first needs it (#527).
// A catalog that is still building, or a read that failed, is read again by the next request.
function ensureDependencies() {
  if (!token || !status?.revision) return Promise.resolve();
  if (!dependencyLoad) {
    const load = dependencyLoad = refreshDependencies().finally(() => {
      load.settled = true;
      // Only a catalog that is still building is read again; ready, disabled and failed are final for the pin.
      if (dependencyLoad === load && (!dependencyCatalog || dependencyCatalog.state === "loading")) dependencyLoad = null;
    });
  }
  return dependencyLoad;
}
async function refreshDependencies() {
  if (!token || !status) return;
  const serial = ++dependencyStatusSerial, guard = dependencyGuard();
  const current = () => guard() && serial === dependencyStatusSerial;
  try {
    const data = await api("/api/dependencies");
    if (!current()) return;
    if (!IndexPin.equal(data.workspaceRevision, status.revision)) { unexpectedPair(undefined, data.workspaceRevision); throw new Error("Library catalog belongs to another workspace revision. Refresh workspace status."); }
    if (!["disabled", "loading", "ready", "failed"].includes(data.state)) throw new Error("Library catalog status unavailable.");
    const catalogChanged = dependencyCatalog?.catalogId !== data.catalogId;
    if (catalogChanged || data.state !== "ready") {
      dependencySerial++; clearDependencySelection();
    }
    if (catalogChanged && dependencyChoice) { dependencyChoice = null; $("dependency-state").textContent = "Library catalog changed. Open the library again from its call or reference."; }
    dependencyCatalog = data;
    $("dependency-warnings").replaceChildren();
    for (const warning of data.warnings || []) $("dependency-warnings").append(element("li", warning, "warning"));
    renderDependencyPackages();
  } catch (error) {
    if ((!current() && !/Library catalog belongs/.test(error.message)) || error.name === "AbortError") return;
    const wasOpen = !!dependencyChoice;
    clearDependencyCatalog();
    if (wasOpen) $("dependency-state").textContent = `Library catalog unavailable. ${error.message} Refresh status (⋯ → Refresh) to retry.`;
  }
}
function renderDependencyPackages() {
  $("dependency-packages").replaceChildren();
  // Only the packages an in-context reference matched; several versions need a choice.
  for (const pkg of dependencyChoice || []) {
    const li = element("li"), pick = externalButton(`${pkg.name} ${pkg.version}`, () => selectDependencyPackage(pkg));
    pick.disabled = dependencyCatalog.state !== "ready" || !dependencyCatalog.catalogId;
    pick.setAttribute("aria-pressed", String(dependencyPackage?.id === pkg.id));
    if (dependencyPackage?.id === pkg.id) pick.className = "selected";
    pick.append(element("span", `${pkg.ecosystem} · ${pkg.source} · source: ${pkg.sourceState} · index: ${pkg.indexState}`, "detail"));
    li.append(pick);
    // Package notes stay folded so the definitions remain in view.
    if (pkg.aliases?.length || pkg.warnings?.length) {
      const notes = element("details");
      notes.append(element("summary", `Package notes (${(pkg.warnings || []).length})`));
      if (pkg.aliases?.length) notes.append(element("p", `Aliases: ${pkg.aliases.join(", ")}`, "detail"));
      for (const warning of pkg.warnings || []) notes.append(element("p", warning, "detail"));
      li.append(notes);
    }
    $("dependency-packages").append(li);
  }
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
    if (data.catalogId !== catalogId || data.items.some(item => item.packageId !== pkg.id)) throw new Error("Library definition provenance mismatch. Refresh status (⋯ → Refresh), then open the library again.");
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
    $("dependency-symbol-state").textContent = `${error.message} Use Filter to retry, or refresh status (⋯ → Refresh) and open the library again if the catalog changed.`;
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
      const message = `${error.message} Refresh status (⋯ → Refresh), then open the library again from its call or reference.`;
      $("external-source-path").textContent = message; $("dependency-symbol-state").textContent = message;
    }
  }
}
// In-context library entry (#450). A reference or external call names a package; the
// catalog offers it only when that exact name (or Cargo alias) is indexed and ready.
const libraryEcosystems = {rust:"cargo", cargo:"cargo"};
function libraryPackages(reference) {
  const ecosystem = libraryEcosystems[reference?.ecosystem], name = reference?.name;
  if (!ecosystem || typeof name !== "string" || !name || dependencyCatalog?.state !== "ready" || !dependencyCatalog.catalogId) return [];
  const crate = value => value.replaceAll("-", "_");
  const matches = dependencyCatalog.packages.filter(pkg => pkg.ecosystem === ecosystem && ["complete", "partial"].includes(pkg.indexState) &&
    (pkg.name === name || crate(pkg.name) === crate(name) || (pkg.aliases || []).includes(crate(name))));
  const exact = reference.version ? matches.filter(pkg => pkg.version === reference.version) : [];
  return exact.length ? exact : matches;
}
function libraryAction(reference) {
  const packages = libraryPackages(reference);
  if (!packages.length) return null;
  const catalogId = dependencyCatalog.catalogId, guard = dependencyGuard();
  return () => {
    if (!guard() || dependencyCatalog?.catalogId !== catalogId) return;
    return perform(() => openLibrary(packages, reference));
  };
}
async function openLibrary(packages, reference) {
  dependencyChoice = packages;
  clearDependencySelection();
  $("dependency-state").textContent = packages.length === 1
    ? `${packages[0].name} ${packages[0].version} · source ${packages[0].sourceState} · index ${packages[0].indexState} · from ${reference.origin}`
    : `${packages.length} indexed packages match ${reference.name} · from ${reference.origin}. Choose a version.`;
  $("dependency-symbol-state").textContent = packages.length === 1 ? "" : "Select a package version.";
  renderDependencyPackages();
  window.TrellisShell?.showSource("library");
  if (packages.length === 1) await selectDependencyPackage(packages[0]);
}
// Before the catalog is read, an empty slot holds the place of the button; it gains the
// button only if the loaded catalog indexes the package.
function libraryButton(reference) {
  // A catalog read for this pin since the last Refresh answers at once; otherwise read it first.
  if (dependencyCatalog?.state === "ready" && !dependencyCatalog.stale) return libraryNode(reference);
  if (!libraryEcosystems[reference?.ecosystem] || typeof reference?.name !== "string" || !reference.name || !status?.revision) return null;
  const slot = element("span", undefined, "library-slot"), session = epoch, pin = IndexPin.copy(status.revision);
  void ensureDependencies().then(() => {
    if (session !== epoch || !IndexPin.equal(pin, status?.revision) || !slot.isConnected) return;
    const node = libraryNode(reference);
    if (node) slot.replaceWith(node);
  });
  return slot;
}
function libraryNode(reference) {
  const open = libraryAction(reference);
  if (!open) return null;
  const node = element("button", "Open in library"); node.type = "button";
  node.title = `Indexed declarations of ${reference.name}: syntax candidates, not a resolved target.`;
  node.addEventListener("click", open);
  return node;
}
