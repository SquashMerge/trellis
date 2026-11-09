"use strict";
// Detected apps in the Architecture view (#789): the groups the switcher offers and the
// per-app slice of a summary. Pure and DOM-free; the controller draws and labels. An app is
// known to the browser by its name, so apps that share a name form one group.
(() => {
  const record = value => value !== null && typeof value === "object" && !Array.isArray(value);
  const strings = value => Array.isArray(value) ? value.filter(item => typeof item === "string") : [];
  const unique = items => [...new Set(items)];
  // Sums every numeric leaf of the counts objects. Keys are the daemon's, but the sum object has
  // no prototype so a key can never reach one.
  function add(into, from) {
    for (const [key, value] of Object.entries(record(from) ? from : {})) {
      if (Number.isSafeInteger(value)) into[key] = (into[key] || 0) + value;
      else if (record(value)) add(into[key] ||= Object.create(null), value);
    }
    return into;
  }
  const sumCounts = list => list.reduce((sum, item) => add(sum, item?.counts), Object.create(null));
  // One group per distinct name, in the order of each name's first inventory entry.
  function groups(view) {
    const byName = new Map();
    for (const app of Array.isArray(view?.apps) ? view.apps : []) {
      if (!record(app) || typeof app.name !== "string") continue;
      if (!byName.has(app.name)) byName.set(app.name, []);
      byName.get(app.name).push(app);
    }
    return [...byName].map(([name, apps]) => ({
      name, apps, duplicate:apps.length > 1,
      manifests:strings(apps.map(app => app.manifest)), kinds:unique(strings(apps.map(app => app.kind))),
      roots:unique(apps.flatMap(app => strings(app.roots))), languages:unique(apps.flatMap(app => strings(app.languages))),
      fileCount:apps.reduce((sum, app) => sum + (Number.isSafeInteger(app.fileCount) ? app.fileCount : 0), 0),
      unassignedFileCount:apps.reduce((sum, app) => sum + (Number.isSafeInteger(app.unassignedFileCount) ? app.unassignedFileCount : 0), 0),
      counts:sumCounts(apps)}));
  }
  const group = (view, name) => groups(view).find(item => item.name === name) || null;
  // The view an app draws: its own ordered layers, the shared layers that serve it (`apps` null
  // means every app), the modules in them plus the app's unassigned files, and the edges
  // between those layers. Layer order is each layer's own, so it restarts per app.
  function scoped(view, name) {
    const layers = view.layers.filter(layer => layer.shared ? layer.apps == null || strings(layer.apps).includes(name) : layer.app === name);
    const names = new Set(layers.map(layer => layer.name));
    return {...view, layers,
      modules:view.modules.filter(module => names.has(module.layer) || (module.layer === null && module.app === name)),
      edges:view.edges.filter(edge => names.has(edge.sourceLayer) && names.has(edge.destinationLayer))};
  }
  const label = item => item.languages.length ? `${item.name} · ${item.languages.join(", ")}` : item.name;
  const manifestList = item => item.manifests.join(", ");
  const rootList = item => item.roots.map(root => root || "./").join(", ");
  function sourceLine(item, count) {
    if (!item) return `${count} apps · hidden layers and unassigned files grouped below`;
    const from = item.manifests.length ? ` · Detected from ${manifestList(item)}${item.kinds.length ? ` (${item.kinds.join(", ")})` : ""}` : "";
    return `${item.name} · ${item.roots.length ? `roots ${rootList(item)}` : "no roots"}${from}`;
  }
  const duplicateNotice = item =>
    `${item.apps.length} detected apps share the name ${item.name} (${manifestList(item)}). A layer cannot name them; rules.md Amendment (#783).`;
  window.TrellisArchitectureApps = {groups, group, scoped, sumCounts, label, sourceLine, duplicateNotice};
})();
