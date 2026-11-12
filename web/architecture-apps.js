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
  const COLUMN_MIN = 352, GUTTER_MIN = 48, LANE_MARGIN = 8, CARD = 176, ARROW = 8, LANE_TOP = 8, LANE_STEP = 6;
  const plural = (n, word) => `${n} ${word}${n === 1 ? "" : "s"}`;
  // A gutter is as wide as its lanes need (#949; owner ruling 2026-11-12): a lane per drawn end, an arrowhead's
  // room on each side and a margin, and never under 48px.
  const gutterWidth = lanes => Math.max(GUTTER_MIN, count(lanes) * LANE_STEP + 2 * ARROW + LANE_MARGIN);
  // Columns sit side by side when they all fit at their least width with each gutter between them; else they stack.
  // `lanes[i]` is the number of lines gutter i (after column i) carries. `natural` is each drawing's own measured
  // width: a column never narrows below it, and whatever then overflows the pane scrolls.
  function layout({width, count:columnCount, natural = [], lanes = []}) {
    const n = Number.isSafeInteger(columnCount) && columnCount > 0 ? columnCount : 1, wide = Number.isFinite(width) ? width : 0;
    const gutters = Array.from({length:n - 1}, (_, index) => gutterWidth(lanes[index])), between = gutters.reduce((sum, each) => sum + each, 0);
    const sideBySide = n > 1 && wide >= n * COLUMN_MIN + between;
    const base = sideBySide ? Math.floor((wide - between) / n) : Math.max(COLUMN_MIN, Math.floor(wide));
    const widths = Array.from({length:n}, (_, index) => Math.max(base, Number.isFinite(natural[index]) ? natural[index] : 0));
    if (!sideBySide) { const all = Math.max(...widths); return {sideBySide, gutters:[], count:n, widths:widths.map(() => all), columns:widths.map(() => ({x:0, width:all})), width:all}; }
    let x = 0;
    const columns = widths.map((each, index) => { const column = {x, width:each}; x += each + (gutters[index] || 0); return column; });
    return {sideBySide, gutters, count:n, widths, columns, width:x};
  }
  // A card floats in its gutter only when the gutter is at least a card wide; otherwise it is listed under the grid.
  const cardFits = gutter => Number.isFinite(gutter) && gutter >= CARD;
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
        if (!drawable(client, server, end, (place, layer) => !!place.bands.get(layer))) { gone.push(end); continue; }
        const from = client.bands.get(end.clientLayer), to = server.bands.get(end.serverLayer);
        const gutter = Math.min(client.index, server.index);
        placed.push({entry, end, from, to, gutter, rightward:client.index < server.index});
        lanes.set(gutter, (lanes.get(gutter) || 0) + 1);
      }
      undrawn.set(entry, gone);
    }
    const used = new Map(), lines = placed.map(item => {
      const slot = used.get(item.gutter) || 0; used.set(item.gutter, slot + 1);
      const left = plan.columns[item.gutter].x + plan.columns[item.gutter].width, laneX = left + Math.round(plan.gutters[item.gutter] * (slot + 1) / (lanes.get(item.gutter) + 1));
      const y0 = Math.min(item.from.y + item.from.h - 4, item.from.y + LANE_TOP + LANE_STEP * slot);
      const y1 = Math.min(item.to.y + item.to.h - 4, item.to.y + LANE_TOP + LANE_STEP * slot);
      const x0 = item.rightward ? item.from.x + item.from.w : item.from.x, x1 = item.rightward ? item.to.x : item.to.x + item.to.w, d = item.rightward ? 1 : -1;
      return {entry:item.entry, gutter:item.gutter, laneX, width:edgeWidth(count(item.end.callSites)),
        path:`M${x0},${y0} L${laneX},${y0} L${laneX},${y1} L${x1 - d * ARROW},${y1}`,
        arrow:`${x1},${y1} ${x1 - d * ARROW},${y1 - ARROW / 2} ${x1 - d * ARROW},${y1 + ARROW / 2}`, y:(y0 + y1) / 2};
    });
    return {lines, undrawn};
  }
  // An end draws a line only between adjacent columns, and only when both of its layers are drawn bands
  // (`has(place, layer)`). `routes`, `sharedSides` and `laneCounts` share this test, so a side and a gutter are chosen from drawn lines alone.
  const drawable = (client, server, end, has) => !!client && !!server && Math.abs(client.index - server.index) === 1 &&
    has(client, end.clientLayer) && has(server, end.serverLayer);
  // Which side of each side-by-side column its Shared bands take (#850; owner decision 2026-11-09).
  // Columns are in interface order (`names`, see columnOrder). Only an end between adjacent columns draws a line, and it
  // leaves its band toward the partner column: an end on an ordered band crosses the Shared bands
  // when they sit on that side, and an end on a Shared band crosses the ordered stack when they sit
  // on the other. Each column takes the side with fewer such crossings; a tie, including a column with
  // no drawn line, keeps the right, as in the one-app view. So with two apps whose interface runs
  // between ordered layers, the left app's Shared band goes left and the right app's goes right.
  // `band(name, layer)` says whether `layer` is a drawn Shared band (true), a drawn ordered band (false)
  // or no band (null) of app `name`. An end draws a line only when both its bands are drawn (see `drawable`);
  // an undrawn end counts nothing. Stacked columns draw no lines and keep the right.
  function sharedSides(view, names, band, sideBySide = true) {
    const at = new Map(names.map((name, index) => [name, {name, index}])), cost = names.map(() => ({left:0, right:0}));
    const has = (place, layer) => typeof layer === "string" && typeof band(place.name, layer) === "boolean";
    for (const entry of sideBySide && Array.isArray(view?.interfaces) ? view.interfaces.filter(record) : []) {
      const client = at.get(entry.client), server = at.get(entry.server);
      for (const end of endsOf(entry)) {
        if (!drawable(client, server, end, has)) continue;
        for (const [index, other, layer] of [[client.index, server.index, end.clientLayer], [server.index, client.index, end.serverLayer]]) {
          const shared = band(names[index], layer), toward = other < index ? "left" : "right";
          cost[index][shared === (toward === "left") ? "right" : "left"]++;
        }
      }
    }
    return new Map(names.map((name, index) => [name, cost[index].left < cost[index].right ? "left" : "right"]));
  }
  // Interface order (#949; owner ruling 2026-11-12, amending the 2026-11-09 manifest order): from manifest order
  // (`names`), each server app sits between its clients, the client with the most matched routes on its left,
  // the next on its right and the rest after them; ties keep manifest order. Servers are taken in manifest
  // order, and an app already placed beside an earlier server stays where it is. Apps with no interface keep
  // their manifest order, and a block takes the place of its first member.
  function columnOrder(view, names) {
    const at = new Map(names.map((name, index) => [name, index])), routesOf = new Map();
    for (const entry of Array.isArray(view?.interfaces) ? view.interfaces.filter(record) : []) {
      if (!at.has(entry.client) || !at.has(entry.server) || entry.client === entry.server) continue;
      const pair = JSON.stringify([entry.client, entry.server]);
      routesOf.set(pair, (routesOf.get(pair) || 0) + count(entry.counts?.matchedRoutes));
    }
    const routes = (client, server) => routesOf.get(JSON.stringify([client, server]));
    let order = [...names];
    const placed = new Set();
    for (const server of names) {
      if (placed.has(server)) continue;
      const clients = names.filter(name => name !== server && !placed.has(name) && routes(name, server) !== undefined)
        .sort((a, b) => routes(b, server) - routes(a, server) || at.get(a) - at.get(b));
      if (!clients.length) continue;
      const block = [clients[0], server, ...clients.slice(1)], first = order.findIndex(name => block.includes(name)), rest = order.filter(name => !block.includes(name));
      order = [...rest.slice(0, first), ...block, ...rest.slice(first)];
      for (const name of block) placed.add(name);
    }
    return order;
  }
  // How many lines each gutter of side-by-side columns in order `names` carries: the ends `drawable` lets through,
  // with `band` as for `sharedSides`. Known before any column is drawn, so the gutters can be sized to them.
  function laneCounts(view, names, band) {
    const at = new Map(names.map((name, index) => [name, {name, index}])), lanes = names.slice(1).map(() => 0);
    const has = (place, layer) => typeof layer === "string" && typeof band(place.name, layer) === "boolean";
    for (const entry of Array.isArray(view?.interfaces) ? view.interfaces.filter(record) : []) {
      const client = at.get(entry.client), server = at.get(entry.server);
      for (const end of endsOf(entry)) if (drawable(client, server, end, has)) lanes[Math.min(client.index, server.index)]++;
    }
    return lanes;
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
  // What every count in the All apps view means, under the inspector (`pin` is the snapshot's label).
  const footnote = pin => `Apps are detected from build manifests (Cargo binary packages, package.json, pyproject.toml, and runnable Maven and Gradle modules) or authored in .trellis/rules.toml. Layer order is compared within an app. Band and arrow counts are reference sites at ${pin}; deviation totals are rule instances, as trellis rules check reports them; ≥ means a lower bound. HTTP interfaces are evidence, not proof.`;
  const duplicateNotice = item =>
    `${item.apps.length} detected apps share the name ${item.name} (${manifestList(item)}). A layer cannot name them; rules.md Amendment (#783).`;

  // ---- Inspector facts, shared by the browser's inspector and the Herdr mod (#886). Values are the DTO's. ----
  const shown = value => typeof value === "number" && Number.isFinite(value) ? String(value) : "—";
  const text = value => typeof value === "string" ? value : "";
  // The interface inspector (AC15): its ends, the counts grid, the wrappers line and the matched list's heading (#950).
  function interfaceFacts(entry) {
    const counts = record(entry?.counts) ? entry.counts : {}, confidence = record(counts.confidence) ? counts.confidence : {};
    const wrappers = entry.wrappers, prefix = wrappers?.prefix;
    return {title:`${entry.client} → ${entry.server}`, basis:"HTTP · method and path match · structural evidence, not proof",
      matchedHeading:`Matched routes (${shown(counts.matchedRoutes)})`,
      ends:[`Client end: ${entry.client} · ${layerNames(entry, "clientLayer")}`, `Server end: ${entry.server} · ${layerNames(entry, "serverLayer")}`],
      rows:[["Matched routes", shown(counts.matchedRoutes)], ["Client call sites", shown(counts.callSites)],
        ["◆ Server-only routes", shown(counts.serverOnly), "warning"], ["◆ Client-only calls", shown(counts.clientOnly), "warning"],
        ["exact", shown(confidence.exact)], ["template", shown(confidence.template)], ["wrapper-inferred", shown(confidence.wrapperInferred)], ["unresolved", shown(confidence.unresolved)]],
      wrappers:Array.isArray(wrappers?.callees) ? `Wrappers: ${wrappers.callees.filter(name => typeof name === "string").join(", ")}${prefix ? ` · Client prefix: ${text(prefix.client)} → ${text(prefix.server)}` : ""}` : null};
  }
  // The app inspector: what was detected, how much of it has layers, and the interfaces it takes part in.
  function appFacts(item, view) {
    const layers = Array.isArray(view?.layers) ? view.layers : [], interfaces = (Array.isArray(view?.interfaces) ? view.interfaces : []).filter(record);
    const ordered = layers.filter(layer => !layer.shared && layer.app === item.name).length;
    const sharedLayers = layers.filter(layer => layer.shared && (layer.apps == null || strings(layer.apps).includes(item.name))).length;
    const unassigned = (Array.isArray(view?.modules) ? view.modules : []).filter(module => module.layer === null && module.app === item.name).length;
    const deviates = count(item.counts?.deviates);
    return {title:item.name, detail:`${item.languages.join(", ") || "no languages"} · ${item.roots.map(root => root || "./").join(", ") || "no roots"}`,
      rows:[["Files", String(item.fileCount)], ["Layers", `${ordered} + ${sharedLayers} shared`],
        ...interfaces.filter(entry => entry.client === item.name).map(entry => [`Calls to ${entry.server}`, shown(entry.counts?.callSites)]),
        ["Deviations", deviates ? `↑ ${plural(deviates, "deviation")}` : "0", deviates ? "error" : null],
        ["Unassigned", unassigned ? `◇ ${unassigned} unassigned` : "0", unassigned ? "warning" : null]],
      origin:origin(item), duplicate:item.duplicate ? duplicateNotice(item) : null, layered:ordered > 0,
      interfaces:interfaces.filter(entry => entry.client === item.name || entry.server === item.name)};
  }

  // ---- The app selection that Architecture, Treemap, Classes and the Files tree share (#852). ----
  // `assignment` is a ready summary's answer to "which app is this file in": each module's `app`, the
  // assignment the rules check uses (through its layer, else the most specific containing root).
  // A path the summary has no module for (an unindexed file, a folder) falls back to roots alone, as
  // the daemon assigns an unassigned file. With fewer than two apps there is no switcher, so no scope.
  function assignment(view) {
    if (view?.state !== "ready" || !Array.isArray(view.apps) || view.apps.length < 2) return null;
    const list = groups(view);
    if (!list.length) return null;
    const byPath = new Map(), folders = new Map(), indexed = new Set();
    for (const module of Array.isArray(view.modules) ? view.modules : []) {
      const identity = module?.identity, path = identity?.path;
      if (!record(identity) || identity.kind === "externalPackage" || typeof path !== "string") continue;
      const app = typeof module.app === "string" ? module.app : null;
      byPath.set(path, app);
      const parts = path.split("/");
      if (app !== null && !folders.has(app)) folders.set(app, new Set());
      for (let end = 1; end < parts.length; end++) {
        const folder = parts.slice(0, end).join("/");
        indexed.add(folder); folders.get(app)?.add(folder);
      }
    }
    return {groups:list, apps:view.apps.filter(app => record(app) && typeof app.name === "string"), byPath, folders, indexed};
  }
  // How specifically `root` contains `path`, as the daemon measures it (src/apps.rs): `.` holds every path.
  const depth = (root, path) => root === "." ? 0 : path === root || path.startsWith(`${root}/`) ? root.length + 1 : null;
  // The app with the most specific root holding `path`; the first on a tie; null for none.
  function rootApp(list, path) {
    let best = null, deepest = -1;
    for (const app of list) for (const root of strings(app.roots)) {
      const found = depth(root, path);
      if (found !== null && found > deepest) { best = app.name; deepest = found; }
    }
    return best;
  }
  const appOf = (assigned, path) => assigned.byPath.has(path) ? assigned.byPath.get(path) : rootApp(assigned.apps, path);
  // The scope a choice gives: an app name the assignment still has, else null (All apps).
  const chosen = (assigned, name) => assigned && typeof name === "string" && assigned.groups.some(item => item.name === name) ? name : null;
  // Whether file `path` is shown under choice `name`. All apps, or no assignment, shows every file.
  const holdsFile = (assigned, name, path) => chosen(assigned, name) === null || appOf(assigned, path) === name;
  // Whether folder `path` ("" is the checkout) is shown under `name`: it holds an indexed file of the
  // app, it holds no indexed file and lies in the app's roots, or it leads to one of those roots.
  function holdsFolder(assigned, name, path) {
    if (chosen(assigned, name) === null || path === "") return true;
    if (assigned.folders.get(name)?.has(path) || (!assigned.indexed.has(path) && rootApp(assigned.apps, path) === name)) return true;
    return assigned.apps.some(app => app.name === name && strings(app.roots).some(root => root !== "." && root.startsWith(`${path}/`)));
  }
  // The choice held for the page: memory only, and a new checkout starts at All apps (#789). Every
  // view reads it at its checkout's root key; a change notifies each subscriber once.
  function selection() {
    let root = null, name = null;
    const listeners = new Set();
    return {
      get:rootKey => rootKey && rootKey === root ? name : null,
      set(rootKey, next) {
        const value = typeof next === "string" ? next : null;
        if (!rootKey || (rootKey === root && value === name)) return false;
        root = rootKey; name = value;
        for (const listener of [...listeners]) listener(value);
        return true;
      },
      subscribe(listener) { listeners.add(listener); return () => listeners.delete(listener); },
    };
  }
  // The switcher's choices, in Architecture's order and words: All apps, then one per app name.
  const choices = assigned => assigned ? [{name:null, text:"All apps"}, ...assigned.groups.map(item => ({name:item.name, text:label(item)}))] : [];
  window.TrellisArchitectureApps = {groups, group, scoped, link, stubs, sumCounts, label, origin, sourceLine, duplicateNotice,
    layout, cardFits, columnOrder, laneCounts, confidenceText, cardText, cardName, endsText, header, routes, sharedSides, key, interfaceFacts, appFacts, footnote,
    assignment, rootApp, appOf, chosen, holdsFile, holdsFolder, selection, choices};
})();
