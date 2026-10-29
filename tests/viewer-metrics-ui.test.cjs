"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const generation = "12345678-1234-4123-8123-123456789abc";
const pin = {indexGeneration:generation, indexRevision:1};
function harness() {
  const elements = new Map(), requests = [], conflicts = [];
  let changes = 0, shown = 0, cleared = 0;
  const state = {rootKey:"checkout-a", session:1, pin:{...pin}, treeRoot:"/repo", indexedWorkspace:"/repo"};
  function node(tagName) {
    return {tagName, children:[], textContent:"", className:"", attrs:{}, listeners:{}, value:"", checked:false, parent:null,
      append(...items) { for (const item of items) { item.parent = this; this.children.push(item); } },
      replaceChildren(...items) { this.children = []; this.textContent = ""; this.append(...items); },
      remove() { if (this.parent) this.parent.children = this.parent.children.filter(item => item !== this); this.parent = null; },
      setAttribute(key, value) { this.attrs[key] = String(value); },
      addEventListener(type, callback) { this.listeners[type] = callback; }};
  }
  const get = id => { if (!elements.has(id)) elements.set(id, node("div")); return elements.get(id); };
  const document = {getElementById:get, createElement:node};
  const window = {TrellisShell:{showMetricInspector() { shown++; }, clearMetricInspector() { cleared++; }}};
  const context = vm.createContext({window, document, URLSearchParams});
  vm.runInContext(fs.readFileSync(path.join(__dirname, "../web/metric-heat.js"), "utf8"), context);
  vm.runInContext(fs.readFileSync(path.join(__dirname, "../web/viewer-metrics.js"), "utf8"), context);
  const controller = window.TrellisMetrics.create({context:() => state, request(url) {
    return new Promise((resolve, reject) => requests.push({url, resolve, reject}));
  }, onChange() { changes++; }, onConflict(error) { conflicts.push(error); }});
  const change = (id, value) => {
    if (typeof value === "boolean") get(id).checked = value; else get(id).value = value;
    get(id).listeners.change({target:get(id)});
  };
  return {controller, helpers:window.TrellisMetrics, state, requests, conflicts, get, node, change,
    counts:() => ({changes, shown, cleared})};
}
const text = node => [node.textContent, ...node.children.map(text)].join(" ");
const row = (path, value, extra = {}) => ({sourceSetId:"source-set:v1:test", language:"javascript", path, declarationCount:1, roleCounts:{production:1, test:0},
  metrics:{cognitive:value, cyclomatic:0, maintainabilityIndex:value, halstead:{volume:value}}, ...extra});
const payload = (files = [], options = {}) => ({payload:{workbench:true,metricsVersion:"metrics:v1", ...pin, scope:"revision", aggregate:"max", role:"production", files,
  directories:[], ...options}});
const settle = async () => { await Promise.resolve(); await Promise.resolve(); };
async function publish(h, files, options) {
  const work = h.controller.refresh(); h.requests.at(-1).resolve(payload(files, options)); await work;
}

test("quartiles preserve zero, null, ties, empty and strict cuts; maintainability reverses severity only", () => {
  const {helpers} = harness();
  assert.deepEqual(Array.from(helpers.quartiles([null, 0, 2, 5, 10, NaN, Infinity])), [2, 5, 10]);
  assert.deepEqual(Array.from(helpers.quartiles([])), []);
  assert.deepEqual(Array.from(helpers.quartiles([7])), [7, 7, 7]);
  assert.deepEqual(Array.from(helpers.quartiles([2, 2, 2, 2])), [2, 2, 2]);
  assert.equal(helpers.band(2, [2, 5, 10], "cognitive"), 0);
  assert.equal(helpers.band(5, [2, 5, 10], "cognitive"), 1);
  assert.equal(helpers.band(10, [2, 5, 10], "cognitive"), 2);
  assert.equal(helpers.band(5, [2, 5, 10], "maintainabilityIndex"), 2);
  assert.equal(helpers.band(null, [2, 5, 10], "cognitive"), null);
});

