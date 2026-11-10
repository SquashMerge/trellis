"use strict";
// Detected and authored apps in the Architecture view (#789, #794): the groups the switcher
// offers and the per-app slice of a summary. Pure and DOM-free; the controller draws and labels. An app is
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
      name, apps, duplicate:apps.length > 1, authored:apps.some(app => app.source === "override"),
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
  const count = value => Number.isSafeInteger(value) && value > 0 ? value : 0;
  // The interface between two apps, by their names.
  const link = (view, client, server) => (Array.isArray(view?.interfaces) ? view.interfaces : []).find(item => record(item) && item.client === client && item.server === server) || null;
  // The stubs of one band of app `name`: an end of an interface it is the client or server of, whose
  // end layer is that band. A client's routes add up across its ends (a route has one handler layer).
  // A server band's ends may share a route, so several ends give an upper-bounded maximum.
  function stubs(view, name, layer) {
    const found = [];
    for (const item of Array.isArray(view?.interfaces) ? view.interfaces : []) {
      if (!record(item)) continue;
      const ends = Array.isArray(item.ends) ? item.ends.filter(record) : [];
      for (const side of ["client", "server"]) {
        if (item[side] !== name) continue;
        const own = ends.filter(end => end[`${side}Layer`] === layer);
        if (!own.length) continue;
        const routes = own.map(end => count(end.routes));
        found.push({side, client:item.client, server:item.server, atLeast:side === "server" && own.length > 1,
          routes:side === "client" ? routes.reduce((sum, value) => sum + value, 0) : Math.max(...routes),
          callSites:own.reduce((sum, end) => sum + count(end.callSites), 0)});
      }
    }
    return found;
  }

  // ---- All apps (#790): pure layout, card text and overlay routing. The controller draws and measures. ----
  const COLUMN_MIN = 352, GUTTER = 176, ARROW = 8, LANE_TOP = 8, LANE_STEP = 6;
  const plural = (n, word) => `${n} ${word}${n === 1 ? "" : "s"}`;
  // Columns sit side by side when they all fit at their least width with a gutter between each; else they stack.
  // `natural` is each drawing's own measured width: a column never narrows below it, and whatever then
  // overflows the pane scrolls.
  function layout({width, count, natural = []}) {
    const n = Number.isSafeInteger(count) && count > 0 ? count : 1, wide = Number.isFinite(width) ? width : 0;
    const sideBySide = n > 1 && wide >= n * COLUMN_MIN + (n - 1) * GUTTER;
    const base = sideBySide ? Math.floor((wide - (n - 1) * GUTTER) / n) : Math.max(COLUMN_MIN, Math.floor(wide));
    const widths = Array.from({length:n}, (_, index) => Math.max(base, Number.isFinite(natural[index]) ? natural[index] : 0));
    if (!sideBySide) { const all = Math.max(...widths); return {sideBySide, gutter:GUTTER, count:n, widths:widths.map(() => all), columns:widths.map(() => ({x:0, width:all})), width:all}; }
    let x = 0;
    const columns = widths.map(each => { const column = {x, width:each}; x += each + GUTTER; return column; });
    return {sideBySide, gutter:GUTTER, count:n, widths, columns, width:x - GUTTER};
  }
  // Non-zero evidence counts of the call sites, as the DTO reports them.
  function confidenceText(counts) {
    const confidence = record(counts?.confidence) ? counts.confidence : {};
    return [["exact", "exact"], ["template", "template"], ["wrapperInferred", "wrapper-inferred"], ["unresolved", "unresolved"]]
      .filter(([key]) => count(confidence[key]) > 0).map(([key, word]) => `${count(confidence[key])} ${word}`).join(" · ");
  }
  const layerNames = (entry, key) => {
    const names = unique((Array.isArray(entry?.ends) ? entry.ends : []).filter(record).map(end => typeof end[key] === "string" ? end[key] : "no layer"));
    return names.length ? names.join(", ") : "no layer";
  };
  // The card's visible lines: every number is the DTO's, never recounted from sites.
  function cardText(entry) {
    const counts = record(entry?.counts) ? entry.counts : {}, lines = [{text:`HTTP · ${plural(count(counts.matchedRoutes), "route")}`, strong:true},
      {text:plural(count(counts.callSites), "call site")}];
    const confidence = confidenceText(counts);
    if (confidence) lines.push({text:confidence});
    if (count(counts.serverOnly)) lines.push({text:`◆ ${count(counts.serverOnly)} server-only`, tone:"warning"});
    if (count(counts.clientOnly)) lines.push({text:`◆ ${count(counts.clientOnly)} client-only`, tone:"warning"});
    return lines;
  }
  // The card's full accessible name, a sentence that ends in Selected while it is selected.
  function cardName(entry, selected = false) {
    const counts = record(entry?.counts) ? entry.counts : {}, confidence = confidenceText(counts);
    return `HTTP interface from ${entry.client} layer ${layerNames(entry, "clientLayer")} to ${entry.server} layer ${layerNames(entry, "serverLayer")}: ` +
      `${plural(count(counts.matchedRoutes), "matched route")}, ${plural(count(counts.callSites), "client call site")}, ` +
      `${plural(count(counts.serverOnly), "server-only route")}, ${plural(count(counts.clientOnly), "client-only call")}` +
      `${confidence ? `; evidence ${confidence}` : ""}${selected ? ". Selected" : ""}`;
  }
  // One line per end for the ends no line can show: both apps and layers, in words.
  const endsText = (entry, ends = endsOf(entry)) => ends.map(end =>
    `${entry.client} · ${typeof end.clientLayer === "string" ? end.clientLayer : "no layer"} → ${entry.server} · ${typeof end.serverLayer === "string" ? end.serverLayer : "no layer"}`);
  // A column header's two lines. `layered` false: the app has no ordered layer, so it says so.
  function header(item, {max, metric, layered}) {
    const deviates = count(item.counts?.deviates), parts = [item.languages.join(", ") || "no languages", item.roots.map(root => root || "./").join(", ") || "no roots", plural(item.fileCount, "file")];
    if (!layered) return {name:item.name, detail:"no layers yet", deviations:null, manifests:item.duplicate ? manifestList(item) : null};
    if (max) parts.push(`${max} (${metric})`);
    return {name:item.name, detail:parts.join(" · "), deviations:`${plural(deviates, "deviation")}`, manifests:item.duplicate ? manifestList(item) : null};
  }
  // Orthogonal lines for the ends that run between adjacent side-by-side columns. `columns` maps an app name to its
  // column index and its bands' frames, already in the grid's coordinates. Each end has its own lane in its
  // gutter; an end whose layer is not a drawn band gets no line and is left to the card's words.
  function routes(view, plan, columns, edgeWidth) {
    const placed = [], lanes = new Map();
    const entries = (Array.isArray(view?.interfaces) ? view.interfaces : []).filter(record);
    if (!plan.sideBySide) return {lines:[], undrawn:new Map(entries.map(entry => [entry, endsOf(entry)]))};
    const undrawn = new Map();
    for (const entry of entries) {
      const client = columns.get(entry.client), server = columns.get(entry.server), gone = [];
      for (const end of endsOf(entry)) {
        const from = client?.bands.get(end.clientLayer), to = server?.bands.get(end.serverLayer);
        if (!client || !server || Math.abs(client.index - server.index) !== 1 || !from || !to) { gone.push(end); continue; }
        const gutter = Math.min(client.index, server.index);
        placed.push({entry, end, from, to, gutter, rightward:client.index < server.index});
        lanes.set(gutter, (lanes.get(gutter) || 0) + 1);
      }
      undrawn.set(entry, gone);
    }
    const used = new Map(), lines = placed.map(item => {
      const slot = used.get(item.gutter) || 0; used.set(item.gutter, slot + 1);
      const left = plan.columns[item.gutter].x + plan.columns[item.gutter].width, laneX = left + Math.round(plan.gutter * (slot + 1) / (lanes.get(item.gutter) + 1));
      const y0 = Math.min(item.from.y + item.from.h - 4, item.from.y + LANE_TOP + LANE_STEP * slot);
      const y1 = Math.min(item.to.y + item.to.h - 4, item.to.y + LANE_TOP + LANE_STEP * slot);
      const x0 = item.rightward ? item.from.x + item.from.w : item.from.x, x1 = item.rightward ? item.to.x : item.to.x + item.to.w, d = item.rightward ? 1 : -1;
      return {entry:item.entry, gutter:item.gutter, laneX, width:edgeWidth(count(item.end.callSites)),
        path:`M${x0},${y0} L${laneX},${y0} L${laneX},${y1} L${x1 - d * ARROW},${y1}`,
        arrow:`${x1},${y1} ${x1 - d * ARROW},${y1 - ARROW / 2} ${x1 - d * ARROW},${y1 + ARROW / 2}`, y:(y0 + y1) / 2};
    });
    return {lines, undrawn};
  }
  // Which side of each side-by-side column its Shared bands take (#850; owner decision 2026-11-09).
  // Columns keep manifest order (`names`). Only an end between adjacent columns draws a line, and it
  // leaves its band toward the partner column: an end on an ordered band crosses the Shared bands
  // when they sit on that side, and an end on a Shared band crosses the ordered stack when they sit
  // on the other. Each column takes the side with fewer such crossings; a tie, including a column with
  // no drawn line, keeps the right, as in the one-app view. So with two apps whose interface runs
  // between ordered layers, the left app's Shared band goes left and the right app's goes right.
  // `band(name, layer)` says whether `layer` is a drawn Shared band (true), a drawn ordered band (false)
  // or no band (null) of app `name`. Stacked columns draw no lines and keep the right.
  function sharedSides(view, names, band, sideBySide = true) {
    const at = new Map(names.map((name, index) => [name, index])), cost = names.map(() => ({left:0, right:0}));
    for (const entry of sideBySide && Array.isArray(view?.interfaces) ? view.interfaces.filter(record) : []) {
      const client = at.get(entry.client), server = at.get(entry.server);
      if (client === undefined || server === undefined || Math.abs(client - server) !== 1) continue;
      for (const end of endsOf(entry)) {
        for (const [index, other, layer] of [[client, server, end.clientLayer], [server, client, end.serverLayer]]) {
          const shared = typeof layer === "string" ? band(names[index], layer) : null;
          if (shared === null) continue;
          const toward = other < index ? "left" : "right";
          cost[index][shared === (toward === "left") ? "right" : "left"]++;
        }
      }
    }
    return new Map(names.map((name, index) => [name, cost[index].left < cost[index].right ? "left" : "right"]));
  }
  const endsOf = entry => (Array.isArray(entry?.ends) ? entry.ends : []).filter(record);
  const key = entry => JSON.stringify(["interface", entry.client, entry.server]);
  const label = item => item.languages.length ? `${item.name} · ${item.languages.join(", ")}` : item.name;
  const manifestList = item => item.manifests.join(", ");
  const rootList = item => item.roots.map(root => root || "./").join(", ");
  // Where an app comes from: its manifests, or the rules file for an authored app (#794).
  const origin = item => item.authored ? "Authored in .trellis/rules.toml"
    : item.manifests.length ? `Detected from ${manifestList(item)}${item.kinds.length ? ` (${item.kinds.join(", ")})` : ""}` : "";
  function sourceLine(item, count) {
    if (!item) return `${count} apps · hidden layers and unassigned files grouped below`;
    const from = origin(item);
    return `${item.name} · ${item.roots.length ? `roots ${rootList(item)}` : "no roots"}${from ? ` · ${from}` : ""}`;
  }
  const duplicateNotice = item =>
    `${item.apps.length} detected apps share the name ${item.name} (${manifestList(item)}). A layer cannot name them; rules.md Amendment (#783).`;
  window.TrellisArchitectureApps = {groups, group, scoped, link, stubs, sumCounts, label, origin, sourceLine, duplicateNotice,
    layout, confidenceText, cardText, cardName, endsText, header, routes, sharedSides, key};
})();
