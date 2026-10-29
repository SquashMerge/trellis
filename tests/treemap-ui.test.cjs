"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const source = fs.readFileSync(path.join(__dirname, "../web/treemap.js"), "utf8");
const pinSource = fs.readFileSync(path.join(__dirname, "../web/app.js"), "utf8").match(/const IndexPin = Object.freeze\(\{[\s\S]*?\n\}\);\nwindow.TrellisIndexPin = IndexPin;/)[0];
const pin = {indexGeneration:"12345678-1234-4123-8123-123456789abc", indexRevision:1};
const descendants = node => [node, ...node.children.flatMap(descendants)];
const text = node => descendants(node).map(item => item.textContent).join(" ");
const deferred = () => { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return {promise, resolve, reject}; };
const file = (path, production, tests, physicalLines, fileRole = "production", sourceSetId = "workspace", language = "rust") => ({
  sourceSetId, language, path, physicalLines, fileRole, roleCounts:{production, test:tests}, declarationCount:production,
  metrics:{cognitive:production ? 3 : null, cyclomatic:production ? 4 : null, maxNesting:production ? 2 : null,
    maintainabilityIndex:production ? 80 : null, halstead:{volume:production ? 12 : null}, fanOut:production ? 1 : null,
    fanIn:{value:null, unavailable:"not-measured"}},
});
function fixture(aggregate = "max", role = "production") {
  const files = [file("src/main.rs", 2, 1, 8), file("tests/main.rs", 0, 2, 5, "test"),
    file("empty.rs", 0, 0, 0), file("tests/empty.rs", 0, 0, 0, "test"), file("src/no-callables.rs", 0, 0, 3),
    file("src/main.rs", 1, 0, 4, "production", "dependency"), file("lib.py", 1, 0, 2, "production", "workspace", "python")];
  if (role === "all") for (const row of files) { row.declarationCount += row.roleCounts.test; if (row.declarationCount) row.metrics.cognitive = 7; }
  return {...pin, aggregate, role, workbench:true,metricsVersion:"metrics:v1", scope:"revision", files, directories:[]};
}
function harness() {
  const elements = new Map(), calls = [], renders = [], reads = [], conflicts = [];
  let context = {rootKey:"checkout-one", session:1, pin}, response = async () => ({payload:fixture()});
  function node(tagName) {
    return {tagName, textContent:"", children:[], attrs:{}, dataset:{}, listeners:{}, hidden:false, value:"", checked:false,
      append(...items) { this.children.push(...items); }, replaceChildren(...items) { this.children = [...items]; this.textContent = ""; },
      setAttribute(key, value) { this.attrs[key] = String(value); }, getAttribute(key) { return this.attrs[key]; },
      querySelectorAll(selector) { assert.equal(selector, "[data-trellis-file-id]"); return descendants(this).filter(item => item.attrs?.["data-trellis-file-id"] !== undefined); },
      addEventListener(name, fn) { (this.listeners[name] ||= []).push(fn); },
      async fire(name) { for (const fn of this.listeners[name] || []) await fn({target:this}); },
    };
  }
  const document = {getElementById:id => { if (!elements.has(id)) elements.set(id, node(id)); return elements.get(id); }, createElement:node};
  const get = document.getElementById;
  let renderer = async (container, files, options) => {
    renders.push({files, options});
    for (const file of files.filter(file => file.physicalLines > 0)) {
      const tile = node("g"); tile.setAttribute("data-trellis-file-id", JSON.stringify([file.sourceSetId, file.language, file.path])); container.append(tile);
    }
    return {status:"ready"};
  };
  const window = {TrellisTreemapPele:{render:(...args) => renderer(...args), select:(container, key) => {
    for (const tile of container.querySelectorAll("[data-trellis-file-id]")) tile.setAttribute("aria-pressed", String(tile.getAttribute("data-trellis-file-id") === key));
  }}, TrellisShell:{
    clearTreemapInspector:() => { get("treemap-inspector").hidden = true; get("treemap-inspector-content").replaceChildren(); },
    showTreemapInspector:() => { get("treemap-inspector").hidden = false; },
  }};
  vm.runInNewContext(pinSource + "\n" + fs.readFileSync(path.join(__dirname, "../web/metric-heat.js"), "utf8") + "\n" + source, {window, document, URLSearchParams, console});
  const controller = window.TrellisTreemap.create({request:(url, current) => { calls.push({url, current}); return response(url); }, context:() => context,
    readSource:(site, revision, current) => reads.push({site, revision, current}), onConflict:error => conflicts.push(error)});
  const button = label => { const result = descendants(get("treemap-inspector-content")).find(item => item.tagName === "button" && item.textContent === label); assert.ok(result, `Missing ${label}`); return result; };
  return {window, controller, calls, renders, reads, conflicts, get, button, setContext:value => {context = value;}, setResponse:fn => {response = fn;}, setRenderer:fn => {renderer = fn;}};
}