test("one pinned workbench request; fixed heat is stable and Relative uses displayed rows without fetching", async () => {
  const h = harness();
  await publish(h, [row("zero.js", 0), row("two.js", 2), row("five.js", 5), row("ten.js", 10), row("hidden.js", 1000), row("empty.js", null)]);
  const query = new URL(h.requests[0].url, "http://local").searchParams;
  assert.equal(query.get("scope"), "revision"); assert.equal(query.get("aggregate"), "max");
  assert.equal(query.get("workbench"), "true"); assert.equal(query.get("role"), "production"); assert.equal(h.get("metric-include-tests").checked, false);
  assert.match(text(h.get("metrics-state")), /production callables/);
  assert.equal(query.get("indexGeneration"), generation); assert.equal(query.get("indexRevision"), "1");
  const displayed = ["zero.js", "two.js", "five.js", "ten.js", "empty.js"].map(path => h.controller.decorate(h.node("button"), {scope:"file", path}));
  h.controller.renderLegend(displayed);
  assert.match(text(h.get("metric-legend")), /Fixed per-callable bands.*≤5.*>5–15.*>15–30.*>30/);
  assert.match(displayed[3].swatch.className, /heat-1/);
  h.controller.renderLegend([displayed[3]]);
  assert.match(displayed[3].swatch.className, /heat-1/, "filtering cannot change fixed severity");
  assert.equal(h.get("metric-relative").checked, false);
  h.change("metric-relative", true); h.controller.renderLegend(displayed);
  assert.equal(h.requests.length, 1);
  assert.match(text(h.get("metric-legend")), /[Rr]elative to visible set.*≤2.*>2–5.*>5–10.*>10/);
  h.change("metric-relative", false);
  assert.match(text(displayed[0].node), /0/); assert.match(text(displayed[4].node), /—/);
  assert.match(displayed[0].swatch.className, /heat-0/);
  assert.match(displayed[4].swatch.className, /neutral/);
  h.change("metric-color", "maintainabilityIndex");
  assert.equal(h.requests.length, 1);
  const mi = h.controller.decorate(h.node("button"), {scope:"file", path:"zero.js"});
  h.controller.renderLegend([mi]);
  assert.match(text(mi.node), /0/); assert.match(mi.swatch.className, /heat-3/);
  assert.match(text(h.get("metric-legend")), /higher is better/);
});

test("no numeric samples show neutral legend; no indexed selection stays unavailable", async () => {
  const h = harness(); await publish(h, [row("empty.js", null)]);
  const empty = h.controller.decorate(h.node("button"), {scope:"file", path:"empty.js"});
  const outside = h.controller.decorate(h.node("button"), null);
  h.controller.renderLegend([empty, outside]);
  assert.match(text(h.get("metric-legend")), /No measured values/);
  assert.match(text(outside.node), /—/); assert.match(outside.swatch.className, /neutral/);
  await h.controller.inspect(null); assert.equal(h.requests.length, 1);
});

test("ambiguous source/language paths are never guessed and optional language can select a unique file", async () => {
  const h = harness();
  await publish(h, [row("same", 1), row("same", 2, {language:"rust"}), row("duplicate", 3), row("duplicate", 4, {sourceSetId:"other"})]);
  await h.controller.inspect({scope:"file", path:"same"});
  await h.controller.inspect({scope:"file", path:"duplicate"});
  assert.equal(h.requests.length, 1); assert.match(text(h.get("metric-inspector-state")), /no unique indexed source identity/);
  const work = h.controller.inspect({scope:"file", path:"same", language:"rust"});
  const query = new URL(h.requests.at(-1).url, "http://local").searchParams;
  assert.equal(query.get("language"), "rust"); assert.equal(query.get("topCognitive"), "3");
  h.requests.at(-1).resolve(payload([row("same", 2, {language:"rust"})], {scope:"file", topCognitive:[]})); await work;
});

