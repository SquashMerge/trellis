"use strict";
// #852: the app switcher of Treemap, Classes and the Files tree, and the one choice they share
// with Architecture. Synthetic summaries only. Run: node --test tests/app-switcher-ui.test.cjs
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const path = require("node:path");
const ROOT = "a".repeat(64), OTHER = "b".repeat(64);
const pin = {indexGeneration:"12345678-1234-4123-8123-123456789abc", indexRevision:1};
const pin2 = {...pin, indexRevision:2};
const read = file => fs.readFileSync(path.join(__dirname, "../web", file), "utf8");
const source = ["index-pin.js", "source-witness.js", "sequence.js", "shell.js", "files.js", "architecture-apps.js", "app.js"].map(read).join("\n");
const response = data => ({ok:true, status:200, json:async () => data});
function deferred() { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return {promise, resolve, reject}; }
const all = node => [node, ...(node.children || []).flatMap(all)];
const text = node => all(node).map(item => item.textContent).join(" ");
const plain = value => JSON.parse(JSON.stringify(value));
const flush = async () => { for (let i = 0; i < 20; i++) await new Promise(resolve => setImmediate(resolve)); };

// Two apps: `srv` rooted at src/ and `web` rooted at web/. `tests/browser/web.test.cjs` is web's through its
// layer although no web root holds it; `web/vendor/lib.js` has no layer and is web's by its root;
// `tools/build.py` is in no app.
const moduleRow = (file, layer, app, language = "javascript") => ({identity:{sourceSetId:"workspace", language, path:file, sid:`sid:${file}`}, layer, app});
const twoApps = [
  {name:"srv", source:"manifest", kind:"cargo", manifest:"Cargo.toml", roots:["src"], entries:["src/main.rs"], languages:["rust"], fileCount:1, unassignedFileCount:0, counts:{}},
  {name:"web", source:"manifest", kind:"npm", manifest:"web/package.json", roots:["web"], entries:[], languages:["javascript"], fileCount:3, unassignedFileCount:1, counts:{}},
];
const modules = [moduleRow("src/main.rs", "server", "srv", "rust"), moduleRow("web/app.js", "client", "web"), moduleRow("tests/browser/web.test.cjs", "web-tests", "web"),
  moduleRow("web/vendor/lib.js", null, "web"), moduleRow("tools/build.py", null, null, "python")];
const summary = (at = pin, extra = {}) => ({payload:{...at, state:"ready", rulesFile:{contentHash:"rules"}, layers:[], edges:[], interfaces:[], apps:twoApps, modules, noApp:{counts:{}, unassignedFileCount:1}, ...extra}});
const catalog = modules.map(item => ({path:item.identity.path, language:item.identity.language, methodCount:1}));