test("inert defaults request one complete revision pin and use physical lines independently of callable heat", async () => {
  const h = harness(); assert.equal(h.calls.length, 0);
  assert.equal(h.get("treemap-color").value, "cognitive"); assert.equal(h.get("treemap-hide-tests").checked, true);
  assert.equal(h.get("treemap-metric-tests").checked, false); await h.controller.refresh();
  const query = new URL(h.calls[0].url, "http://local").searchParams;
  for (const [key, expected] of Object.entries({scope:"revision", aggregate:"max", role:"production", workbench:"true", indexGeneration:pin.indexGeneration, indexRevision:"1"})) assert.equal(query.get(key), expected);
  const {files, options} = h.renders[0]; assert.equal(files.length, 5); assert.ok(files.some(file => file.physicalLines === 0));
  const mixed = files.find(file => file.path === "src/main.rs" && file.sourceSetId === "workspace");
  assert.equal(options.fileValue(mixed), 3); assert.match(h.get("treemap-state").textContent, /17 physical file lines/);
  assert.equal(h.reads.length, 0); options.onSelectFile(mixed);
  const content = text(h.get("treemap-inspector-content"));
  assert.match(content, /8 physical file lines · includes inline test lines/); assert.match(content, /mixed callable population/);
  assert.match(content, /2 production \/ 1 test/); assert.match(content, /2 selected production callables/); assert.match(content, /not-measured/);
  await h.button("Read indexed file source").fire("click"); assert.equal(h.reads.length, 1);
  assert.equal(h.reads[0].site.sourceSetId, "workspace"); assert.equal(h.reads[0].site.language, "rust"); assert.equal(h.reads[0].site.path, mixed.path);
  assert.equal(h.reads[0].site.range.endLine, 8); assert.equal(h.reads[0].revision.indexGeneration, pin.indexGeneration);
  assert.equal(h.reads[0].current(), true); h.controller.clearInspection(); assert.equal(h.reads[0].current(), false);
});

test("include tests affects only metrics; hide tests affects only file visibility and root role hides empty tests", async () => {
  const h = harness(); await h.controller.refresh();
  h.setResponse(async () => ({payload:fixture("max", "all")})); h.get("treemap-metric-tests").checked = true;
  await h.get("treemap-metric-tests").fire("change"); assert.equal(h.renders.at(-1).files.length, 5);
  assert.match(h.calls.at(-1).url, /role=all/); assert.match(h.get("treemap-state").textContent, /17 physical file lines/);
  h.get("treemap-hide-tests").checked = false; await h.get("treemap-hide-tests").fire("change");
  assert.equal(h.calls.length, 2); assert.equal(h.renders.at(-1).files.length, 7); assert.match(h.get("treemap-state").textContent, /22 physical file lines/);
  h.setResponse(async () => ({payload:fixture()})); h.get("treemap-metric-tests").checked = false; await h.get("treemap-metric-tests").fire("change");
  const {files, options} = h.renders.at(-1); const testOnly = files.find(file => file.path === "tests/main.rs");
  assert.equal(options.fileValue(testOnly), null); options.onSelectFile(testOnly);
  assert.match(text(h.get("treemap-inspector-content")), /No production callables.*unavailable, not zero/);
  h.get("treemap-color").value = "role"; await h.get("treemap-color").fire("change");
  const role = h.renders.at(-1); assert.equal(role.options.fileBand(role.files.find(file => file.path === "tests/empty.rs")), "none");
  assert.equal(role.options.fileBand(role.files.find(file => file.path === "tests/main.rs")), "test");
  assert.equal(role.options.fileBand(role.files.find(file => file.path === "src/main.rs" && file.sourceSetId === "workspace")), "mixed");
  assert.match(text(h.get("treemap-legend")), /production-only \/ mixed \/ test-only \/ no measured callables/);
});