test("single selected-scope request renders every nested leaf, exact unavailable reason, raw callable identity and safe text", async () => {
  const h = harness();
  const metrics = {loc:0, sloc:null, commentLines:2, cyclomatic:0, cognitive:0, maxNesting:0,
    halstead:{volume:0, difficulty:1, effort:2, bugs:0.1}, maintainabilityIndex:90,
    fanIn:{value:null, unavailable:"call resolution not yet produced (#40/#68)"}, fanOut:0, parameters:0, exits:{return:0, throw:1}};
  const file = row("<img>.js", 0, {metrics}); await publish(h, [file]);
  const work = h.controller.inspect({scope:"file", path:file.path});
  h.requests.at(-1).resolve(payload([file], {scope:"file", topCognitive:[{...file, role:"production", name:"<script>alert(1)</script>", kind:"function", sid:"not-a-graph-id",
    range:{start:0, end:17}, metrics:{cognitive:4, cyclomatic:2, maxNesting:1}}]})); await work;
  assert.equal(h.requests.length, 2);
  const values = text(h.get("metric-inspector-values"));
  assert.match(values, /Halstead · volume\s+0/); assert.match(values, /Exits · throw\s+1/);
  assert.match(values, /Fan-in\s+— — call resolution not yet produced \(#40\/#68\)/);
  assert.match(values, /SLOC \(callable ranges\)\s+—/);
  const top = h.get("metric-inspector-top");
  assert.match(text(top), /<script>alert\(1\)<\/script>/); assert.match(text(top), /UTF-8 bytes \[0, 17\)/);
  assert.match(text(top), /Cognitive 4 · Cyclomatic 2 · Max nesting 1/);
  assert.equal(top.children[1].tagName, "li");
  assert.equal(top.children[1].children[0].textContent, "<script>alert(1)</script>");
  assert.equal(top.children[1].children.some(item => item.tagName === "a" || item.tagName === "button"), false);
});

test("directory uses its server aggregate without child fanout; empty callables stay null", async () => {
  const h = harness(); const directory = {sourceSetId:"source-set:v1:test", path:"src", declarationCount:0, metrics:{cognitive:null}};
  await publish(h, [], {directories:[directory]});
  const work = h.controller.inspect({scope:"directory", path:"src"});
  const query = new URL(h.requests.at(-1).url, "http://local").searchParams;
  assert.equal(query.get("scope"), "directory"); assert.equal(query.has("language"), false);
  h.requests.at(-1).resolve(payload([], {scope:"directory", directories:[directory], topCognitive:[]})); await work;
  assert.equal(h.requests.length, 2); assert.match(text(h.get("metric-inspector-state")), /No production callables/);
  assert.match(text(h.get("metric-inspector-values")), /Cognitive\s+—/);
});

test("late revision success/error is inert after reset, root, session or full pin changes", async () => {
  for (const change of [h => h.controller.reset(), h => h.state.rootKey = "checkout-b", h => h.state.session++,
    h => h.state.pin = {...pin, indexGeneration:"another-generation"}, h => h.state.pin = {...pin, indexRevision:2}]) {
    for (const reject of [false, true]) {
      const h = harness(); const work = h.controller.refresh(); change(h);
      const before = JSON.stringify({counts:h.counts(), state:text(h.get("metrics-state")), conflicts:h.conflicts});
      if (reject) h.requests[0].reject(Object.assign(new Error("old failure"), {status:409, code:"revision_conflict"}));
      else h.requests[0].resolve(payload([row("old.js", 99)]));
      await work;
      assert.equal(JSON.stringify({counts:h.counts(), state:text(h.get("metrics-state")), conflicts:h.conflicts}), before);
    }
  }
});

test("aggregation clears old swatches and fences old refresh; selection is reinspected at new aggregate", async () => {
  const h = harness(); await publish(h, [row("file.js", 7)]);
  const oldSelection = h.controller.inspect({scope:"file", path:"file.js"});
  const decorated = h.controller.decorate(h.node("button"), {scope:"file", path:"file.js"});
  h.change("metric-aggregate", "mean"); assert.equal(decorated.node.parent, null);
  assert.match(h.requests[2].url, /aggregate=mean/);
  h.requests[1].reject(new Error("late old selected failure")); await oldSelection;
  assert.doesNotMatch(text(h.get("metric-inspector-state")), /late old selected failure/);
  h.requests[2].resolve(payload([row("file.js", 3)], {aggregate:"mean"})); await settle();
  assert.equal(h.requests.length, 4); assert.match(h.requests[3].url, /aggregate=mean/);
  h.requests[3].resolve(payload([row("file.js", 3)], {aggregate:"mean", scope:"file", topCognitive:[]})); await settle();
  assert.match(text(h.get("metric-inspector-values")), /Cognitive\s+3/);
});

test("inspection replacement/clear fences both late outcomes and reset cannot reopen inspector", async () => {
  for (const reject of [false, true]) {
    const h = harness(); await publish(h, [row("a.js", 1), row("b.js", 2)]);
    const first = h.controller.inspect({scope:"file", path:"a.js"});
    const second = h.controller.inspect({scope:"file", path:"b.js"});
    h.requests[2].resolve(payload([row("b.js", 2)], {scope:"file", topCognitive:[]})); await second;
    const before = text(h.get("metric-inspector-values"));
    if (reject) h.requests[1].reject(Object.assign(new Error("late inspection conflict"), {status:409, code:"revision_conflict"}));
    else h.requests[1].resolve(payload([row("a.js", 99)], {scope:"file"}));
    await first; assert.equal(text(h.get("metric-inspector-values")), before); assert.equal(h.conflicts.length, 0);
    const pending = h.controller.inspect({scope:"file", path:"a.js"}); h.controller.clearInspection();
    const shown = h.counts().shown;
    h.requests[3].resolve(payload([row("a.js", 9)], {scope:"file"})); await pending;
    assert.equal(h.counts().shown, shown);
  }
});

test("current pair conflict calls conflict hook; honest timeout remains visible without retry", async () => {
  const h = harness(); const first = h.controller.refresh();
  h.requests[0].resolve(payload([], {indexRevision:2})); await first;
  assert.equal(h.conflicts.length, 1); assert.match(text(h.get("metrics-state")), /snapshot changed/);
  const second = h.controller.refresh();
  h.requests[1].reject(Object.assign(new Error("Request timed out. No automatic retry was made."), {name:"TimeoutError"})); await second;
  assert.match(text(h.get("metrics-state")), /Request timed out. No automatic retry/);
  assert.equal(h.requests.length, 2);
});


test("new aggregation revision wins over late prior success and error", async () => {
  for (const reject of [false, true]) {
    const h = harness(); const old = h.controller.refresh();
    h.change("metric-aggregate", "sum");
    h.requests[1].resolve(payload([row("current.js", 3)], {aggregate:"sum"})); await settle();
    const before = {changes:h.counts().changes, state:text(h.get("metrics-state"))};
    if (reject) h.requests[0].reject(Object.assign(new Error("old max conflict"), {status:409, code:"revision_conflict"}));
    else h.requests[0].resolve(payload([row("current.js", 99)]));
    await old;
    assert.deepEqual({changes:h.counts().changes, state:text(h.get("metrics-state"))}, before);
    const visible = h.controller.decorate(h.node("button"), {scope:"file", path:"current.js"});
    assert.match(text(visible.node), /3/); assert.equal(h.conflicts.length, 0);
  }
});

test("role change clears inspection and heat, and fences late same-pin refresh and inspection outcomes", async () => {
  for (const reject of [false, true]) {
    const h = harness(); await publish(h, [row("mixed.rs", 2, {roleCounts:{production:1,test:1}})]);
    const inspection = h.controller.inspect({scope:"file", path:"mixed.rs"});
    const old = h.controller.refresh();
    const decoration = h.controller.decorate(h.node("button"), {scope:"file", path:"mixed.rs"});
    const cleared = h.counts().cleared;
    h.change("metric-include-tests", true);
    assert.equal(decoration.node.parent, null); assert.ok(h.counts().cleared > cleared);
    assert.equal(new URL(h.requests[3].url, "http://local").searchParams.get("role"), "all");
    h.requests[3].resolve(payload([row("mixed.rs", 9, {roleCounts:{production:1,test:1}})], {role:"all"})); await settle();
    const before = {changes:h.counts().changes, shown:h.counts().shown, state:text(h.get("metrics-state"))};
    for (const index of [1, 2]) {
      if (reject) h.requests[index].reject(Object.assign(new Error("late production conflict"), {status:409, code:"revision_conflict"}));
      else h.requests[index].resolve(payload([row("mixed.rs", 99)], {scope:index === 1 ? "file" : "revision", topCognitive:[]}));
    }
    await Promise.all([inspection, old]);
    assert.deepEqual({changes:h.counts().changes, shown:h.counts().shown, state:text(h.get("metrics-state"))}, before);
    assert.equal(h.conflicts.length, 0); assert.equal(h.requests.length, 4);
    assert.match(before.state, /all indexed callables/);
    const work = h.controller.inspect({scope:"file", path:"mixed.rs"});
    assert.match(h.requests[4].url, /role=all/);
    h.requests[4].resolve(payload([row("mixed.rs", 9)], {role:"all", scope:"file", topCognitive:[]})); await work;
    assert.match(text(h.get("metric-inspector-scope")), /all indexed callables/);
    assert.match(text(h.get("metric-inspector-values")), /Cognitive\s+9/);
    h.change("metric-include-tests", false);
    assert.match(h.requests[5].url, /role=production/);
    h.requests[5].resolve(payload([])); await settle();
  }
});

test("response role must echo selection; test-only file has unavailable production heat and honest empty inspection", async () => {
  const h = harness(); const wrong = h.controller.refresh();
  h.requests[0].resolve(payload([row("test.rs", 99)], {role:"all"})); await wrong;
  assert.match(text(h.get("metrics-state")), /Invalid metrics response/);
  const file = row("test.rs", null, {declarationCount:0, roleCounts:{production:0,test:2}});
  await publish(h, [file]);
  const decoration = h.controller.decorate(h.node("button"), {scope:"file", path:"test.rs"});
  h.controller.renderLegend([decoration]);
  assert.match(text(decoration.node), /—/); assert.match(decoration.swatch.className, /neutral/);
  assert.match(text(h.get("metric-legend")), /production callables/);
  const wrongInspection = h.controller.inspect({scope:"file", path:"test.rs"});
  h.requests.at(-1).resolve(payload([row("test.rs", 99)], {scope:"file", role:"all", topCognitive:[]})); await wrongInspection;
  assert.match(text(h.get("metric-inspector-state")), /Invalid metrics response/);
  const empty = h.controller.inspect({scope:"file", path:"test.rs"});
  h.requests.at(-1).resolve(payload([file], {scope:"file", topCognitive:[]})); await empty;
  assert.match(text(h.get("metric-inspector-state")), /No production callables.*unavailable, not zero/);
  assert.match(text(h.get("metric-inspector-top")), /Highest cognitive complexity · production callables.*No production callables/);
});


test("excluded file counts use selected minus raw exactly once and sum requires explicit Relative", async () => {
  const h = harness(), fixture = row("fixtures/input.js", null, {declarationCount:0,roleCounts:{production:2,test:1},excludedReason:"fixture"});
  await publish(h, [row("mixed.js", 7, {roleCounts:{production:1,test:2}}), fixture]);
  assert.match(text(h.get("metrics-state")), /5 callables excluded from heat.*1 fixture files/);
  const inspection = h.controller.inspect({scope:"file",path:fixture.path});
  h.requests.at(-1).resolve(payload([fixture], {scope:"file",topCognitive:[]})); await inspection;
  assert.match(text(h.get("metric-inspector-state")), /Excluded from heat: fixture/);
  h.controller.clearInspection(); h.change("metric-aggregate", "sum");
  h.requests.at(-1).resolve(payload([row("sum.js", 100)], {aggregate:"sum"})); await settle();
  const item = h.controller.decorate(h.node("button"), {scope:"file",path:"sum.js"});
  h.controller.renderLegend([item]); assert.match(item.swatch.className, /neutral/);
  assert.match(text(h.get("metric-legend")), /relative totals; no callable risk bands/);
  const count = h.requests.length; h.change("metric-relative", true); h.controller.renderLegend([item]);
  assert.match(item.swatch.className, /heat-0/); assert.equal(h.requests.length, count);
});