function harness() {
  const elements = new Map(), requests = [], calls = {treemap:[], classes:[], architecture:[]}, options = {};
  function node(tagName = "div") {
    const classes = new Set();
    return {tagName, dataset:{}, children:[], attrs:{}, listeners:{}, textContent:"", value:"", hidden:false, disabled:false,
      classList:{add(c) { classes.add(c); }, remove(c) { classes.delete(c); }, contains(c) { return classes.has(c); }, toggle(c, force) { const on = force ?? !classes.has(c); if (on) classes.add(c); else classes.delete(c); return on; }},
      setAttribute(k, v) { this.attrs[k] = String(v); }, getAttribute(k) { return this.attrs[k]; },
      append(...items) { this.children.push(...items); }, replaceChildren(...items) { this.children = items; this.textContent = ""; },
      addEventListener(type, fn) { this.listeners[type] = fn; }, querySelector() { return null; }, focus() { document.activeElement = this; }, scrollIntoView() {},
      contains(item) { return all(this).includes(item); }};
  }
  const get = id => { if (!elements.has(id)) { const item = node(); item.id = id; elements.set(id, item); } return elements.get(id); };
  const document = {getElementById:get, createElement:node, createElementNS:(_, tag) => node(tag), createTextNode:value => ({...node("#text"), textContent:value}), createDocumentFragment:() => node("fragment"), body:node("body"), addEventListener() {}};
  // The views are recorders: what matters here is what app.js hands them and when it calls them.
  const view = name => ({create:given => { options[name] = given; return {refresh() {}, reset() {}, clearInspection() {}, suspend() {}, reopen() {}, scopeChanged:() => calls[name].push(given.appFilter?.()?.name ?? given.appScope?.get() ?? null)}; }});
  const window = {TrellisTreemap:view("treemap"), TrellisArchitecture:view("architecture"),
    TrellisClasses:{init:given => { options.classes = given; }, reset() {}, closeContextMenu() {}, scopeChanged:() => calls.classes.push(options.classes.appFilter()?.name ?? null)}};
  let network = () => { throw new Error("Unexpected request"); };
  const context = vm.createContext({document, window, console, AbortController, DOMException, TextEncoder, TextDecoder, URLSearchParams, setTimeout:() => 0, clearTimeout() {},
    fetch(url, init) {
      requests.push(url);
      const prefix = /^\/api\/checkouts\/([a-f0-9]{64})\//.exec(url);
      assert.ok(prefix, url);
      return network("/api/" + url.slice(prefix[0].length), init);
    }});
  const run = code => vm.runInContext(code, context); run(source);
  run(`token="synthetic"; selectedRootKey="${ROOT}"; checkouts=[{rootKey:"${ROOT}", workspaceRoot:"/fixture", state:"available"}]; status={revision:${JSON.stringify(pin)}, workspaceRoot:"/fixture"};`);
  const pressed = id => get(id).children.filter(item => item.getAttribute("aria-pressed") === "true").map(item => item.textContent);
  const choices = id => get(id).children.map(item => item.textContent);
  return {get, run, requests, calls, options, pressed, choices, network(fn) { network = fn; },
    click:async (id, label) => { const pick = get(id).children.find(item => item.textContent.startsWith(label)); assert.ok(pick, `${id} offers ${label}`); await pick.listeners.click(); await flush(); }};
}
const summaryNetwork = (h, answer = () => summary()) => h.network(url => {
  if (url.startsWith("/api/architecture?")) return response(answer(url));
  throw new Error(`Unexpected request ${url}`);
});
const listed = h => all(h.get("file-tree")).filter(item => item.className === "file-pick").map(item => item.title);

test("pure assignment: each module's app, roots for the rest, folders that hold or lead to the app", () => {
  const window = {};
  vm.runInNewContext(read("architecture-apps.js"), {window});
  const A = window.TrellisArchitectureApps, assigned = A.assignment(summary().payload);
  assert.deepEqual(plain(assigned.groups.map(item => item.name)), ["srv", "web"]);
  assert.equal(A.appOf(assigned, "tests/browser/web.test.cjs"), "web", "the layer's app wins over the roots");
  assert.equal(A.appOf(assigned, "web/vendor/lib.js"), "web", "an unassigned file is its root's");
  assert.equal(A.appOf(assigned, "tools/build.py"), null, "no app");
  assert.equal(A.appOf(assigned, "web/new.js"), "web", "an unindexed path follows the roots");
  assert.equal(A.appOf(assigned, "webby/x.js"), null, "a root is a whole path component");
  assert.equal(A.holdsFile(assigned, null, "tools/build.py"), true, "All apps holds every file");
  assert.equal(A.holdsFile(assigned, "gone", "tools/build.py"), true, "a name the assignment lacks is All apps");
  assert.equal(A.holdsFile(assigned, "web", "src/main.rs"), false);
  for (const [folder, web, srv] of [["tests", true, false], ["tests/browser", true, false], ["web", true, false], ["web/vendor", true, false],
    ["src", false, true], ["tools", false, false], ["", true, true]]) {
    assert.equal(A.holdsFolder(assigned, "web", folder), web, `web ${folder}`);
    assert.equal(A.holdsFolder(assigned, "srv", folder), srv, `srv ${folder}`);
  }
  // A nested root: the folders above it lead to the app.
  const nested = A.assignment(summary(pin, {apps:[twoApps[0], {...twoApps[1], roots:["apps/web"]}], modules:[]}).payload);
  assert.equal(A.holdsFolder(nested, "web", "apps"), true);
  assert.equal(A.rootApp(nested.apps, "apps/web/x.js"), "web");
  // The most specific root wins, `.` holds everything, and the first app wins a tie.
  assert.equal(A.rootApp([{name:"all", roots:["."]}, {name:"deep", roots:["a/b"]}, {name:"tie", roots:["a/b"]}], "a/b/c"), "deep");
  assert.equal(A.rootApp([{name:"all", roots:["."]}], "x"), "all");
  // Fewer than two apps, or no ready summary, is no assignment.
  assert.equal(A.assignment(summary(pin, {apps:[twoApps[0]]}).payload), null);
  assert.equal(A.assignment({...summary().payload, state:"no_layers"}), null);
  assert.deepEqual(plain(A.choices(assigned).map(item => item.text)), ["All apps", "srv · rust", "web · javascript"]);
  assert.deepEqual(plain(A.choices(null)), []);
});