test("keyboard-equivalent list selects exact duplicate paths including zero-area files and updates selection ring", async () => {
  const h = harness(); await h.controller.refresh();
  const buttons = h.get("treemap-files").children; assert.equal(buttons.length, 5);
  const empty = buttons.find(button => button.textContent.includes("zero area")); await empty.fire("click");
  assert.equal(h.get("treemap-inspector-title").textContent, "empty.rs"); assert.match(text(h.get("treemap-inspector-content")), /Empty indexed file · zero area/);
  assert.equal(descendants(h.get("treemap-inspector-content")).some(node => node.textContent === "Read indexed file source"), false);
  const other = buttons.find(button => button.textContent.includes("dependency")); await other.fire("click");
  assert.equal(other.getAttribute("aria-pressed"), "true"); assert.equal(empty.getAttribute("aria-pressed"), "false");
  const marked = h.get("treemap-diagram").children.filter(node => node.getAttribute("aria-pressed") === "true"); assert.equal(marked.length, 1);
  assert.equal(marked[0].getAttribute("data-trellis-file-id"), '["dependency","rust","src/main.rs"]');
  await h.button("Read indexed file source").fire("click"); assert.equal(h.reads[0].site.sourceSetId, "dependency");
});

test("directory drills use exact source sets, breadcrumbs go up, language/path filters recompute area", async () => {
  const h = harness(); await h.controller.refresh();
  await h.renders[0].options.onSelectDirectory({sourceSetId:"workspace", path:"src"});
  assert.equal(h.renders.at(-1).files.length, 2); assert.match(h.get("treemap-state").textContent, /11 physical file lines/);
  assert.deepEqual(h.get("treemap-breadcrumbs").children.map(node => node.textContent), ["All indexed files", "workspace", "src"]);
  await h.get("treemap-breadcrumbs").children[0].fire("click");
  h.get("treemap-language").value = "python"; await h.get("treemap-language").fire("change");
  assert.equal(h.renders.at(-1).files.length, 1); assert.match(h.get("treemap-state").textContent, /2 physical file lines/);
  h.get("treemap-path").value = "missing"; await h.get("treemap-path").fire("input");
  assert.equal(h.get("treemap-state").dataset.state, "empty"); assert.equal(h.get("treemap-files").children.length, 0); assert.equal(h.calls.length, 1);
});

test("recolor uses visible quartiles, ties stay together, only MI inverts and unavailable values are not zero", async () => {
  const h = harness(), api = h.window.TrellisTreemap, cuts = api.quartiles([1,1,1,9,null,NaN,Infinity]);
  assert.deepEqual(Array.from(cuts), [1,1,9]); assert.equal(api.band(1,cuts,"cognitive"),0); assert.equal(api.band(1,cuts,"maintainabilityIndex"),3); assert.equal(api.band(null,cuts,"cognitive"),null);
  await h.controller.refresh(); h.get("treemap-color").value = "maintainabilityIndex"; await h.get("treemap-color").fire("change");
  assert.equal(h.calls.length, 1); assert.match(text(h.get("treemap-legend")), /higher is better; heat inverted/); assert.match(text(h.get("treemap-legend")), /≥40/);
  assert.equal(h.renders.at(-1).options.fileBand(h.renders.at(-1).files.find(file => file.declarationCount)), 0);
  h.get("treemap-relative").checked = true; await h.get("treemap-relative").fire("change");
  assert.match(text(h.get("treemap-legend")), /[Rr]elative to visible set.*≤80/);
  assert.equal(h.calls.length, 1);
  h.get("treemap-color").value = "halstead.volume"; await h.get("treemap-color").fire("change");
  assert.equal(h.renders.at(-1).options.fileValue(h.renders.at(-1).files.find(file => file.declarationCount)), 12);
  assert.notEqual(api.identity(file("same.rs",1,0,1,"production","one")), api.identity(file("same.rs",1,0,1,"production","two")));
});

