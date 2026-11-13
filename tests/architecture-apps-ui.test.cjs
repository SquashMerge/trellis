"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const read = file => fs.readFileSync(path.join(__dirname, "..", file), "utf8");
const pinSource = read("web/app.js").match(/const IndexPin = Object.freeze\(\{[\s\S]*?\n\}\);\nwindow.TrellisIndexPin = IndexPin;/)[0];
const pin = {indexGeneration:"12345678-1234-4123-8123-123456789abc",indexRevision:1};
const descendants = node => [node, ...node.children.flatMap(descendants)];
const text = node => descendants(node).map(item => item.textContent).join(" ");
const flush = () => new Promise(resolve => setImmediate(resolve));
const identity = file => ({sourceSetId:"workspace", language:"javascript", path:file, sid:`sid:${file}`});

function node(tagName) {
  return {tagName, textContent:"", children:[], dataset:{}, attrs:{}, listeners:{}, hidden:false, value:"", checked:false, style:{}, parent:null,
    append(...items) { for (const item of items) item.parent = this; this.children.push(...items); },
    replaceChildren(...items) { for (const item of items) item.parent = this; this.children = [...items]; this.textContent = ""; },
    after(...items) { const at = this.parent.children.indexOf(this); for (const item of items) item.parent = this.parent; this.parent.children.splice(at + 1, 0, ...items); },
    setAttribute(key, value) { this.attrs[key] = String(value); }, getAttribute(key) { return this.attrs[key] ?? null; },
    hasAttribute(key) { return key in this.attrs; }, focus() {},
    querySelectorAll(selector) {
      const found = selector.match(/^\[([\w-]+)(?:="([^"]*)")?\]$/);
      return found ? descendants(this).filter(item => found[1] in item.attrs && (found[2] === undefined || item.attrs[found[1]] === found[2])) : [];
    },
    addEventListener(name, fn) { (this.listeners[name] ||= []).push(fn); },
    async fire(name) { for (const fn of this.listeners[name] || []) await fn({target:this}); }};
}
// The apps module alone, as the page loads it.
function appsModule() {
  const window = {};
  vm.runInNewContext(read("web/architecture-apps.js"), {window});
  return window.TrellisArchitectureApps;
}
// The controller over a summary payload, with the renderer stubbed to record what it is given.
function controller(payload, options = {}) {
  const elements = new Map(), created = [], renders = [], calls = [], inspected = [];
  const get = id => created.findLast(item => item.id === id) || (elements.has(id) ? elements.get(id) : elements.set(id, node(id)).get(id));
  const document = {querySelector:() => null, getElementById:get, createElement:tag => { const item = node(tag); created.push(item); return item; }, createElementNS:(_ns, tag) => node(tag)};
  const bands = view => new Map(view.layers.filter(layer => !layer.shared).map((layer, index) => [layer.name, {x:10, y:40 + index * 80, w:options.bandWidth ?? 300, h:60}]));
  let renderer = async (container, view, renderOptions) => { renders.push({view, options:renderOptions}); container.replaceChildren(node("svg")); return {status:"ready", bands:bands(view)}; };
  const window = {TrellisArchitecturePele:{render:(...args) => renderer(...args), edgeWidth:count => 1 + count},
    TrellisShell:{clearArchitectureInspector() {}, showArchitectureInspector() {}},
    // `drawer`: the page is at the narrow-screen drawer width, the one width at which All apps stacks.
    matchMedia:query => ({matches:query === "(max-width: 900px)" && options.drawer === true})};
  get("architecture-diagram").clientWidth = options.width ?? 1500;
  vm.runInNewContext([pinSource, read("web/metric-heat.js"), read("web/architecture-apps.js"), read("web/architecture-deviations.js"), read("web/architecture.js")].join("\n"),
    {window, document, URLSearchParams, console, setTimeout, clearTimeout});
  const architecture = window.TrellisArchitecture.create({
    request:async url => { calls.push(url); return {payload}; }, onInspect:() => inspected.push(true),
    context:() => ({rootKey:"checkout", session:1, pin}), readSource() {}, onConflict() {}});
  return {architecture, get, renders, calls, window, inspected, setRenderer(fn) { renderer = fn; }};
}
const counts = (comparable, deviates, unknown) => ({comparable, conforms:comparable - deviates - unknown, deviates, unknown, excepted:0});
const app = (name, manifest, root, appCounts, extra = {}) =>
  ({name, kind:"npm", manifest, roots:[root], entries:[], languages:["javascript"], fileCount:2, source:"manifest", counts:appCounts, unassignedFileCount:1, ...extra});
function summary(apps, layers = [], modules = []) {
  return {...pin, state:"ready", rulesFile:{path:".trellis/rules.toml", source:"checkout-config", contentHash:"rules-one"}, aggregate:"max", role:"production",
    workbench:true, metricsVersion:"metrics:v1", evidenceTier:"syntax", semanticComplete:false, layers, modules, edges:[],
    references:{counts:{mappedSites:0, unresolvedSites:0}, coverage:[], limitations:[]}, rules:{counts:counts(10, 4, 2), unassignedCount:0},
    exceptionCount:0, apps, noApp:{counts:counts(0, 0, 0), unassignedFileCount:0}, interfaces:[]};
}
const buttons = h => h.get("architecture-apps").children.map(item => item.textContent);

test("scoped keeps the app's ordered layers, the shared layers that serve it, and the modules and edges inside them", () => {
  const apps = appsModule();
  const layers = [{name:"A", order:1, shared:false, app:"one"}, {name:"B", order:1, shared:false, app:"two"},
    {name:"All", shared:true, app:null, apps:null}, {name:"OnlyTwo", shared:true, app:null, apps:["two"]}, {name:"OnlyOne", shared:true, apps:["one"]}];
  const module = (file, layer, name = null) => ({identity:identity(file), layer, app:name});
  const modules = [module("a.js", "A", "one"), module("b.js", "B", "two"), module("s.js", "All"), module("t.js", "OnlyTwo"), module("u1.js", null, "one"), module("u2.js", null, "two"), module("n.js", null, null)];
  const edge = (sourceLayer, destinationLayer) => ({sourceLayer, destinationLayer, role:"production", count:1});
  const view = {state:"ready", layers, modules, edges:[edge("A", "All"), edge("A", "B"), edge("B", "OnlyTwo")], marker:"kept"};
  const one = apps.scoped(view, "one");
  assert.deepEqual([...one.layers].map(layer => layer.name), ["A", "All", "OnlyOne"]);
  assert.deepEqual([...one.modules].map(item => item.identity.path), ["a.js", "s.js", "u1.js"], "no other app's unassigned file, and none without an app");
  assert.deepEqual([...one.edges].map(item => `${item.sourceLayer}>${item.destinationLayer}`), ["A>All"]);
  assert.equal(one.marker, "kept"); assert.equal(view.layers.length, 5, "the held snapshot is not changed");
  assert.deepEqual([...apps.scoped(view, "two").layers].map(layer => layer.name), ["B", "All", "OnlyTwo"]);
  assert.deepEqual([...apps.scoped(view, "two").edges].map(item => `${item.sourceLayer}>${item.destinationLayer}`), ["B>OnlyTwo"]);
});

test("app_totals_read_dto: a group's numbers are the DTO's entries, summed", () => {
  const apps = appsModule(), dto = summary([app("web", "web/package.json", "web", counts(10, 3, 2)), app("daemon", "Cargo.toml", "src", counts(6, 1, 1), {kind:"cargo", languages:["rust"]})]);
  const [web, daemon] = apps.groups(dto);
  assert.equal(web.name, "web"); assert.equal(web.counts.deviates, 3); assert.equal(web.counts.comparable, 10); assert.equal(web.fileCount, 2);
  assert.equal(daemon.counts.unknown, 1); assert.equal(apps.label(daemon), "daemon · rust");
  assert.equal(apps.groups({apps:[]}).length, 0); assert.equal(apps.group(dto, "nope"), null);
  assert.equal(apps.group({apps:[{name:"__proto__", counts:{__proto__:null, deviates:1}}]}, "__proto__").counts.deviates, 1);
  assert.equal(({}).deviates, undefined, "a count key can never reach a prototype");
});

test("duplicate_app_names_share_one_group: one button, summed totals, every manifest listed, the notice, and the setup help", async () => {
  const dto = summary([app("web", "web/package.json", "web", counts(10, 3, 2)), app("web", "app/package.json", "app", counts(4, 1, 1)),
    app("daemon", "Cargo.toml", "src", counts(6, 1, 1), {kind:"cargo", languages:["rust"]})],
  [{name:"Storage", order:1, shared:false, app:"daemon", apps:null}], [{identity:identity("web/a.js"), layer:null, app:"web", physicalLines:1, declarationCount:1, metrics:{}}]);
  const h = controller(dto); await h.architecture.refresh();
  assert.deepEqual(buttons(h), ["All apps", "web · javascript", "daemon · rust"], "three apps, two names, two buttons plus All");
  await h.get("architecture-apps").children[1].fire("click"); await flush();
  assert.match(text(h.get("architecture-source-line")), /web · roots web, app · Detected from web\/package\.json, app\/package\.json \(npm\)/);
  assert.match(text(h.get("architecture-source-line")), /2 detected apps share the name web \(web\/package\.json, app\/package\.json\)\. A layer cannot name them; rules\.md Amendment \(#783\)\./);
  assert.equal(h.get("architecture-review").textContent, "4 deviations", "the group's totals are summed");
  assert.equal(h.get("architecture-state").dataset.state, "empty", "no layer can name the group, so it gets the setup help");
  assert.match(h.get("architecture-state").textContent, /web has no layers yet/);
});

test("authored_app_label (#794): an override app says it is authored in the rules file and lists every root", async () => {
  const apps = appsModule();
  const authored = app("web", null, "client", counts(4, 1, 0), {kind:null, source:"override", roots:["client", "shared/ui"]});
  const dto = summary([authored, app("daemon", "Cargo.toml", ".", counts(6, 1, 1), {kind:"cargo", languages:["rust"]})],
    [{name:"Screens", order:1, shared:false, app:"web", apps:null}, {name:"Storage", order:1, shared:false, app:"daemon", apps:null}]);
  const [web, daemon] = apps.groups(dto);
  assert.equal(web.authored, true); assert.equal(web.manifests.length, 0); assert.equal(web.kinds.length, 0);
  assert.equal(apps.origin(web), "Authored in .trellis/rules.toml");
  assert.equal(apps.sourceLine(web, 2), "web · roots client, shared/ui · Authored in .trellis/rules.toml");
  assert.equal(daemon.authored, false); assert.equal(apps.origin(daemon), "Detected from Cargo.toml (cargo)");
  const h = controller(dto); await h.architecture.refresh();
  assert.deepEqual(buttons(h), ["All apps", "web · javascript", "daemon · rust"]);
  await h.get("architecture-apps").children[1].fire("click"); await flush();
  assert.equal(text(h.get("architecture-source-line")).trim(), "web · roots client, shared/ui · Authored in .trellis/rules.toml");
  assert.doesNotMatch(text(h.get("architecture-source-line")), /Detected from/);
});

test("T00 markup in an app name is text: no element is created from it", async () => {
  const name = '<img src=x onerror="window.__pwned=1">';
  const dto = summary([app(name, "web/package.json", "web", counts(1, 0, 0)), app("daemon", "Cargo.toml", "src", counts(1, 0, 0))],
    [], [{identity:identity("web/a.js"), layer:null, app:name, physicalLines:1, declarationCount:1, metrics:{}}]);
  const h = controller(dto); await h.architecture.refresh();
  assert.equal(buttons(h)[1], `${name} · javascript`);
  await h.get("architecture-apps").children[1].fire("click"); await flush();
  const nodes = [h.get("architecture-apps"), h.get("architecture-source-line"), h.get("architecture-state"), h.get("architecture-diagram")].flatMap(descendants);
  assert.equal(nodes.filter(item => item.tagName === "img").length, 0);
  assert.equal(h.window.__pwned, undefined);
  assert.ok(text(h.get("architecture-source-line")).includes(name), "the name appears as literal text");
  assert.ok(h.get("architecture-state").textContent.includes(name));
  assert.equal(h.get("architecture-diagram").children.length > 0, true);
});

test("stubs sum a client's ends, bound a server's shared routes, skip other layers and tolerate hostile shapes", () => {
  const apps = appsModule(), end = (clientLayer, serverLayer, routes, callSites) => ({clientLayer, serverLayer, routes, callSites});
  const view = {interfaces:[{client:"web", server:"api", ends:[end("UI", "Http", 2, 3), end("UI", "Rpc", 1, 4), end(null, "Http", 5, 5), end("Other", "Http", 9, 9)]},
    null, {client:"web", server:"api", ends:"nope"}, {client:"api", server:"web", ends:[end("Http", "UI", Infinity, -1), "x"]}]};
  const flat = list => JSON.parse(JSON.stringify(list));
  assert.deepEqual(flat(apps.stubs(view, "web", "UI")), [{side:"client", client:"web", server:"api", atLeast:false, routes:3, callSites:7},
    {side:"server", client:"api", server:"web", atLeast:false, routes:0, callSites:0}]);
  assert.deepEqual(flat(apps.stubs(view, "api", "Http")), [{side:"server", client:"web", server:"api", atLeast:true, routes:9, callSites:17},
    {side:"client", client:"api", server:"web", atLeast:false, routes:0, callSites:0}]);
  assert.deepEqual(flat(apps.stubs(view, "api", "Nothing")), []);
  assert.deepEqual(flat(apps.stubs({}, "web", "UI")), []);
  assert.equal(apps.link(view, "web", "api"), view.interfaces[0]); assert.equal(apps.link(view, "api", "nobody"), null);
});

// All apps (#790): three apps in a row, web -> api -> db, with a second interface running back from db to api.
// api serves web and db, so interface order (#949) keeps the inventory's web | api | db.
function threeApps(extra = {}) {
  const layer = (name, app) => ({name, order:1, shared:false, app, apps:null});
  const module = (file, name, app) => ({identity:identity(file), layer:name, app, physicalLines:3, declarationCount:1, metrics:{cognitive:4}});
  const end = (clientLayer, serverLayer, routes, callSites) => ({clientLayer, serverLayer, routes, callSites});
  const entry = (client, server, ends, counts) => ({client, server, kind:"http", ends, wrappers:{callees:[], prefix:null},
    counts:{matchedRoutes:0, callSites:0, serverOnly:0, clientOnly:0, confidence:{exact:0, template:0, wrapperInferred:0, unresolved:0}, ...counts}});
  const dto = summary([app("web", "web/package.json", "web", counts(10, 3, 2)), app("api", "api/package.json", "api", counts(6, 1, 1), {unassignedFileCount:4}),
    app("db", "db/package.json", "db", counts(2, 0, 0))],
  [layer("UI", "web"), layer("Api", "api"), layer("Data", "db")], [module("web/a.js", "UI", "web"), module("api/a.js", "Api", "api"), module("db/a.js", "Data", "db")]);
  dto.interfaces = [entry("web", "api", [end("UI", "Api", 2, 5), end("UI", "Api", 1, 1)], {matchedRoutes:3, callSites:6, serverOnly:2, clientOnly:0, confidence:{exact:4, template:0, wrapperInferred:2, unresolved:0}}),
    entry("api", "db", [end("Api", "Data", 1, 40)], {matchedRoutes:1, callSites:40}),
    entry("db", "api", [end("Data", "Api", 1, 2)], {matchedRoutes:1, callSites:2, clientOnly:7})];
  dto.noApp = {counts:counts(0, 0, 0), unassignedFileCount:9};
  return Object.assign(dto, extra);
}
const flat = item => text(item).replace(/\s+/g, " ").trim();
const grid = h => h.get("architecture-diagram").children[0];
const columnsOf = h => grid(h).children.filter(item => item.attrs.role === "group");
const cardsOf = h => descendants(h.get("architecture-diagram")).filter(item => item.attrs["data-trellis-card"] === "interface");
const headers = h => descendants(h.get("architecture-diagram")).filter(item => item.className === "architecture-app-header");

test("layout (#949, owner ruling 2026-11-12): 2 to 4 columns always side by side, 352px or wider, gutters sized to their lanes; only the drawer stacks", () => {
  const apps = appsModule(), plan = (width, count, lanes, extra = {}) => JSON.parse(JSON.stringify(apps.layout({width, count, lanes, ...extra})));
  // A gutter is max(48, lanes x 6 + 2 x 8 + 8): 48 up to four lanes, then 6px a lane.
  assert.deepEqual(plan(2400, 5, [0, 1, 4, 5]).gutters, [48, 48, 48, 54]); assert.deepEqual(plan(2000, 2, [26]).gutters, [180]);
  assert.deepEqual(plan(2000, 3).gutters, [48, 48], "no lanes known: the least gutter");
  // Never stack; scroll: any width keeps 2 to 4 columns side by side at 352px or more, wider than the pane.
  for (const count of [2, 3, 4]) for (const width of [0, 300, 1098, 1151]) {
    const at = plan(width, count, [2, 1, 1]);
    assert.equal(at.sideBySide, true, `${count} columns at ${width}px`); assert.ok(at.widths.every(each => each >= 352), `${count} at ${width}: ${at.widths}`);
  }
  const narrow = plan(1098, 3, [2, 1]);
  assert.deepEqual(narrow.columns.map(column => column.x), [0, 400, 800]); assert.equal(narrow.width, 1152, "wider than the 1098px pane, so the canvas scrolls");
  assert.equal(plan(1600, 3, [2, 1]).widths[0], Math.floor((1600 - 2 * 48) / 3), "a wider pane widens the columns");
  // Only the drawer width stacks, at one width for every column.
  const drawer = plan(1600, 3, [2, 1], {drawer:true});
  assert.equal(drawer.sideBySide, false); assert.deepEqual(drawer.gutters, [], "stacked columns have no gutter"); assert.deepEqual(drawer.widths, [1600, 1600, 1600]);
  assert.deepEqual(plan(300, 2, [1], {drawer:true}).widths, [352, 352], "text never shrinks below the least column; the diagram scrolls");
  // Five or more sit side by side only when they all fit.
  assert.equal(plan(5 * 352 + 4 * 48 - 1, 5).sideBySide, false); assert.equal(plan(5 * 352 + 4 * 48, 5).sideBySide, true);
  assert.equal(plan(5000, 1).sideBySide, false, "one column is never side by side");
  const wide = JSON.parse(JSON.stringify(apps.layout({width:1158, count:3, lanes:[5, 1], natural:[352, 900, 100]})));
  assert.deepEqual(wide.widths, [352, 900, 352], "a drawing wider than its column widens it, and never narrows one");
  assert.deepEqual(wide.columns.map(column => column.x), [0, 406, 1354]); assert.equal(wide.width, 1706);
  assert.equal(plan(NaN, 2).sideBySide, true, "an unmeasured pane still lays 2 columns side by side");
  assert.equal(apps.cardFits(176), true); assert.equal(apps.cardFits(175), false, "a card floats only in a gutter at least a card wide");
});

test("columnOrder (#949): each server sits between its clients, the busier on its left; ties and apps with no interface keep manifest order", () => {
  const apps = appsModule(), link = (client, server, matchedRoutes) => ({client, server, ends:[], counts:{matchedRoutes}});
  const order = (names, ...interfaces) => [...apps.columnOrder({interfaces}, names)];
  assert.deepEqual(order(["trellis", "herdr-mod", "web"], link("web", "trellis", 28), link("herdr-mod", "trellis", 30)), ["herdr-mod", "trellis", "web"]);
  assert.deepEqual(order(["trellis", "herdr-mod", "web"], link("web", "trellis", 28), link("herdr-mod", "trellis", 3)), ["web", "trellis", "herdr-mod"], "the client with the most matched routes on the left");
  assert.deepEqual(order(["srv", "b", "a"], link("a", "srv", 4), link("b", "srv", 4)), ["b", "srv", "a"], "a tie keeps manifest order");
  assert.deepEqual(order(["srv", "a", "b", "c"], link("a", "srv", 1), link("b", "srv", 9), link("c", "srv", 5)), ["b", "srv", "c", "a"], "the rest after the right client");
  assert.deepEqual(order(["solo", "srv", "other", "web"], link("web", "srv", 2)), ["solo", "web", "srv", "other"], "apps with no interface keep their places; a block takes its first member's");
  assert.deepEqual(order(["x", "y", "z"]), ["x", "y", "z"], "no interface: manifest order");
  assert.deepEqual(order(["x", "y"], link("x", "x", 5), link("y", "ghost", 5), null), ["x", "y"], "a self link, an absent app or a hostile entry orders nothing");
  assert.deepEqual(order(["web", "api", "db"], link("web", "api", 3), link("api", "db", 1)), ["web", "api", "db"], "a chain: an app placed beside one server stays");
});

test("card_counts_equal_dto: every number on a card is the DTO's, non-zero confidence only, and ◆ lines only when above zero", () => {
  const apps = appsModule(), [web, , back] = threeApps().interfaces;
  assert.deepEqual(JSON.parse(JSON.stringify(apps.cardText(web))), [{text:"HTTP · 3 routes", strong:true}, {text:"6 call sites"},
    {text:"4 exact · 2 wrapper-inferred"}, {text:"◆ 2 server-only", tone:"warning"}], "the ends add up to 8 sites; the card says the DTO's 6");
  assert.deepEqual(Array.from(apps.cardText(back), row => row.text), ["HTTP · 1 route", "2 call sites", "◆ 7 client-only"]);
  assert.equal(apps.confidenceText({confidence:{exact:0, template:0, wrapperInferred:0, unresolved:0}}), ""); assert.equal(apps.confidenceText(null), "");
  assert.equal(apps.cardName(web), "HTTP interface from web layer UI to api layer Api: 3 matched routes, 6 client call sites, 2 server-only routes, 0 client-only calls; evidence 4 exact · 2 wrapper-inferred");
  assert.ok(apps.cardName(web, true).endsWith(". Selected"));
  assert.equal(apps.cardText({counts:{matchedRoutes:Infinity, callSites:-1, serverOnly:"3"}})[0].text, "HTTP · 0 routes", "a hostile count is zero");
});

test("a column per app with its header totals read from the DTO; an app without layers says so", async () => {
  const dto = threeApps(); dto.layers = dto.layers.filter(layer => layer.name !== "Data"); dto.modules = dto.modules.filter(item => item.layer !== "Data");
  const h = controller(dto); await h.architecture.refresh();
  assert.equal(grid(h).dataset.layout, "columns");
  assert.deepEqual(columnsOf(h).map(item => item.attrs["aria-label"]), ["web", "api", "db"], "in interface order, here the inventory's");
  assert.deepEqual(h.renders.map(render => render.options.title), ["web · ordered layers", "api · ordered layers"], "db has no layer to draw");
  assert.deepEqual(headers(h).map(flat), [
    "web javascript · web · 2 files · max 4 (cognitive) · 3 deviations Selected", "api javascript · api · 2 files · max 4 (cognitive) · 1 deviation Selected",
    "db no layers yet Selected"]);
  assert.match(text(columnsOf(h)[2]), /◇ 1 unassigned · listed, never dropped/);
  assert.equal(h.get("architecture-key-interface").hidden, false);
  assert.deepEqual(h.renders.map(render => render.options.display.layers.size), [1, 1]);
});

test("each column's budget is floor(150 / n) and its drawing opens the shown layers", async () => {
  const h = controller(threeApps()); await h.architecture.refresh();
  for (const render of h.renders) { render.options.display.reset(); assert.equal(render.options.display.entries("UI", []).length, 0); }
  const budgets = h.renders.map(render => { let used = 0; const room = Array.from({length:200}, (_, index) => ({identity:identity(`f${index}.js`), declarationCount:1, layer:"UI", metrics:{cognitive:1}}));
    render.options.display.reset(); used = render.options.display.entries("UI", room).length; return used; });
  assert.deepEqual(budgets.map(count => count <= 50), [true, true, true], "no column draws more than floor(150 / 3) chips");
});

test("gutters and lanes (#949): a gutter as wide as its lines, a line per end in its own lane, the arrowhead at the server end, cards under the grid", async () => {
  const h = controller(threeApps(), {width:1408}); await h.architecture.refresh();
  assert.equal(cardsOf(h).length, 3);
  // Two lines in each gutter, so each is the least 48px: columns of floor((1408 - 96) / 3) = 437 at x 0, 485 and 970.
  assert.deepEqual(grid(h).style.gridTemplateColumns.split(" "), ["437px", "48px", "437px", "48px", "437px"]);
  assert.deepEqual(columnsOf(h).map(item => item.style.gridColumn), ["1", "3", "5"], "a column takes every other track");
  const lines = descendants(h.get("architecture-diagram")).filter(item => item.attrs.class === "architecture-apps-line");
  const forward = lines.filter(item => item.attrs["data-interface"] === JSON.stringify(["interface", "web", "api"]));
  assert.equal(forward.length, 2, "each end has a line"); assert.deepEqual(forward.map(item => item.attrs["stroke-width"]), ["6", "2"], "edgeWidth of that end's call sites");
  const lane = line => Number(line.attrs.d.match(/L(\d+),/)[1]);
  const laneXs = forward.map(lane); assert.equal(new Set(laneXs).size, laneXs.length, "no two lines share a lane");
  assert.ok(laneXs.every(x => x > 437 && x < 485), `the lanes run inside the 48px gutter between the first two columns: ${laneXs}`);
  const arrow = descendants(h.get("architecture-diagram")).find(item => item.attrs.class === "architecture-apps-arrow" && item.attrs["data-interface"] === JSON.stringify(["interface", "web", "api"]));
  assert.ok(arrow.attrs.points.startsWith("495,"), "the head sits on the server band's left border: " + arrow.attrs.points);
  const backwards = descendants(h.get("architecture-diagram")).find(item => item.attrs.class === "architecture-apps-arrow" && item.attrs["data-interface"] === JSON.stringify(["interface", "db", "api"]));
  assert.ok(backwards.attrs.points.startsWith("795,"), "B -> A runs the other way and ends on the server's right border: " + backwards.attrs.points);
  const overlay = descendants(h.get("architecture-diagram")).find(item => item.attrs.class === "architecture-apps-overlay");
  assert.equal(overlay.attrs["aria-hidden"], "true"); assert.equal(overlay.attrs.width, "1407");
  assert.equal(descendants(overlay).some(item => /error/.test(item.attrs.class || "")), false, "never the error tone");
  const card = cardsOf(h)[0];
  assert.match(card.attrs["aria-label"], /^HTTP interface from web layer UI to api layer Api: 3 matched routes/);
  assert.equal(descendants(card).some(item => item.className === "architecture-card-ends"), false, "both ends are drawn, so no words");
  const under = descendants(h.get("architecture-diagram")).find(item => item.className === "architecture-apps-cards");
  assert.deepEqual(cardsOf(h).map(item => [item.parent === under, item.attrs["data-gutter"]]), [[true, undefined], [true, undefined], [true, undefined]],
    "a 48px gutter is narrower than a card, so every card is listed under the grid");
  assert.deepEqual(cardsOf(h).map(item => item.children[0].textContent), ["web → api", "api → db", "db → api"], "and names its interface");
});

test("interface order (#949): 3 apps at a realistic width put the server between its 2 clients, side by side with both lines", async () => {
  const dto = threeApps();
  // Manifest order srv | one | two: srv serves both, two with more matched routes.
  dto.apps = [app("srv", "Cargo.toml", "srv", counts(1, 0, 0)), app("one", "one/package.json", "one", counts(1, 0, 0)), app("two", "two/package.json", "two", counts(1, 0, 0))];
  dto.layers = [["Api", "srv"], ["UiOne", "one"], ["UiTwo", "two"]].map(([name, owner]) => ({name, order:1, shared:false, app:owner, apps:null}));
  dto.modules = dto.layers.map(layer => ({identity:identity(`${layer.app}/a.js`), layer:layer.name, app:layer.app, physicalLines:3, declarationCount:1, metrics:{cognitive:1}}));
  const entry = (client, layer, routes) => ({client, server:"srv", kind:"http", ends:[{clientLayer:layer, serverLayer:"Api", routes, callSites:routes}], wrappers:{callees:[], prefix:null},
    counts:{matchedRoutes:routes, callSites:routes, serverOnly:0, clientOnly:0, confidence:{exact:routes, template:0, wrapperInferred:0, unresolved:0}}});
  dto.interfaces = [entry("one", "UiOne", 3), entry("two", "UiTwo", 28)];
  const h = controller(dto, {width:1152}); await h.architecture.refresh();
  assert.equal(grid(h).dataset.layout, "columns", "3 x 352 + 2 x 48 fits");
  assert.deepEqual(columnsOf(h).map(item => item.attrs["aria-label"]), ["two", "srv", "one"]);
  const lines = descendants(h.get("architecture-diagram")).filter(item => item.attrs.class === "architecture-apps-line");
  assert.deepEqual(lines.map(item => JSON.parse(item.attrs["data-interface"]).slice(1)), [["one", "srv"], ["two", "srv"]], "both interfaces run as lines");
  assert.equal(cardsOf(h).some(item => descendants(item).some(child => child.className === "architecture-card-ends")), false, "no end is left to words");
});

test("an interface between columns that are not adjacent, or stacked columns, name their ends in words", async () => {
  // A chain web -> api -> db, and web -> db past api, which no order can make adjacent.
  const dto = threeApps(); dto.interfaces.push({...structuredClone(dto.interfaces[1]), client:"web", server:"db", ends:[{clientLayer:"UI", serverLayer:"Data", routes:1, callSites:3}]});
  const h = controller(dto); await h.architecture.refresh();
  assert.deepEqual(columnsOf(h).map(item => item.attrs["aria-label"]), ["web", "api", "db"]);
  const far = cardsOf(h).find(item => item.attrs["data-trellis-action-id"] === JSON.stringify(["interface", "web", "db"]));
  assert.match(flat(far), /web · UI → db · Data/); assert.equal(far.attrs["data-gutter"], undefined, "it is not placed in a gutter");
  assert.equal(descendants(h.get("architecture-diagram")).some(item => item.attrs["data-interface"] === JSON.stringify(["interface", "web", "db"])), false, "and draws no line");
  // Only the narrow-screen drawer width stacks (owner ruling 2026-11-12); a narrow desktop canvas stays in columns.
  const desktop = controller(threeApps(), {width:900}); await desktop.architecture.refresh();
  assert.equal(grid(desktop).dataset.layout, "columns"); assert.equal(grid(desktop).style.width, "1152px", "wider than the canvas, which scrolls");
  const narrow = controller(threeApps(), {width:700, drawer:true}); await narrow.architecture.refresh();
  assert.equal(grid(narrow).dataset.layout, "stacked");
  assert.equal(descendants(narrow.get("architecture-diagram")).some(item => item.attrs.class === "architecture-apps-line"), false, "no line in a stacked layout");
  for (const card of cardsOf(narrow)) assert.match(text(card), /→/);
  const order = grid(narrow).children.map(item => item.attrs["data-trellis-card"] || item.attrs["aria-label"]);
  assert.deepEqual(order.slice(0, 4), ["web", "interface", "api", "interface"], "a card follows its client column");
});

test("the aside: one dashed box per hidden layer and the DTO's unassigned counts", async () => {
  const dto = threeApps(); dto.layers[0].display = "hidden";
  const h = controller(dto); await h.architecture.refresh();
  const aside = descendants(h.get("architecture-diagram")).find(item => item.className === "architecture-apps-aside");
  const boxes = aside.children.map(flat);
  assert.equal(boxes[0], "UI · web 1 file · hidden UI");
  assert.deepEqual(boxes.slice(1), ["web ◇ 1 unassigned · listed, never dropped", "api ◇ 4 unassigned · listed, never dropped", "db ◇ 1 unassigned · listed, never dropped", "no app ◇ 9 unassigned · listed, never dropped"]);
  assert.equal(aside.children[0].attrs["aria-expanded"], "false");
  await aside.children[0].fire("click"); await flush();
  assert.equal(h.get("architecture-hidden-toggle").attrs["aria-expanded"], "true", "the box is the toggle's face");
});

test("AC23 no cross-app sequence control and no serves relation anywhere in the view", async () => {
  const h = controller(threeApps()); await h.architecture.refresh();
  const everything = [h.get("architecture-diagram"), h.get("architecture-apps"), h.get("architecture-source-line"), h.get("architecture-inspector-content")].flatMap(descendants)
    .flatMap(item => [item.textContent, ...Object.values(item.attrs)]).join("\n");
  assert.doesNotMatch(everything, /cross-app sequence/i); assert.doesNotMatch(everything, /serves/i);
  assert.doesNotMatch(read("web/index.html"), /Cross-app sequence/i);
});

test("duplicate_app_names_share_one_group: a duplicated name is one column that lists its manifests", async () => {
  const dto = threeApps(); dto.apps.push(app("web", "app/package.json", "app", counts(4, 1, 1)));
  const h = controller(dto); await h.architecture.refresh();
  assert.deepEqual(columnsOf(h).map(item => item.attrs["aria-label"]), ["web", "api", "db"]);
  assert.match(flat(headers(h)[0]), /Detected from web\/package\.json, app\/package\.json/); assert.match(flat(headers(h)[0]), /4 deviations/, "summed counts");
});

test("AC25 selecting a column header or a card shows the word Selected, presses it alone and names it so", async () => {
  const h = controller(threeApps()); await h.architecture.refresh();
  const pressedNow = () => descendants(h.get("architecture-diagram")).filter(item => item.attrs["aria-pressed"] === "true");
  assert.equal(pressedNow().length, 0);
  await headers(h)[1].fire("click");
  assert.deepEqual(pressedNow(), [headers(h)[1]]); assert.equal(h.get("architecture-inspector-title").textContent, "api");
  const card = cardsOf(h)[0]; await card.fire("click");
  assert.deepEqual(pressedNow(), [card]); assert.equal(h.get("architecture-inspector-title").textContent, "web → api");
  assert.ok(card.attrs["aria-label"].endsWith(". Selected"));
  const line = descendants(h.get("architecture-diagram")).find(item => item.attrs.class === "architecture-apps-line");
  assert.equal(line.attrs["data-selected"], "true");
  assert.ok(descendants(card).some(item => item.className === "architecture-selected-tag" && item.textContent === "Selected"));
  await h.get("architecture-review").fire("click"); await flush(); await h.get("architecture-deviations-diagram").fire("click"); await flush();
  assert.equal(pressedNow().length, 0, "the overview presses nothing");
  assert.equal(card.attrs["aria-label"].endsWith("Selected"), false); assert.equal(line.attrs["data-selected"], "false");
});

test("T00 markup in app names and layers is text: no element is created from a column, card or aside", async () => {
  const bad = '<img src=x onerror="window.__pwned=1">';
  const dto = threeApps(); dto.apps[0].name = bad; dto.layers[0].app = bad; dto.modules[0].app = bad; dto.interfaces[0].client = bad;
  const h = controller(dto); await h.architecture.refresh();
  const all = descendants(h.get("architecture-diagram"));
  assert.equal(all.filter(item => item.tagName === "img").length, 0); assert.equal(h.window.__pwned, undefined);
  assert.ok(text(headers(h)[0]).includes(bad)); assert.ok(cardsOf(h)[0].attrs["aria-label"].includes(bad));
  assert.ok(all.some(item => item.textContent.includes(bad)));
  const source = read("web/architecture-apps.js") + read("web/architecture.js");
  assert.doesNotMatch(source, /innerHTML|outerHTML|insertAdjacentHTML/);
});

test("stale async: a scope change or a suspend during the column drawings paints nothing", async () => {
  const h = controller(threeApps()); await h.architecture.refresh();
  const gate = (() => { let release; const promise = new Promise(resolve => { release = resolve; }); return {promise, release}; })();
  let first = true;
  h.setRenderer(async (container, view, options) => {
    h.renders.push({view, options});
    if (first) { first = false; await gate.promise; }
    container.replaceChildren(node("svg")); return {status:"ready", bands:new Map()};
  });
  const slow = h.architecture.refresh(); await flush();
  const before = h.renders.length; h.architecture.suspend(); gate.release(); await slow; await flush();
  assert.equal(h.renders.length, before, "the suspended drawing never reaches its next column");
  assert.equal(h.get("architecture-diagram").children.length, 0, "nothing is painted");
  const later = controller(threeApps()); await later.architecture.refresh();
  const pending = []; later.setRenderer(async (container) => { await new Promise(resolve => pending.push(resolve)); container.replaceChildren(node("svg")); return {status:"ready", bands:new Map()}; });
  const drawing = later.architecture.refresh(); await flush();
  const switched = later.get("architecture-apps").children[1].fire("click"); await flush();
  while (pending.length) { pending.shift()(); await flush(); }
  await Promise.all([drawing, switched]); await flush();
  assert.equal(descendants(later.get("architecture-diagram")).some(item => item.className === "architecture-apps-grid"), false, "the superseded All apps drawing never lands");
});

test("a column's shared-layer chips sum that app's own edges, as the one-app view does", async () => {
  const dto = threeApps();
  dto.layers.push({name:"Common", shared:true, app:null, apps:null});
  dto.modules.push({identity:identity("common/a.js"), layer:"Common", app:null, physicalLines:1, declarationCount:1, metrics:{cognitive:1}});
  const edge = (destinationLayer, count) => ({sourceLayer:"Common", destinationLayer, role:"production", count, status:"deviates", excepted:false, exceptionReason:null, pairs:[]});
  dto.edges = [edge("UI", 2), edge("Api", 3)];
  const h = controller(dto); await h.architecture.refresh();
  const chips = render => JSON.parse(JSON.stringify(render.options.layerStats(render.view.layers.find(layer => layer.name === "Common")).map(stat => stat.text)));
  const columns = new Map(h.renders.map(render => [render.options.title.split(" ")[0], chips(render)]));
  for (const name of ["web", "api"]) {
    const before = h.renders.length;
    await h.get("architecture-apps").children.find(item => item.textContent.startsWith(name)).fire("click"); await flush();
    assert.deepEqual(chips(h.renders.at(-1)), columns.get(name), `${name}: the column and the one-app view agree`);
    assert.equal(h.renders.length, before + 1);
    await h.get("architecture-apps").children[0].fire("click"); await flush();
  }
  assert.notDeepEqual(columns.get("web"), columns.get("api"), "each column counts only its own edges");
  assert.ok(columns.get("web").some(text => /2/.test(text)) && !columns.get("web").some(text => /5/.test(text)));
});

// #850: the side each column's Shared bands take. `band` is the drawn bands of each app: true for
// a Shared band, false for an ordered one, and anything else is no band.
function sides({interfaces, names = ["web", "api"], bands = {web:{UI:false, Common:true}, api:{Api:false, Store:false, Common:true}}, sideBySide = true}) {
  const apps = appsModule(), end = ([clientLayer, serverLayer]) => ({clientLayer, serverLayer, routes:1, callSites:1});
  const view = {interfaces:interfaces.map(([client, server, ends]) => ({client, server, ends:ends.map(end)}))};
  return Object.fromEntries(apps.sharedSides(view, names, (name, layer) => bands[name]?.[layer] ?? null, sideBySide));
}
test("#850 shared_side_two_apps: an interface on the ordered layers puts each Shared band away from the partner", () => {
  assert.deepEqual(sides({interfaces:[["web", "api", [["UI", "Api"]]]]}), {web:"left", api:"right"});
  assert.deepEqual(sides({interfaces:[["api", "web", [["Api", "UI"], ["Store", "UI"]]]], names:["api", "web"]}), {api:"left", web:"right"},
    "the given column order decides, whichever app is the client");
});
test("#850 shared_side_fallbacks: ties keep the right; a Shared end, both-side partners and hidden or far ends count by crossings", () => {
  // No interface, or only undrawn, far or stacked ends: no line, so the right, as in the one-app view.
  assert.deepEqual(sides({interfaces:[]}), {web:"right", api:"right"});
  assert.deepEqual(sides({interfaces:[["web", "api", [["Hidden", "Api"]]]]}), {web:"right", api:"right"}, "an end on no drawn band draws no line");
  assert.deepEqual(sides({interfaces:[["web", "api", [["UI", "Api"]]]], sideBySide:false}), {web:"right", api:"right"}, "stacked columns draw no lines");
  assert.deepEqual(sides({interfaces:[["web", "db", [["UI", "Data"]]]], names:["web", "api", "db"], bands:{web:{UI:false}, db:{Data:false}}}),
    {web:"right", api:"right", db:"right"}, "columns that are not adjacent draw no line");
  // The Shared band is itself the end: on the partner's side it crosses nothing, so it stays right of the left app.
  assert.deepEqual(sides({interfaces:[["web", "api", [["Common", "Api"]]]]}), {web:"right", api:"right"});
  assert.deepEqual(sides({interfaces:[["web", "api", [["UI", "Common"]]]]}), {web:"left", api:"left"}, "a Shared server end pulls the right app's band toward its partner");
  // One ordered and one Shared end toward the same partner: one crossing either way, a tie, so the right.
  assert.deepEqual(sides({interfaces:[["web", "api", [["UI", "Api"], ["Common", "Api"]]]]}), {web:"right", api:"right"});
  // A middle app with partners on both sides takes the side with fewer crossings, and the right on a tie.
  const three = {names:["web", "api", "db"], bands:{web:{UI:false}, api:{Api:false, Common:true}, db:{Data:false}}};
  assert.equal(sides({...three, interfaces:[["web", "api", [["UI", "Api"]]], ["api", "db", [["Api", "Data"]]]]}).api, "right", "one line each side: a tie");
  assert.equal(sides({...three, interfaces:[["web", "api", [["UI", "Api"]]], ["api", "db", [["Api", "Data"], ["Api", "Data"]]]]}).api, "left", "two lines right, one left");
  assert.equal(sides({...three, interfaces:[["web", "api", [["UI", "Api"], ["UI", "Api"]]], ["api", "db", [["Api", "Data"]]]]}).api, "right", "two lines left, one right");
});
test("#850 each column's drawing is asked for its Shared side; the one-app view never is", async () => {
  const dto = threeApps();
  dto.apps = dto.apps.slice(0, 2); dto.layers = dto.layers.filter(layer => layer.app !== "db"); dto.modules = dto.modules.filter(module => module.app !== "db");
  dto.interfaces = dto.interfaces.filter(entry => entry.client !== "db" && entry.server !== "db");
  dto.layers.push({name:"Common", shared:true, app:null, apps:null});
  dto.modules.push({identity:identity("common/a.js"), layer:"Common", app:null, physicalLines:1, declarationCount:1, metrics:{cognitive:1}});
  const h = controller(dto); await h.architecture.refresh();
  assert.deepEqual(h.renders.map(render => [render.options.title.split(" ")[0], render.options.sharedSide]), [["web", "left"], ["api", "right"]]);
  await h.get("architecture-apps").children.find(item => item.textContent.startsWith("web")).fire("click"); await flush();
  assert.equal(h.renders.at(-1).options.sharedSide, undefined, "one app: unchanged");
  const narrow = controller(dto, {width:700, drawer:true}); await narrow.architecture.refresh();
  assert.deepEqual(narrow.renders.map(render => render.options.sharedSide), ["right", "right"], "stacked columns keep the right");
});
test("#850 shared_side_drawn_lines_only: an end whose partner layer is null or undrawn draws no line and counts nothing", () => {
  const bands = {web:{UI:false, Store:false, Common:true}, api:{Api:false}};
  // Prime's probe (PR #872): one real line from web's Shared band, two distinct undrawn pairs from its ordered bands.
  assert.deepEqual(sides({bands, interfaces:[["web", "api", [["Common", "Api"], ["UI", null], ["Store", null]]]]}), {web:"right", api:"right"},
    "only the Shared line is drawn, and it crosses nothing with the band on the partner's side");
  // Mirrored: web is the right column, so its one real Shared line runs left and its band goes left.
  assert.deepEqual(sides({bands, names:["api", "web"], interfaces:[["web", "api", [["Common", "Api"], ["UI", null], ["Store", null]]]]}), {api:"left", web:"left"},
    "api's one drawn end leaves right, so its side is left too");
  // The null on the other end: an api end with no layer draws no line for either column.
  assert.deepEqual(sides({bands, interfaces:[["web", "api", [["UI", "Api"]]], ["api", "web", [[null, "UI"], [null, "Store"]]]]}), {web:"left", api:"right"});
  // Mixed: one drawn ordered line and an undrawn Shared end; only the drawn line decides.
  assert.deepEqual(sides({bands, interfaces:[["web", "api", [["UI", "Api"], ["Common", null]]]]}), {web:"left", api:"right"});
  // A lone ordered -> null end draws no line, so the right fallback, on both columns.
  assert.deepEqual(sides({bands, interfaces:[["web", "api", [["UI", null]]]]}), {web:"right", api:"right"});
  assert.deepEqual(sides({bands, interfaces:[["web", "api", [[null, "Api"]]]]}), {web:"right", api:"right"});
});
test("#850 routes and sharedSides agree on which ends draw a line", () => {
  const apps = appsModule(), frame = {x:0, y:0, w:10, h:10};
  const view = {interfaces:[{client:"web", server:"api", ends:[{clientLayer:"Common", serverLayer:"Api", callSites:1}, {clientLayer:"UI", serverLayer:null, callSites:1}, {clientLayer:"Store", serverLayer:null, callSites:1}]}]};
  const plan = apps.layout({width:1600, count:2});
  const places = new Map([["web", {index:0, bands:new Map([["Common", frame], ["UI", frame], ["Store", frame]])}], ["api", {index:1, bands:new Map([["Api", frame]])}]]);
  const routed = apps.routes(view, plan, places, () => 1);
  assert.equal(routed.lines.length, 1); assert.equal(routed.undrawn.get(view.interfaces[0]).length, 2);
});