test("pure selection: memory only, per checkout, one notification per change", () => {
  const window = {};
  vm.runInNewContext(read("architecture-apps.js"), {window});
  const selection = window.TrellisArchitectureApps.selection(), heard = [];
  selection.subscribe(value => heard.push(value));
  assert.equal(selection.get(ROOT), null);
  assert.equal(selection.set(ROOT, "web"), true);
  assert.equal(selection.set(ROOT, "web"), false, "the same choice is no change");
  assert.equal(selection.get(ROOT), "web");
  assert.equal(selection.get(OTHER), null, "another checkout starts at All apps");
  selection.set(OTHER, "srv");
  assert.equal(selection.get(ROOT), null, "the choice belongs to the checkout it was made in");
  assert.deepEqual(plain(heard), ["web", "srv"]);
});

test("switcher_state: every view offers the same choices; one choice shows in all of them and routes to every view", async () => {
  const h = harness(); summaryNetwork(h);
  h.run("drawAppSwitchers()");
  for (const id of ["treemap-apps", "classes-apps", "files-apps"]) assert.equal(h.get(id).hidden, true, `${id} waits for the summary`);
  await h.run("refreshAppInventory()"); await flush();
  for (const id of ["treemap-apps", "classes-apps", "files-apps"]) {
    assert.equal(h.get(id).hidden, false);
    assert.deepEqual(h.choices(id), ["All apps", "srv · rust", "web · javascript"]);
    assert.deepEqual(h.pressed(id), ["All apps"]);
  }
  assert.equal(h.requests.filter(url => url.includes("/architecture?")).length, 1, "one summary read");
  h.calls.treemap.length = h.calls.classes.length = h.calls.architecture.length = 0;
  await h.click("treemap-apps", "web");
  for (const id of ["treemap-apps", "classes-apps", "files-apps"]) assert.deepEqual(h.pressed(id), ["web · javascript"], `${id} shows the shared choice`);
  assert.deepEqual(h.calls.treemap, ["web"]); assert.deepEqual(h.calls.classes, ["web"]); assert.deepEqual(h.calls.architecture, ["web"]);
  assert.equal(h.options.architecture.appScope.get(), "web", "Architecture reads the same choice");
  // Architecture's own switcher changes it for the others.
  h.options.architecture.appScope.set("srv"); await flush();
  assert.deepEqual(h.pressed("files-apps"), ["srv · rust"]);
  assert.equal(h.options.treemap.appFilter().name, "srv");
  assert.equal(h.options.treemap.appFilter().holds("src/main.rs"), true);
  assert.equal(h.options.treemap.appFilter().holds("tools/build.py"), false);
  // All apps hands the views no filter.
  await h.click("classes-apps", "All apps");
  assert.equal(h.options.treemap.appFilter(), null); assert.equal(h.options.classes.appFilter(), null);
  assert.equal(h.requests.filter(url => url.includes("/architecture?")).length, 1, "choosing never reads the summary again");
  // Treemap's file opener asks for a file's own app, or All apps for a file in none.
  h.options.treemap.showApp("web/app.js"); assert.equal(h.options.architecture.appScope.get(), "web");
  h.options.treemap.showApp("tools/build.py"); assert.equal(h.options.architecture.appScope.get(), null);
  // A new checkout starts at All apps.
  await h.click("files-apps", "web");
  h.run(`selectedRootKey="${OTHER}"`);
  assert.equal(h.run("chosenApp()"), null);
});