test("full payload echoes and metadata reject atomically; same-pin aggregate reload is explicit", async () => {
  for (const mutate of [p => {p.role="all";}, p => {p.aggregate="sum";}, p => {p.scope="file";}, p => {p.metricsVersion="other";}, p => {delete p.files[0].physicalLines;}, p => {delete p.files[0].fileRole;}, p => {p.files.push(p.files[0]);}]) {
    const h=harness(); h.setResponse(async () => {const payload=fixture();mutate(payload);return {payload};});await h.controller.refresh();
    assert.equal(h.renders.length,0);assert.equal(h.get("treemap-files").children.length,0);assert.equal(h.get("treemap-state").dataset.state,"error");
  }
  const h=harness();await h.controller.refresh();h.setResponse(async () => ({payload:fixture("mean")}));h.get("treemap-aggregate").value="mean";await h.get("treemap-aggregate").fire("change");
  assert.match(h.calls.at(-1).url,/aggregate=mean/);assert.match(h.calls.at(-1).url,/indexRevision=1/);
  h.setResponse(async () => ({payload:{...fixture("mean"),indexGeneration:"other"}}));await h.controller.refresh();assert.equal(h.conflicts.length,1);
});

test("role, aggregate, refresh and local filters invalidate selected source and old render callbacks", async () => {
  for (const change of ["treemap-metric-tests","treemap-aggregate","treemap-hide-tests","treemap-color","treemap-language","treemap-path","treemap-refresh"]) {
    const h=harness();await h.controller.refresh();const old=h.renders[0];old.options.onSelectFile(old.files.find(file => file.declarationCount));
    const button=h.button("Read indexed file source");await button.fire("click");assert.equal(h.reads[0].current(),true);
    const control=h.get(change);control.checked=!control.checked;control.value=({"treemap-aggregate":"sum","treemap-color":"role","treemap-language":"python","treemap-path":"src"})[change] || "";
    h.setResponse(async () => ({payload:fixture(change==="treemap-aggregate"?"sum":"max",change==="treemap-metric-tests"?"all":"production")}));
    await control.fire(change==="treemap-path"?"input":change==="treemap-refresh"?"click":"change");
    assert.equal(h.reads[0].current(),false,change);await button.fire("click");assert.equal(h.reads.length,1,change);
    assert.equal(old.options.isCurrent(),false,change);old.options.onSelectFile(old.files[0]);assert.equal(h.get("treemap-inspector").hidden,true,change);
  }
});

test("late full-pin/root/session fetches cannot repaint or report conflicts", async () => {
  for (const changed of [{rootKey:"checkout-two",session:1,pin},{rootKey:"checkout-one",session:2,pin},{rootKey:"checkout-one",session:1,pin:{...pin,indexRevision:2}},{rootKey:"checkout-one",session:1,pin:{...pin,indexGeneration:"other"}}]) {
    for (const reject of [false,true]) {
      const h=harness(), pending=deferred();h.setResponse(() => pending.promise);const loading=h.controller.refresh();h.setContext(changed);
      if (reject) pending.reject(Object.assign(new Error("stale"),{status:409,code:"revision_conflict"}));else pending.resolve({payload:fixture()});await loading;
      assert.equal(h.renders.length,0);assert.equal(h.conflicts.length,0);assert.equal(h.calls[0].current(),false);
    }
  }
});

test("same-pin role/aggregate and refresh request serials fence late success and failure", async () => {
  for (const control of ["treemap-metric-tests","treemap-aggregate","treemap-refresh"]) for (const reject of [false,true]) {
    const h=harness(), pending=deferred();h.setResponse(() => pending.promise);const loading=h.controller.refresh();
    h.setResponse(async () => ({payload:fixture(control==="treemap-aggregate"?"sum":"max",control==="treemap-metric-tests"?"all":"production")}));
    h.get(control).checked=true;h.get(control).value="sum";await h.get(control).fire(control==="treemap-refresh"?"click":"change");const state=h.get("treemap-state").textContent;
    if(reject)pending.reject(Object.assign(new Error("old"),{status:409,code:"revision_conflict"}));else pending.resolve({payload:fixture()});await loading;
    assert.equal(h.renders.length,1);assert.equal(h.calls[0].current(),false);assert.equal(h.conflicts.length,0);assert.equal(h.get("treemap-state").textContent,state);
  }
});

