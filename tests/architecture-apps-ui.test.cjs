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
  return {tagName, textContent:"", children:[], dataset:{}, attrs:{}, listeners:{}, hidden:false, value:"", checked:false,
    append(...items) { this.children.push(...items); }, replaceChildren(...items) { this.children = [...items]; this.textContent = ""; },
    setAttribute(key, value) { this.attrs[key] = String(value); }, focus() {},
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
function controller(payload) {
  const elements = new Map(), created = [], renders = [], calls = [];
  const get = id => created.findLast(item => item.id === id) || (elements.has(id) ? elements.get(id) : elements.set(id, node(id)).get(id));
  const document = {querySelector:() => null, getElementById:get, createElement:tag => { const item = node(tag); created.push(item); return item; }};
  const window = {TrellisArchitecturePele:{render:async (_container, view, options) => { renders.push({view, options}); return {status:"ready"}; }},
    TrellisShell:{clearArchitectureInspector() {}, showArchitectureInspector() {}}};
  vm.runInNewContext([pinSource, read("web/metric-heat.js"), read("web/architecture-apps.js"), read("web/architecture-deviations.js"), read("web/architecture.js")].join("\n"),
    {window, document, URLSearchParams, console, setTimeout, clearTimeout});
  const architecture = window.TrellisArchitecture.create({
    request:async url => { calls.push(url); return {payload}; },
    context:() => ({rootKey:"checkout", session:1, pin}), readSource() {}, onConflict() {}});
  return {architecture, get, renders, calls, window};
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