test("files_list_and_tree: one app lists its files, by layer or root, and counts the rest", async () => {
  const h = harness(); summaryNetwork(h);
  await h.run("refreshAppInventory()"); await flush();
  h.context = null;
  h.run(`files=${JSON.stringify(catalog)}; filesCatalogLoaded=true; renderFiles()`);
  assert.deepEqual(listed(h).sort(), catalog.map(file => file.path).sort(), "All apps lists every file, the one in no app too");
  await h.click("files-apps", "web");
  assert.deepEqual(listed(h).sort(), ["tests/browser/web.test.cjs", "web/app.js", "web/vendor/lib.js"], "the layer's file and the unassigned file are web's");
  assert.match(h.get("files-state").textContent, /3 of 5 loaded files · 2 in other apps or no app · All apps shows them/);
  // The directory tree, browsed at the index workspace: folders that hold or lead to the app, its files.
  const dir = (name, at = name) => ({kind:"directory", name, path:at});
  const file = (name, at = name, indexed = at) => ({kind:"file", name, path:at, indexedPath:indexed, methodCount:1});
  h.run(`treeMode=true; treeRoot="/fixture"; indexedWorkspace="/fixture"; directories.clear();
    directories.set("", {items:${JSON.stringify([dir("src"), dir("tests"), dir("tools"), dir("web"), {kind:"file", name:"README.md", path:"README.md", indexedPath:null, unindexedReason:"Not indexed"}])}, nextOffset:null, open:true});
    directories.set("web", {items:${JSON.stringify([file("app.js", "web/app.js"), dir("vendor", "web/vendor")])}, nextOffset:null, open:true});
    renderDirectoryTree()`);
  const picks = () => all(h.get("file-tree")).filter(item => /file-pick/.test(item.className || "")).map(item => item.title);
  assert.deepEqual(picks(), ["tests/", "web/", "web/app.js", "web/vendor/"]);
  assert.match(h.get("files-state").textContent, /3 entries outside web · All apps shows them/);
  await h.click("files-apps", "srv");
  assert.deepEqual(picks(), ["src/"]);
  await h.click("files-apps", "All apps");
  assert.deepEqual(picks(), ["src/", "tests/", "tools/", "web/", "web/app.js", "web/vendor/"]);
  assert.doesNotMatch(h.get("files-state").textContent, /outside/);
});

test("stale_summary: a summary read for an older pin or checkout never draws a switcher", async () => {
  for (const transition of [`status={...status, revision:${JSON.stringify(pin2)}}`, "epoch++", `selectedRootKey="${OTHER}"`]) {
    const h = harness(), gate = deferred();
    h.network(url => { if (url.startsWith("/api/architecture?")) return gate.promise; throw new Error(url); });
    const pending = h.run("refreshAppInventory()");
    h.run(transition);
    gate.resolve(response(summary())); await pending.catch(() => {}); await flush();
    for (const id of ["treemap-apps", "classes-apps", "files-apps"]) assert.equal(h.get(id).hidden, true, `${transition}: ${id}`);
    assert.equal(h.run("appAssignment()"), null, transition);
  }
  // The current pin's summary still arrives after an old one is dropped.
  const h = harness(), first = deferred();
  let reads = 0;
  h.network(url => { if (!url.startsWith("/api/architecture?")) throw new Error(url); reads++; return reads === 1 ? first.promise : Promise.resolve(response(summary(pin2))); });
  const old = h.run("refreshAppInventory()");
  h.run(`status={...status, revision:${JSON.stringify(pin2)}}`);
  await h.run("refreshAppInventory()"); await flush();
  first.resolve(response(summary())); await old.catch(() => {}); await flush();
  assert.deepEqual(h.choices("files-apps"), ["All apps", "srv · rust", "web · javascript"]);
  assert.deepEqual(h.requests.filter(url => url.includes("/architecture?")).map(url => /indexRevision=(\d+)/.exec(url)[1]), ["1", "2"]);
});