test("late Pele outcome cannot repaint a newer filter; rejected mapping exposes no partial navigation", async () => {
  const h=harness(), pending=deferred(), entered=deferred();h.setRenderer(async (_container,files,options) => {entered.resolve({files,options});return pending.promise;});
  const loading=h.controller.refresh(), old=await entered.promise;assert.equal(old.options.isCurrent(),true);
  h.setRenderer(async () => ({status:"ready"}));h.get("treemap-language").value="python";await h.get("treemap-language").fire("change");const state=h.get("treemap-state").textContent;
  pending.resolve({status:"error",reason:"stale mapping"});await loading;assert.equal(old.options.isCurrent(),false);assert.equal(h.get("treemap-state").textContent,state);
  h.setRenderer(async container => {container.append({children:[],textContent:"partial"});return {status:"error",reason:"Missing native identity"};});await h.controller.refresh();
  assert.equal(h.get("treemap-state").dataset.state,"error");assert.equal(h.get("treemap-diagram").children.length,0);assert.equal(h.get("treemap-files").children.length,0);
});

test("source completion guard includes full generation/revision and checkout/session, even without reset", async () => {
  for (const changed of [{rootKey:"other",session:1,pin},{rootKey:"checkout-one",session:2,pin},{rootKey:"checkout-one",session:1,pin:{...pin,indexRevision:2}},{rootKey:"checkout-one",session:1,pin:{...pin,indexGeneration:"other"}}]) {
    const h=harness();await h.controller.refresh();const rendered=h.renders[0];rendered.options.onSelectFile(rendered.files.find(file => file.declarationCount));const button=h.button("Read indexed file source");
    await button.fire("click");h.setContext(changed);assert.equal(h.reads[0].current(),false);await button.fire("click");assert.equal(h.reads.length,1);
  }
});

test("Architecture entry checks the full pin and exact tuple, opens parent scope, and expressly reveals test files", async () => {
  const h=harness();const wanted={sourceSetId:"workspace",language:"rust",path:"tests/main.rs"};
  assert.equal(await h.controller.openFile(wanted,{...pin,indexGeneration:"different"}),false);assert.equal(h.calls.length,0);
  assert.equal(await h.controller.openFile(wanted,pin),true);assert.equal(h.get("treemap-hide-tests").checked,false);assert.equal(h.get("treemap-inspector-title").textContent,"tests/main.rs");
  assert.equal(h.renders.at(-1).files.length,2);assert.equal(h.renders.at(-1).options.selectedKey,'["workspace","rust","tests/main.rs"]');
  const duplicate={sourceSetId:"dependency",language:"rust",path:"src/main.rs"};assert.equal(await h.controller.openFile(duplicate,pin),true);
  await h.button("Read indexed file source").fire("click");assert.equal(h.reads[0].site.sourceSetId,"dependency");
  assert.equal(await h.controller.openFile({...duplicate,language:"python"},pin),false);assert.match(h.get("treemap-state").textContent,/identity was not returned/);assert.equal(h.get("treemap-inspector").hidden,true);
});

test("zero-only scopes remain selectable without fabricated SVG area or a renderer dependency", async () => {
  const h=harness();h.setResponse(async () => {const payload=fixture();payload.files=[file("empty.rs",0,0,0)];return {payload};});
  h.setRenderer(async () => {throw new Error("zero areas must not need geometry");});await h.controller.refresh();assert.equal(h.get("treemap-state").dataset.state,"ready");
  assert.equal(h.get("treemap-diagram").children.length,0);assert.equal(h.get("treemap-files").children.length,1);await h.get("treemap-files").children[0].fire("click");assert.equal(h.get("treemap-inspector-title").textContent,"empty.rs");
});


test("pending Architecture entry cannot override a newer local filter or a later entry", async () => {
  const wanted={sourceSetId:"workspace",language:"rust",path:"src/main.rs"};
  const h=harness(), pending=deferred();h.setResponse(() => pending.promise);const opening=h.controller.openFile(wanted,pin);
  h.get("treemap-language").value="python";await h.get("treemap-language").fire("change");pending.resolve({payload:fixture()});
  assert.equal(await opening,false);assert.equal(h.get("treemap-language").value,"python");assert.equal(h.get("treemap-inspector").hidden,true);
  const h2=harness(), old=deferred();h2.setResponse(() => old.promise);const first=h2.controller.openFile(wanted,pin);
  h2.setResponse(async () => ({payload:fixture()}));assert.equal(await h2.controller.openFile({...wanted,sourceSetId:"dependency"},pin),true);
  old.resolve({payload:fixture()});assert.equal(await first,false);assert.match(text(h2.get("treemap-inspector-content")),/dependency · rust/);
});


test("fixed heat survives filters; sum is neutral until Relative and never refetches on local controls", async () => {
  const h = harness(); await h.controller.refresh();
  assert.equal(h.get("treemap-relative").checked, false);
  const measured = h.renders.at(-1).files.find(row => row.declarationCount);
  assert.equal(h.renders.at(-1).options.fileBand(measured), 0);
  h.get("treemap-path").value = measured.path; await h.get("treemap-path").fire("input");
  assert.equal(h.renders.at(-1).options.fileBand(measured), 0); assert.equal(h.calls.length, 1);
  h.get("treemap-path").value = ""; await h.get("treemap-path").fire("input");
  h.setResponse(async () => ({payload:fixture("sum")})); h.get("treemap-aggregate").value = "sum"; await h.get("treemap-aggregate").fire("change");
  const rendered = h.renders.at(-1); assert.equal(rendered.options.fileBand(rendered.files.find(row => row.declarationCount)), null);
  assert.match(text(h.get("treemap-legend")), /relative totals/);
  h.get("treemap-relative").checked = true; await h.get("treemap-relative").fire("change");
  assert.equal(h.renders.at(-1).options.fileBand(h.renders.at(-1).files.find(row => row.declarationCount)), 0); assert.equal(h.calls.length, 2);
});

test("excluded file rows retain physical area, exact reasons, raw populations and selection", async () => {
  const h = harness(), dto = fixture(), excluded = file("fixtures/input.rs", 3, 2, 9);
  excluded.declarationCount = 0; excluded.metrics = {cognitive:null}; excluded.excludedReason = "fixture"; dto.files.push(excluded);
  h.setResponse(async () => ({payload:dto})); await h.controller.refresh();
  h.get("treemap-path").value = "fixtures/"; await h.get("treemap-path").fire("input");
  const rendered = h.renders.at(-1); assert.equal(rendered.files[0], excluded); assert.equal(rendered.options.fileBand(excluded), null);
  rendered.options.onSelectFile(excluded);
  assert.match(text(h.get("treemap-inspector-content")), /Excluded from heat: fixture/);
  assert.match(h.get("treemap-state").textContent, /9 physical file lines.*8 callables excluded from heat.*1 fixture files/);
});


test("layer exclusions warning preserves measured paint and stays visible in empty scopes", async () => {
  const h = harness(), dto = fixture();
  const warning = "Rules unavailable; layer-based heat exclusions were not applied.";
  dto.layerExclusionsWarning = warning;
  h.setResponse(async () => ({payload:dto})); await h.controller.refresh();
  assert.equal(h.get("treemap-state").dataset.state, "ready");
  assert.ok(h.get("treemap-state").textContent.endsWith(warning));
  const rendered = h.renders.at(-1), measured = rendered.files.find(row => row.declarationCount > 0);
  assert.equal(rendered.options.fileValue(measured), 3); assert.equal(rendered.options.fileBand(measured), 0);
  assert.ok(h.get("treemap-files").children.length > 0); assert.ok(h.get("treemap-diagram").children.length > 0);
  assert.equal(h.get("treemap-state").children.length, 0, "warning is plain state text");
  h.get("treemap-path").value = "no-matching-files"; await h.get("treemap-path").fire("input");
  assert.equal(h.get("treemap-state").dataset.state, "empty");
  assert.match(h.get("treemap-state").textContent, /No indexed files match/);
  assert.ok(h.get("treemap-state").textContent.endsWith(warning));
  h.setResponse(async () => ({payload:{...dto,files:[]}})); await h.controller.refresh();
  assert.equal(h.get("treemap-state").dataset.state, "empty");
  assert.ok(h.get("treemap-state").textContent.endsWith(warning));
  h.setResponse(async () => ({payload:fixture()})); await h.controller.refresh();
  assert.ok(!h.get("treemap-state").textContent.includes(warning), "a normal payload clears the warning");
});