test("summary_ends_choice: a summary without the chosen app ends it; a rules change reads the assignment again", async () => {
  const h = harness();
  let answer = summary();
  summaryNetwork(h, () => answer);
  await h.run("refreshAppInventory()"); await flush();
  await h.click("files-apps", "web");
  answer = summary(pin, {apps:[twoApps[0], {...twoApps[1], name:"client"}]});
  h.run("clearRevisionReads()"); await flush();
  assert.deepEqual(h.choices("files-apps"), ["All apps", "srv · rust", "client · javascript"]);
  assert.deepEqual(h.pressed("files-apps"), ["All apps"], "web is gone, so the choice is All apps");
  assert.equal(h.options.architecture.appScope.get(), null, "and it does not come back");
});

// Negative control: with one app, or no rules file, there is no switcher and nothing is filtered.
test("negative control: one app or no rules file draws no switcher and lists every file", async () => {
  for (const answer of [summary(pin, {apps:[twoApps[0]]}), {payload:{...pin, state:"missing_rules", rulesFile:null, apps:[], modules:[], layers:[]}}]) {
    const h = harness(); summaryNetwork(h, () => answer);
    await h.run("refreshAppInventory()"); await flush();
    for (const id of ["treemap-apps", "classes-apps", "files-apps"]) assert.equal(h.get(id).hidden, true, id);
    h.run("chooseApp('web')"); await flush();
    assert.equal(h.run("chosenApp()"), null, "nothing to choose");
    h.run(`files=${JSON.stringify(catalog)}; filesCatalogLoaded=true; renderFiles()`);
    assert.equal(listed(h).length, catalog.length);
    assert.equal(h.options.treemap.appFilter(), null);
  }
});

// #852 review (Prime, PR #875): an Architecture summary under new rules at the same pin (here the
// uncached sum aggregation) drops the shared assignment, so Files, Treemap and Classes follow it.
test("rules_change_seen_in_a_summary: a summary with a new rules hash rereads the shared assignment", async () => {
  const h = harness();
  const moved = modules.map(item => item.identity.path === "web/app.js" ? {...item, app:"srv"} : item);
  let rules = "rules";
  h.network(url => {
    if (!url.startsWith("/api/architecture?")) throw new Error(url);
    const aggregate = new URLSearchParams(url.split("?")[1]).get("aggregate");
    return response(summary(pin, {aggregate, rulesFile:{contentHash:rules}, ...(rules === "rules" ? {} : {modules:moved})}));
  });
  await h.run("refreshAppInventory()"); await flush();
  await h.click("files-apps", "web");
  assert.equal(h.run("appFileFilter()('web/app.js')"), true);
  const sum = `/api/architecture?aggregate=sum&role=production&workbench=true&indexGeneration=${pin.indexGeneration}&indexRevision=1`;
  // Control: a summary under the same rules leaves the assignment and reads nothing more.
  await h.run(`architectureRequest(${JSON.stringify(sum)}, () => true)`); await flush();
  const reads = h.requests.filter(url => url.includes("/architecture?")).length;
  assert.equal(reads, 2); assert.equal(h.run("appFileFilter()('web/app.js')"), true);
  // The rules change: Architecture reads the sum summary under them.
  rules = "new-rules";
  await h.run(`architectureRequest(${JSON.stringify(sum.replace("sum", "mean"))}, () => true)`); await flush();
  assert.equal(h.run("Apps.appOf(appAssignment(), 'web/app.js')"), "srv", "the shared assignment is the new rules'");
  assert.equal(h.run("appFileFilter()('web/app.js')"), false, "the Files web filter drops the moved file");
  assert.equal(h.options.treemap.appFilter().holds("web/app.js"), false, "and so does Treemap's");
  assert.deepEqual(h.pressed("files-apps"), ["web · javascript"], "the choice stays");
  assert.ok(h.requests.filter(url => url.includes("/architecture?")).at(-1).includes("aggregate=max"), "the assignment was read again under the new rules");
});
