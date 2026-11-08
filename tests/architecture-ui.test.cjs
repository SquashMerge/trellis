"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const source = fs.readFileSync(path.join(__dirname, "../web/architecture.js"), "utf8");
const pinSource = fs.readFileSync(path.join(__dirname, "../web/app.js"), "utf8").match(/const IndexPin = Object.freeze\(\{[\s\S]*?\n\}\);\nwindow.TrellisIndexPin = IndexPin;/)[0];
const pin = {indexGeneration:"12345678-1234-4123-8123-123456789abc",indexRevision:1};
const descendants = node => [node, ...node.children.flatMap(descendants)];
const text = node => descendants(node).map(item => item.textContent).join(" ");
const deferred = () => { let resolve, reject; const promise = new Promise((yes, no) => {resolve = yes; reject = no;}); return {promise, resolve, reject}; };
const identity = (path, sourceSetId = "workspace") => ({sourceSetId,language:"rust",path,sid:`sid:${sourceSetId}:${path}`});
function fixture(aggregate = "max", hash = "rules-one", role = "production") {
  const a = identity("src/a.rs"), b = identity("src/b.rs");
  const site = {role:"production",relationKind:"rust_path",spelling:"crate::b",lines:{start:2,end:2},range:{start:10,end:18},ownerSyntaxId:"owner",callId:null};
  const pair = {source:a,target:b,count:7,sites:[site],omittedSites:6,evidenceTier:"syntax",semanticSites:0};
  return {...pin,state:"ready",rulesFile:{path:".trellis/rules.toml",source:"checkout-config",contentHash:hash},
    aggregate,role,workbench:true,metricsVersion:"metrics:v1",evidenceTier:"syntax",semanticComplete:false,
    layers:[{name:"UI",order:1,shared:false},{name:"Storage",order:2,shared:false}],
    modules:[{identity:a,layer:"UI",physicalLines:4,declarationCount:2,roleCounts:{production:2,test:1},metrics:{cognitive:3,cyclomatic:4,maintainabilityIndex:80,halstead:{volume:12},fanIn:{value:null,unavailable:"not-measured"}},revisionId:"revision-a",contentHash:"hash-a",provenanceId:"native"},
      {identity:b,layer:"Storage",physicalLines:8,declarationCount:0,roleCounts:{production:0,test:2},metrics:{cognitive:null,cyclomatic:null},revisionId:"revision-b",contentHash:"hash-b",provenanceId:"native"}],
    edges:[{sourceLayer:"UI",destinationLayer:"Storage",role:"production",count:7,status:"conforms",excepted:false,exceptionReason:null,pairs:[pair]},
      {sourceLayer:"Storage",destinationLayer:"UI",role:"test",count:2,status:"deviates",excepted:false,exceptionReason:null,pairs:[]}],
    references:{production:[pair],tests:[],unresolved:[],counts:{mappedSites:9,unresolvedSites:0},coverage:[{layer:"UI",role:"production",sites:7,mappedSites:7,unresolvedSites:0},{layer:"Storage",role:"test",sites:2,mappedSites:2,unresolvedSites:0}],limitations:["Structural mapping is not binding."]},rules:{instances:[],testOnly:[],counts:{comparable:0,conforms:0,deviates:0,unknown:0,excepted:0},unassignedCount:0},exceptionCount:0};
}
// The daemon's paged detail route (#448), answered from the last full fixture. The
// controller receives only the summary (the harness strips the detail arrays).
const flush = () => new Promise(resolve => setImmediate(resolve));
function emulateDetails(full, url) {
  const query = new URL(url, "http://local").searchParams, kind = query.get("kind");
  const layerOf = id => full.modules.find(module => JSON.stringify(module.identity) === JSON.stringify(id))?.layer ??
    full.references.externalPackages?.find(item => JSON.stringify(item.identity) === JSON.stringify(id))?.layer ?? null;
  const is = (value, key) => !query.has(key) || value === query.get(key);
  let items = [];
  if (kind === "instances") {
    items = [["production", full.rules.instances], ["test", full.rules.testOnly]].filter(([role]) => is(role, "role"))
      .flatMap(([role, rows]) => rows.map(instance => ({role, instance}))).filter(({instance:r}) =>
        is(r.rule, "rule") && is(r.status, "status") && is(String(!!r.excepted), "excepted") && is(r.exceptionReason, "exceptionReason") &&
        is(r.sourceLayer, "sourceLayer") && is(r.destinationLayer, "destinationLayer") &&
        (!query.has("layer") || [r.sourceLayer, r.destinationLayer].includes(query.get("layer"))) &&
        (!query.has("path") || [r.target?.path, r.destination?.path].includes(query.get("path"))));
  } else if (kind === "pairs") {
    items = [["production", full.references.production], ["test", full.references.tests]].filter(([role]) => is(role, "role"))
      .flatMap(([role, rows]) => rows.map(pair => ({role, sourceLayer:layerOf(pair.source), targetLayer:layerOf(pair.target), pair}))).filter(item =>
        (!query.has("path") || [item.pair.source.path, item.pair.target.path].includes(query.get("path"))) &&
        is(JSON.stringify(item.pair.source), "source") && is(JSON.stringify(item.pair.target), "target") &&
        (!query.has("layer") || [item.sourceLayer, item.targetLayer].includes(query.get("layer"))) &&
        (!query.has("unlayered") || item.sourceLayer === null || item.targetLayer === null || item.sourceLayer === item.targetLayer));
  } else items = full.references.unresolved.filter(item => is(item.source.path, "path"));
  const offset = Number(query.get("offset")), limit = Number(query.get("limit") || 100);
  return {payload:{indexGeneration:query.get("indexGeneration"), indexRevision:Number(query.get("indexRevision")),
    rulesFile:{contentHash:query.get("rulesHash")}, kind, offset, total:items.length,
    nextOffset:offset + limit < items.length ? offset + limit : null, items:items.slice(offset, offset + limit)}};
}
function harness() {
  const elements = new Map(), calls = [], detailCalls = [], renders = [], reads = [], conflicts = [], timers = [];
  let context = {rootKey:"checkout-one",session:1,pin}, response = async () => ({payload:fixture(),catchingUp:false}), full = null, detailError = null;
  // Created nodes with an id (the inspector's review chips, #449) resolve by that id; the newest wins.
  const created = [];
  const document = {querySelector:() => null,getElementById:id => {
    const made = created.findLast(item => item.id === id);
    if (made) return made;
    if (!elements.has(id)) elements.set(id, node(id)); return elements.get(id);
  },createElement:tag => { const item = node(tag); created.push(item); return item; }};
  function node(tagName) {
    return {tagName,textContent:"",children:[],dataset:{},attrs:{},listeners:{},hidden:false,value:"",checked:false,
      append(...items) {this.children.push(...items);},replaceChildren(...items) {this.children = [...items];this.textContent="";},
      setAttribute(key,value) {this.attrs[key]=String(value);},focus() {},
      addEventListener(name,fn) {(this.listeners[name] ||= []).push(fn);},
      async fire(name) {for (const fn of this.listeners[name] || []) await fn({target:this});}};
  }
  const get = document.getElementById;
  let renderer = async (_container, view, options) => {renders.push({view,options}); return {status:"ready"};};
  const window = {TrellisArchitecturePele:{render:(...args) => renderer(...args)},TrellisShell:{
    clearArchitectureInspector:() => {get("architecture-inspector").hidden=true;get("architecture-inspector-content").replaceChildren();},
    showArchitectureInspector:() => {get("architecture-inspector").hidden=false;},
  }};
  vm.runInNewContext(pinSource + "\n" + fs.readFileSync(path.join(__dirname, "../web/metric-heat.js"), "utf8") + "\n" + fs.readFileSync(path.join(__dirname, "../web/architecture-deviations.js"), "utf8") + "\n" + source, {window,document,URLSearchParams,console,setTimeout:(run,delay) => {timers.push({run,delay}); return timers.length;},clearTimeout:id => {if (id) timers[id-1].run = null;}});
  const controller = window.TrellisArchitecture.create({request:async (url,current,options) => {
    if (url.startsWith("/api/architecture/details?")) { detailCalls.push({url,current}); if (detailError) throw detailError; return emulateDetails(full, url); }
    calls.push({url,current,options}); const data = await response(url,options);
    if (!url.startsWith("/api/architecture?") || !data?.payload?.references) return data;
    // The daemon sends the summary; only the detail emulation sees the full arrays.
    full = data.payload;
    const {production, tests, unresolved, modules, ...references} = full.references;
    const {instances, testOnly, unassigned, externalPackages, ...rules} = full.rules;
    return {...data, payload:{...full, references, rules}};
  },context:() => context,
    readSource:(site,revision,current) => reads.push({site,revision,current}),onConflict:() => conflicts.push(true)});
  const button = label => {const found = descendants(get("architecture-inspector-content")).find(item => item.tagName === "button" && item.textContent === label);assert.ok(found, `Missing ${label}`);return found;};
  return {timers,window,controller,calls,detailCalls,renders,reads,conflicts,get,button,setContext:value => {context=value;},setResponse:fn => {response=fn;},setRenderer:fn => {renderer=fn;},setDetailError:error => {detailError=error;}};
}

test("init is inert; one owned pin/digest supplies metric values and explicit sampled source actions", async () => {
  const h = harness();assert.equal(h.calls.length,0);await h.controller.refresh();
  assert.equal(h.calls.length,1);const query = new URL(h.calls[0].url,"http://local").searchParams;
  assert.equal(query.get("workbench"),"true");assert.equal(query.get("role"),"production");assert.equal(h.get("architecture-metric-tests").checked,false);
  assert.equal(query.get("aggregate"),"max");assert.equal(query.get("indexGeneration"),pin.indexGeneration);assert.equal(query.get("indexRevision"),"1");
  const {view,options} = h.renders[0];assert.equal(options.moduleValue(view.modules[0]),3);assert.equal(options.moduleValue(view.modules[1]),null);
  assert.equal(options.layerLabel(view.layers[0]),"1 · UI · 1 file · max 3");
  assert.deepEqual([...options.layerStats(view.layers[0])].map(stat => [stat.text, stat.tone]),[["↓7","neutral"]],"only non-zero counts become chips");
  assert.equal(h.reads.length,0);assert.equal(h.detailCalls.length,0,"the summary load reads no details");options.onSelectModule(view.modules[0]);await flush();
  assert.match(text(h.get("architecture-inspector-content")),/4 physical file lines · 2 production callables/);
  assert.match(text(h.get("architecture-inspector-content")),/not-measured/);
  assert.match(text(h.get("architecture-inspector-content")),/6 omitted sites/);
  await h.button("Read indexed file source").fire("click");assert.equal(h.reads.length,1);assert.equal(h.reads[0].site.path,"src/a.rs");
  assert.equal(h.reads[0].revision.indexGeneration,pin.indexGeneration);
  h.controller.clearInspection();assert.equal(h.reads[0].current(),false);
});

test("test toggle and metric recolor only owned data; aggregate reloads the same pin", async () => {
  const h=harness();await h.controller.refresh();const initial=h.get("architecture-summary").textContent;
  h.get("architecture-tests").checked=true;await h.get("architecture-tests").fire("change");
  assert.equal(h.calls.length,1);assert.equal(h.renders.at(-1).options.showTests,true);assert.match(h.get("architecture-summary").textContent,/7 production \/ 2 test-only/);
  assert.match(initial,/7 production \/ 2 test-only/);
  h.get("architecture-color").value="layer";await h.get("architecture-color").fire("change");
  assert.equal(h.renders.at(-1).options.moduleBand(h.renders.at(-1).view.modules[0]),null);
  assert.equal(h.renders.at(-1).options.moduleValue(h.renders.at(-1).view.modules[0]),undefined,"Layer mode omits metric labels rather than claiming unavailable");assert.equal(h.calls.length,1);
  h.setResponse(async () => ({payload:fixture("mean")}));h.get("architecture-aggregate").value="mean";await h.get("architecture-aggregate").fire("change");
  assert.equal(h.calls.length,2);assert.match(h.calls[1].url,/aggregate=mean/);assert.match(h.calls[1].url,/indexRevision=1/);
});

test("late session/pin/operation replies and old rules-digest callbacks cannot select or read source", async () => {
  const h=harness();await h.controller.refresh();const first=h.renders[0];first.options.onSelectModule(first.view.modules[0]);
  const oldSource=h.button("Read indexed file source"), pending=deferred();h.setResponse(() => pending.promise);
  const refresh=h.controller.refresh();await oldSource.fire("click");assert.equal(h.reads.length,0);
  h.setContext({rootKey:"checkout-two",session:2,pin});pending.resolve({payload:fixture("max","rules-two")});await refresh;
  assert.equal(h.renders.length,1);assert.equal(first.options.isCurrent(),false);
  first.options.onSelectModule(first.view.modules[0]);assert.equal(h.get("architecture-inspector").hidden,true);
});

test("refresh replaces rules at unchanged code pin and old source callbacks stay inert", async () => {
  const h=harness();await h.controller.refresh();const first=h.renders[0];first.options.onSelectModule(first.view.modules[0]);const old=h.button("Read indexed file source");
  h.setResponse(async () => ({payload:fixture("max","rules-two")}));await h.controller.refresh();await old.fire("click");
  assert.equal(h.reads.length,0);assert.equal(h.renders[1].view.rulesFile.contentHash,"rules-two");assert.equal(first.options.isCurrent(),false);
});

test("missing layers and located invalid rules refuse partial interactive diagrams", async () => {
  const h=harness();h.setResponse(async () => ({payload:{...fixture(),state:"missing_rules",rulesFile:null,layers:[],modules:[],edges:[],references:null,rules:null}}));await h.controller.refresh();
  assert.equal(h.renders.length,0);assert.match(h.get("architecture-state").textContent,/\.trellis\/rules.toml/);assert.match(h.get("architecture-state").textContent,/trellis_propose_layers/);
  h.setResponse(async () => {const error=new Error("invalid");error.diagnostic={path:".trellis/rules.toml",line:8,column:2,kind:"invalid_order",message:"Order must be an integer"};throw error;});await h.controller.refresh();
  assert.match(h.get("architecture-state").textContent,/rules.toml:8:2/);assert.equal(h.get("architecture-diagram").children.length,0);
});

test("renderer rejection is atomic and the controller does not substitute a hand-built graph", async () => {
  const h=harness();h.setRenderer(async container => {container.append({children:[],textContent:"unsafe partial"});return {status:"error",reason:"Missing mapped edge"};});await h.controller.refresh();
  assert.equal(h.get("architecture-diagram").children.length,0);assert.equal(h.get("architecture-state").dataset.state,"error");assert.match(h.get("architecture-state").textContent,/Missing mapped edge/);
});

test("quartiles ignore unavailable values, keep ties together and reverse only MI heat", () => {
  const h=harness(), api=h.window.TrellisArchitecture;
  const cuts=api.quartiles([1,1,1,9,null,NaN,Infinity]);assert.deepEqual(Array.from(cuts),[1,1,9]);
  assert.equal(api.band(1,cuts,"cognitive"),0);assert.equal(api.band(1,cuts,"maintainabilityIndex"),3);assert.equal(api.band(null,cuts,"cognitive"),null);
  const a=identity("same.rs","one"),b=identity("same.rs","two");assert.notEqual(api.moduleKey(a),api.moduleKey(b));
});

test("metric role is independent of test-reference overlay and fences old source actions", async () => {
  const h = harness(); await h.controller.refresh(); const first = h.renders[0];
  first.options.onSelectModule(first.view.modules[0]);
  const oldSource = h.button("Read indexed file source"); await oldSource.fire("click");
  assert.equal(h.reads[0].current(), true);
  h.get("architecture-tests").checked = true; await h.get("architecture-tests").fire("change");
  h.setResponse(async () => ({payload:fixture("max", "rules-one", "all")}));
  h.get("architecture-metric-tests").checked = true;
  const changing = h.get("architecture-metric-tests").fire("change");
  assert.equal(h.get("architecture-inspector").hidden, true);
  assert.equal(h.get("architecture-diagram").children.length, 0);
  assert.equal(Boolean(h.reads[0].current()), false); await oldSource.fire("click");
  assert.equal(h.reads.length, 1); await changing;
  assert.match(h.calls.at(-1).url, /role=all/);
  assert.equal(h.renders.at(-1).options.showTests, true);
  assert.equal(first.options.isCurrent(), false);
  assert.match(text(h.get("architecture-legend")), /all indexed callables/);
  assert.match(h.get("architecture-summary").textContent, /7 production \/ 2 test-only cross-layer sites · Metrics: max, all indexed callables\.$/);
  const current = h.renders.at(-1); current.options.onSelectModule(current.view.modules[0]);
  assert.match(text(h.get("architecture-inspector-content")), /metrics:v1 · all indexed callables/);
  h.setResponse(async () => ({payload:fixture()})); h.get("architecture-metric-tests").checked = false;
  await h.get("architecture-metric-tests").fire("change");
  assert.match(h.calls.at(-1).url, /role=production/); assert.equal(h.renders.at(-1).options.showTests, true);
});

test("a building answer shows progress, and refresh settles only on the result", async () => {
  const h = harness(), building = progress => ({payload:{state:"building",...pin,progress,retryAfterMs:250},catchingUp:false});
  const answers = [building({phase:"reading",completed:120,total:604}), building({phase:"measuring",completed:0,total:604})];
  h.setResponse(async () => answers.shift() || {payload:fixture(),catchingUp:false});
  const tick = () => new Promise(setImmediate);
  let settled = false;
  const refreshed = h.controller.refresh().then(() => { settled = true; });
  await tick();
  assert.equal(h.get("architecture-state").textContent, "Building Architecture for this revision: reading indexed files 120 of 604…");
  assert.equal(h.get("architecture-state").dataset.state, "loading");
  assert.equal(h.get("architecture-diagram").attrs["aria-busy"], "true");
  assert.equal(h.renders.length, 0);
  assert.equal(h.timers.at(-1).delay, 250);
  h.timers.at(-1).run(); await tick();
  assert.equal(h.get("architecture-state").textContent, "Building Architecture for this revision: measuring code…");
  assert.equal(settled, false);
  h.timers.at(-1).run();
  await refreshed;
  assert.equal(h.calls.length, 3);
  assert.equal(h.renders.length, 1);
  assert.equal(h.get("architecture-state").textContent, "");
  assert.equal(h.get("architecture-diagram").attrs["aria-busy"], "false");
});

test("leaving Architecture cancels the pending building read and settles its refresh", async () => {
  const h = harness();
  h.setResponse(async () => ({payload:{state:"building",...pin,progress:null,retryAfterMs:250},catchingUp:false}));
  const refreshed = h.controller.refresh();
  await new Promise(setImmediate);
  const pending = h.timers.at(-1);
  h.controller.reset();
  assert.equal(pending.run, null);
  await refreshed;
  assert.equal(h.calls.length, 1);
});

test("same-pin role changes fence both late request outcomes and async Pele completion", async () => {
  for (const reject of [false, true]) {
    const h = harness(), pending = deferred(); h.setResponse(() => pending.promise);
    const old = h.controller.refresh();
    h.setResponse(async () => ({payload:fixture("max", "rules-one", "all")}));
    h.get("architecture-metric-tests").checked = true; await h.get("architecture-metric-tests").fire("change");
    const state = h.get("architecture-summary").textContent;
    if (reject) pending.reject(Object.assign(new Error("old role conflict"), {status:409, code:"revision_conflict"}));
    else pending.resolve({payload:fixture()});
    await old;
    assert.equal(h.renders.length, 1); assert.equal(h.get("architecture-summary").textContent, state);
    assert.equal(h.conflicts.length, 0); assert.equal(h.calls[0].current(), false);
  }
  const h = harness(), renderer = deferred(), entered = deferred();
  h.setRenderer(async (_container, _view, options) => {entered.resolve(options); return renderer.promise;});
  const oldRender = h.controller.refresh();
  const oldOptions = await entered.promise;
  assert.equal(oldOptions.isCurrent(), true);
  h.setRenderer(async () => ({status:"ready"}));
  h.setResponse(async () => ({payload:fixture("max", "rules-one", "all")}));
  h.get("architecture-metric-tests").checked = true; await h.get("architecture-metric-tests").fire("change");
  const state = h.get("architecture-summary").textContent;
  assert.equal(oldOptions.isCurrent(), false);
  renderer.resolve({status:"error", reason:"late production render"}); await oldRender;
  assert.equal(h.get("architecture-summary").textContent, state);
  assert.match(state, /all indexed callables/);
});

test("Architecture rejects wrong role echo and labels unavailable production populations", async () => {
  const h = harness(); h.setResponse(async () => ({payload:fixture("max", "rules-one", "all")}));
  await h.controller.refresh(); assert.equal(h.renders.length, 0);
  assert.match(h.get("architecture-state").textContent, /Invalid Architecture response/);
  h.setResponse(async () => ({payload:fixture()})); await h.controller.refresh();
  const {view,options} = h.renders.at(-1); options.onSelectModule(view.modules[1]);
  assert.equal(options.moduleValue(view.modules[1]), null);
  assert.match(text(h.get("architecture-inspector-content")), /No production callables.*unavailable, not zero/);
  assert.match(text(h.get("architecture-legend")), /production callables/);
});


test("external package evidence and deny results display without fabricated native metrics or source", async () => {
  const h = harness(), payload = fixture();
  const external = {kind:"externalPackage",sourceSetId:"workspace",language:"rust",package:"legacy-db",standardLibrary:false};
  const descriptor = {identity:external,ecosystem:"cargo",layer:"External",evidence:[{
    kind:"manifest",sourceSetId:"workspace",nativeRevisionId:"native-revision",dependencyHash:"dependencies-hash",
    path:"Cargo.toml",contentHash:"manifest-hash",importRoot:"legacy_db",section:"dependencies",key:"legacy-db",versionConstraint:"1",
  }]};
  const site = {role:"production",relationKind:"import",spelling:"legacy_db::*",lines:{start:2,end:2},range:{start:10,end:22},ownerSyntaxId:"owner",callId:null};
  const pair = {source:payload.modules[0].identity,target:external,count:1,sites:[site],omittedSites:0,evidenceTier:"syntax",semanticSites:0};
  payload.layers.push({name:"External",order:3,shared:false});
  payload.references.externalPackages = [descriptor];
  payload.references.production.push(pair);
  payload.references.counts.mappedSites++;
  payload.rules.instances.push(
    {rule:"$layers",target:pair.source,destination:external,sourceLayer:"UI",destinationLayer:"External",status:"conforms",excepted:false,referenceCount:1},
    {rule:"$external",target:pair.source,destination:external,status:"deviates",excepted:false,referenceCount:1},
  );
  payload.edges.push({sourceLayer:"UI",destinationLayer:"External",role:"production",count:1,status:"conforms",excepted:false,exceptionReason:null,pairs:[pair]});
  h.setResponse(async () => ({payload})); await h.controller.refresh();
  assert.equal(h.renders.length, 1);
  const {view,options} = h.renders[0];
  assert.equal(view.modules.length, 2, "package remains a descriptor, never a native measured module");
  assert.equal(view.modules.some(module => module.identity.kind === "externalPackage"), false);
  assert.equal(options.moduleValue(descriptor), undefined, "no metric value is invented for a package");
  assert.match(text(h.get("architecture-notice-content")), /External packages and captured evidence.*Cargo\.toml.*manifest-hash/s);
  const notice = label => descendants(h.get("architecture-notice-content")).find(item => item.tagName === "button" && item.textContent === label);
  await notice("Inspect external policy results").fire("click"); await flush();
  assert.match(text(h.get("architecture-inspector-content")), /\$external.*legacy-db.*deviates/s);
  assert.equal(new URL(h.detailCalls.at(-1).url, "http://local").searchParams.get("rule"), "$external");
  const layer = view.layers.find(layer => layer.name === "External");
  assert.match(options.layerLabel(layer), /0 files · 1 external packages/);
  options.onSelectLayer(layer); await flush();
  const inspector = text(h.get("architecture-inspector-content"));
  assert.match(inspector, /External packages \(no native source or metrics\)/);
  assert.match(inspector, /src\/a\.rs → legacy-db \(rust package\)/);
  assert.match(inspector, /legacy-db.*manifest-hash/s);
  assert.equal(descendants(h.get("architecture-inspector-content")).some(item => item.tagName === "button" && item.textContent === "Read indexed file source"), false);
  await h.button("Read indexed reference source").fire("click");
  assert.equal(h.reads.length, 1);
  assert.equal(h.reads[0].site.path, "src/a.rs", "only the genuine importing native file can be read");
});

test("unassigned file review counts native files, not the broader identity catalog", async () => {
  const h = harness(), payload = fixture();
  payload.modules[0].layer = null;
  payload.modules.push({identity:identity("src/isolated.rs"),layer:null,physicalLines:1,declarationCount:0,metrics:{}});
  const external = {kind:"externalPackage",sourceSetId:"workspace",language:"rust",package:"legacy-db",standardLibrary:false};
  payload.references.externalPackages = [{identity:external,ecosystem:"cargo",layer:null,evidence:[]}];
  payload.references.production.push({...payload.references.production[0],target:external,count:1,omittedSites:0});
  payload.rules.unassigned = [payload.modules[0].identity, payload.modules[2].identity, external];
  payload.rules.unassignedCount = payload.rules.unassigned.length;
  h.setResponse(async () => ({payload})); await h.controller.refresh();
  const assertFileReview = async () => {
    assert.equal(h.get("architecture-unassigned").textContent, "2 unassigned files");
    await h.get("architecture-unassigned").fire("click");
    const rows = descendants(h.get("architecture-review-list")).filter(item => item.className === "architecture-rule-row");
    assert.deepEqual(rows.map(row => row.dataset.source), ["src/a.rs", "src/isolated.rs"], "includes an isolated native file, not the assigned file or external identity");
    assert.equal(payload.rules.unassignedCount, 3, "the broader catalog is unchanged");
    assert.match(text(h.get("architecture-notice-content")), /3 unassigned identities \(native files and external packages\)/);
  };
  await assertFileReview();
  const initial = h.renders[0].view;
  assert.equal(initial.indexGeneration, pin.indexGeneration); assert.equal(initial.indexRevision, pin.indexRevision);
  assert.equal(initial.rulesFile.contentHash, "rules-one");
  payload.role = "all"; h.get("architecture-metric-tests").checked = true;
  await h.get("architecture-metric-tests").fire("change");
  await assertFileReview();
  for (const call of h.calls) {
    const query = new URL(call.url, "http://local").searchParams;
    assert.equal(query.get("indexGeneration"), pin.indexGeneration); assert.equal(query.get("indexRevision"), String(pin.indexRevision));
    assert.equal(query.has("paths"), false); assert.equal(query.has("path"), false, "Architecture requests checkout-wide file inventory");
  }
  assert.equal(h.renders.at(-1).view.rulesFile.contentHash, initial.rulesFile.contentHash);
});

function authoringFixture(hash = "rules-one") {
  const payload = fixture("max",hash), a = payload.modules[0].identity, b = payload.modules[1].identity;
  const pair = payload.references.production[0];
  payload.rules.instances = [
    {rule:"$layers",target:a,destination:b,sourceLayer:"UI",destinationLayer:"Storage",status:"deviates",excepted:false,referenceCount:7},
    {rule:"$layers",target:b,destination:a,sourceLayer:"Storage",destinationLayer:"UI",status:"deviates",excepted:false,referenceCount:2},
    {rule:"$layers",target:a,destination:null,sourceLayer:"UI",destinationLayer:null,status:"unknown",excepted:false,referenceCount:1},
  ];
  payload.rules.testOnly = [{...payload.rules.instances[0],referenceCount:1}];
  payload.modules.push({identity:identity("src/no-edges.rs"),layer:null,physicalLines:1,declarationCount:0,metrics:{}});
  payload.edges[0] = {...payload.edges[0],status:"deviates"};
  return payload;
}
const ruleRows = h => descendants(h.get("architecture-review-list")).filter(item => item.className === "architecture-rule-row").length;
const byLabel = (node,label) => descendants(node).find(item => item.attrs["aria-label"] === label);
async function selectTwo(h) {
  await h.get("architecture-review").fire("click");await h.get("architecture-deviations-review").fire("click");await flush();
  for (const label of ["Select src/a.rs → src/b.rs","Select src/b.rs → src/a.rs"]) {
    const input = byLabel(h.get("architecture-review-list"),label);assert.ok(input);input.checked=true;await input.fire("change");
  }
  await h.get("architecture-add-selected").fire("click");
}
function editorResponse(hash = "rules-one") {
  return {payload:{...pin,baseHash:hash,proposedHash:"rules-proposed",text:"# preserved\n[[exceptions]]\nreason = '<safe>'\n",diff:"--- rules\n+++ rules\n+reason = '<safe>'",pairs:[{},{}]}};
}

test("review filters independent role/state, edge instances and no-edge unassigned assignments", async () => {
  const h=harness(),payload=authoringFixture();h.setResponse(async () => ({payload}));await h.controller.refresh();
  await h.get("architecture-review").fire("click");await h.get("architecture-deviations-review").fire("click");await flush();
  assert.equal(ruleRows(h),2,"the deviations chip lists the production deviations it counts");
  h.get("architecture-review-role").value="all";await h.get("architecture-review-role").fire("change");await flush();
  assert.equal(ruleRows(h),3,"two production deviations plus test-only row");
  const testCheckbox=descendants(h.get("architecture-review-list")).filter(item=>item.tagName==="input").at(-1);assert.equal(testCheckbox.disabled,true);
  h.get("architecture-review-role").value="test";await h.get("architecture-review-role").fire("change");await flush();assert.equal(ruleRows(h),1);
  assert.match(text(h.get("architecture-review-list")),/test-only rows cannot be waived/);assert.equal(h.calls.length,1);
  h.get("architecture-review-role").value="all";h.get("architecture-review-state").value="unknown";await h.get("architecture-review-state").fire("change");await flush();assert.match(text(h.get("architecture-review-list")),/Unknown destination/);
  h.renders[0].options.onSelectEdge(payload.edges[0]);await flush();assert.match(text(h.get("architecture-inspector-content")),/\$layers/);assert.equal(ruleRows(h),1);
  await h.get("architecture-unassigned").fire("click");assert.match(text(h.get("architecture-review-list")),/src\/no-edges.rs/);
  const assign=descendants(h.get("architecture-review-list")).find(item=>item.textContent==="Assign file");await assign.fire("click");
  assert.equal(h.get("architecture-reason").hidden,true);assert.match(h.get("architecture-edit-scope").textContent,/assignFile.*no-edges/s);
});

test("multi-pair one-reason edit uses one preview and exact same operations/hashes for apply", async () => {
  const h=harness();let hash="rules-one";
  h.setResponse(async (url,options) => {
    if(url.endsWith("/preview")) return editorResponse();
    if(url.endsWith("/apply")) {hash="rules-proposed";return {payload:{...pin,rulesFile:{contentHash:hash},mutationOutcome:"committed"}};}
    return {payload:authoringFixture(hash)};
  });
  await h.controller.refresh();await selectTwo(h);
  await h.get("architecture-preview").fire("click");assert.equal(h.calls.length,1);assert.match(h.get("architecture-edit-state").textContent,/nonblank/);
  h.get("architecture-reason").value="Reviewed group <script>";await h.get("architecture-reason").fire("input");await h.get("architecture-preview").fire("click");
  const sent=h.calls.at(-1);assert.equal(sent.options.method,"POST");assert.equal(sent.options.body.operations.length,1);assert.equal(sent.options.body.operations[0].pairs.length,2);
  assert.equal(sent.options.body.operations[0].reason,"Reviewed group <script>");assert.equal(h.get("architecture-proposed-text").textContent,editorResponse().payload.text);
  assert.match(h.get("architecture-diff").textContent,/<safe>/);assert.equal(h.get("architecture-diff").children.length,0);
  await h.get("architecture-confirm").fire("click");
  const writes=h.calls.filter(c=>c.options?.method==="POST");assert.equal(writes.length,2);
  assert.equal(JSON.stringify(writes[1].options.body),JSON.stringify({...sent.options.body,proposedHash:"rules-proposed"}));assert.match(h.get("architecture-state").textContent,/Rules committed.*same code/);
});

test("a refresh that fails after an apply drops the reopen request, so a later refresh keeps the tree closed", async () => {
  const h=harness();let hash="rules-one",failRead=false;
  h.setResponse(async url => {
    if(url.endsWith("/preview")) return editorResponse();
    if(url.endsWith("/apply")) {hash="rules-proposed";failRead=true;return {payload:{...pin,rulesFile:{contentHash:hash},mutationOutcome:"committed"}};}
    if(failRead) throw Object.assign(new Error("Architecture is still building"),{status:503});
    return {payload:authoringFixture(hash)};
  });
  await h.controller.refresh();await selectTwo(h);
  h.get("architecture-reason").value="Reviewed";await h.get("architecture-reason").fire("input");await h.get("architecture-preview").fire("click");
  await h.get("architecture-review").fire("click");await flush();
  assert.equal(h.get("architecture-deviations").hidden,false,"the tree is open when the rules are applied");
  await h.get("architecture-confirm").fire("click");await flush();
  assert.equal(h.get("architecture-deviations").hidden,true);
  failRead=false;await h.controller.refresh();await flush();
  assert.equal(h.get("architecture-deviations").hidden,true,"a later successful refresh does not reopen the tree");
});

test("conflict preserves draft and reload requires a new preview on the new rules base", async () => {
  const h=harness();let hash="rules-one";
  h.setResponse(async (url,options) => {
    if(url.endsWith("/preview")) return editorResponse(options.body.baseHash);
    if(url.endsWith("/apply")) throw Object.assign(new Error("changed"),{status:409,code:"rules_conflict"});
    if(url.includes("/rules?")) return {payload:{...pin,rulesFile:{contentHash:hash},exceptions:[],layers:[]}};
    return {payload:authoringFixture(hash)};
  });
  await h.controller.refresh();await selectTwo(h);h.get("architecture-reason").value="Keep this reason";
  await h.get("architecture-preview").fire("click");await h.get("architecture-confirm").fire("click");
  assert.equal(h.get("architecture-reason").value,"Keep this reason");assert.match(h.get("architecture-edit-state").textContent,/Draft kept/);assert.equal(h.get("architecture-preview-result").hidden,true);
  const before=h.calls.length;await h.get("architecture-confirm").fire("click");await h.get("architecture-preview").fire("click");assert.equal(h.calls.length,before,"old preview cannot retry before reload");
  hash="rules-new";await h.get("architecture-rules-reload").fire("click");assert.equal(h.get("architecture-reason").value,"Keep this reason");
  await h.get("architecture-preview").fire("click");assert.equal(h.calls.at(-1).options.body.baseHash,"rules-new");assert.equal(h.calls.at(-1).options.body.operations[0].pairs.length,2);
});

test("authored orphan edit/remove use hash-qualified entry index; reason input invalidates preview", async () => {
  const h=harness();h.setResponse(async (url) => {
    if(url.endsWith("/preview")) return editorResponse();
    if(url.includes("/rules?")) return {payload:{...pin,rulesFile:{contentHash:"rules-one"},layers:[],exceptions:[{index:4,rule:"named",target:{path:"orphan.rs"},reason:"Original reason"}]}};
    return {payload:authoringFixture()};
  });
  await h.controller.refresh();await h.get("architecture-authored").fire("click");
  assert.match(text(h.get("architecture-exceptions")),/orphan.rs/);
  const entry=h.get("architecture-exceptions").children[0];await descendants(entry).find(item=>item.textContent==="Edit reason").fire("click");
  assert.equal(h.get("architecture-reason").value,"Original reason");await h.get("architecture-preview").fire("click");assert.equal(h.calls.at(-1).options.body.operations[0].index,4);
  h.get("architecture-reason").value="Revised reason";await h.get("architecture-reason").fire("input");assert.equal(h.get("architecture-preview-result").hidden,true);
  const count=h.calls.length;await h.get("architecture-confirm").fire("click");assert.equal(h.calls.length,count);
  await descendants(entry).find(item=>item.textContent==="Remove").fire("click");await h.get("architecture-preview").fire("click");assert.equal(h.calls.at(-1).options.body.operations[0].type,"removeException");
});

test("literal file-to-layer/glob selection emits only finite witnessed native production pairs", async () => {
  const h=harness();h.setResponse(async (url) => url.endsWith("/preview") ? editorResponse() : {payload:authoringFixture()});
  await h.controller.refresh();await h.get("architecture-review").fire("click");await h.get("architecture-deviations-review").fire("click");h.get("architecture-source-glob").value="src/**/a.*";h.get("architecture-destination-layer").value="Storage";
  await h.get("architecture-select-scope").fire("click");await h.get("architecture-add-selected").fire("click");h.get("architecture-reason").value="Selected finite pairs";await h.get("architecture-preview").fire("click");
  assert.equal(h.calls.at(-1).options.body.operations[0].pairs.length,1);assert.equal(h.calls.at(-1).options.body.operations[0].pairs[0].source.path,"src/a.rs");
});

test("late preview replies and old draft controls cannot cross checkout/session/pin resets", async () => {
  const h=harness(), pending=deferred();h.setResponse(async (url) => url.endsWith("/preview") ? pending.promise : {payload:authoringFixture()});
  await h.controller.refresh();await selectTwo(h);h.get("architecture-reason").value="Draft";const request=h.get("architecture-preview").fire("click");
  h.setContext({rootKey:"another-checkout",session:2,pin});h.controller.reset();pending.resolve(editorResponse());await request;
  assert.equal(h.get("architecture-preview-result").hidden,true);const before=h.calls.length;await h.get("architecture-confirm").fire("click");assert.equal(h.calls.length,before);
});


test("late apply cannot refresh a successor checkout and failed post-write recheck is explicitly stale", async () => {
  const h=harness(), pending=deferred();
  h.setResponse(async url=>url.endsWith("/preview") ? editorResponse() : url.endsWith("/apply") ? pending.promise : {payload:authoringFixture()});
  await h.controller.refresh();await selectTwo(h);h.get("architecture-reason").value="Draft";await h.get("architecture-preview").fire("click");
  const save=h.get("architecture-confirm").fire("click"), count=h.calls.length;
  h.setContext({rootKey:"successor",session:2,pin});h.controller.reset("Successor checkout");
  pending.resolve({payload:{...pin,rulesFile:{contentHash:"rules-proposed"},mutationOutcome:"committed"}});await save;
  assert.equal(h.calls.length,count);assert.equal(h.get("architecture-state").textContent,"Successor checkout");
  const next=harness();let written=false;
  next.setResponse(async url=> {
    if(url.endsWith("/preview")) return editorResponse();
    if(url.endsWith("/apply")) {written=true;return {payload:{...pin,rulesFile:{contentHash:"rules-proposed"},mutationOutcome:"durability_uncertain"}};}
    if(written) throw new Error("Recheck I/O error");
    return {payload:authoringFixture()};
  });
  await next.controller.refresh();await selectTwo(next);next.get("architecture-reason").value="Draft";await next.get("architecture-preview").fire("click");await next.get("architecture-confirm").fire("click");
  assert.equal(next.get("architecture-state").dataset.state,"stale");assert.match(next.get("architecture-state").textContent,/Rules were written.*durability is uncertain.*Recheck failed/);
  assert.equal(next.get("architecture-diagram").children.length,0);
});


test("lost or invalid apply response leaves the write outcome unknown until rules reload", async () => {
  for (const failure of [new TypeError("Failed to fetch"),Object.assign(new Error("Request timed out"),{name:"TimeoutError"}),new SyntaxError("Invalid JSON"),null]) {
    const h=harness();let hash="rules-one";
    h.setResponse(async url=> {
      if(url.endsWith("/preview")) return editorResponse(hash);
      if(url.endsWith("/apply")) {hash="rules-proposed";if(failure) throw failure;return {payload:{...pin}};}
      if(url.includes("/rules?")) return {payload:{...pin,rulesFile:{contentHash:hash},exceptions:[],layers:[]}};
      return {payload:authoringFixture(hash)};
    });
    await h.controller.refresh();await selectTwo(h);h.get("architecture-reason").value="Kept after response loss";
    await h.get("architecture-preview").fire("click");await h.get("architecture-confirm").fire("click");
    assert.match(h.get("architecture-edit-state").textContent,/Write outcome unknown.*Reload rules/);
    assert.equal(h.get("architecture-state").dataset.state,"stale");assert.equal(h.get("architecture-diagram").children.length,0);
    assert.equal(h.get("architecture-preview-result").hidden,true);assert.equal(h.get("architecture-reason").value,"Kept after response loss");
    assert.equal(h.conflicts.length,0);
    const count=h.calls.length;await h.get("architecture-confirm").fire("click");await h.get("architecture-preview").fire("click");assert.equal(h.calls.length,count);
    await h.get("architecture-rules-reload").fire("click");await h.get("architecture-preview").fire("click");
    assert.equal(h.calls.at(-1).options.body.baseHash,"rules-proposed");assert.equal(h.get("architecture-reason").value,"Kept after response loss");
    assert.equal(h.calls.filter(call=>call.url.endsWith("/apply")).length,1,"recovery never retries apply");
  }
});

test("typed precommit apply refusals keep their public message without claiming an unknown write", async () => {
  for (const refusal of [{status:400,code:"invalid_request",message:"Requested pair has no production deviation witness"},
    {status:422,code:"invalid_rules",diagnostic:{path:".trellis/rules.toml",line:2,column:1,kind:"invalid_value",message:"Reason is too long"}},
    {status:503,code:"storage_unavailable",message:"Storage is unavailable"}]) {
    const h=harness();
    h.setResponse(async url=> {
      if(url.endsWith("/preview")) return editorResponse();
      if(url.endsWith("/apply")) throw Object.assign(new Error(refusal.message || "Invalid rules"),refusal);
      return {payload:authoringFixture()};
    });
    await h.controller.refresh();await selectTwo(h);h.get("architecture-reason").value="Kept after refusal";
    await h.get("architecture-preview").fire("click");await h.get("architecture-confirm").fire("click");
    assert.ok(h.get("architecture-edit-state").textContent.includes(refusal.diagnostic?.message || refusal.message));
    assert.doesNotMatch(h.get("architecture-edit-state").textContent,/outcome unknown/i);
    const count=h.calls.length;await h.get("architecture-confirm").fire("click");assert.equal(h.calls.length,count);
    await h.get("architecture-preview").fire("click");assert.equal(h.calls.length,count+1,"known refusal permits a fresh preview without reload");
  }
});

test("no_layers keeps a rules-hash-qualified authoring basis for orphan edits and removal without a graph", async () => {
  for (const mode of ["Edit reason","Remove"]) {
    const h=harness();let hash="rules-one";
    h.setResponse(async (url,options) => {
      if(url.endsWith("/preview")) return editorResponse();
      if(url.endsWith("/apply")) {hash="rules-proposed";return {payload:{...pin,rulesFile:{contentHash:hash},mutationOutcome:"committed"}};}
      if(url.includes("/rules?")) return {payload:{...pin,rulesFile:{contentHash:hash},layers:[],exceptions:[{index:2,rule:"orphan-rule",target:{path:"orphan.rs"},reason:"Original reason"}]}};
      return {payload:{...fixture("max",hash),state:"no_layers",layers:[],modules:[],edges:[],references:null,rules:null,exceptionCount:1}};
    });
    await h.controller.refresh();assert.equal(h.renders.length,0);assert.match(h.get("architecture-state").textContent,/Existing authored exceptions/);
    await h.get("architecture-color").fire("change");assert.equal(h.renders.length,0);
    await h.get("architecture-authored").fire("click");assert.match(text(h.get("architecture-exceptions")),/orphan.rs/);
    const entry=h.get("architecture-exceptions").children[0];await descendants(entry).find(item=>item.textContent===mode).fire("click");
    if(mode==="Edit reason") h.get("architecture-reason").value="Revised orphan reason";
    await h.get("architecture-preview").fire("click");const body=h.calls.at(-1).options.body;
    assert.equal(body.baseHash,"rules-one");assert.equal(body.operations[0].index,2);assert.equal(body.operations[0].type,mode==="Edit reason" ? "editException" : "removeException");
    await h.get("architecture-confirm").fire("click");assert.match(h.get("architecture-state").textContent,/Rules committed/);assert.equal(h.renders.length,0);
    const chips=descendants(h.get("architecture-inspector-content")).filter(item=>item.className==="architecture-chip").map(item=>item.textContent);
    assert.deepEqual(chips,["1 exception"],"without layers only the authored-exceptions count is measured");
  }
});


test("Relative uses admitted display modules, collapsed representation is stable, and sum is neutral", async () => {
  const h = harness(), dto = fixture();
  dto.layers.push({name:"Hidden",order:3,shared:false,display:"hidden"});
  dto.modules.push({...dto.modules[0],identity:identity("hidden/hot.rs"),layer:"Hidden",metrics:{cognitive:100},declarationCount:0,excludedReason:"hidden_layer"});
  dto.modules[0].metrics.cognitive = 7;
  h.setResponse(async () => ({payload:dto})); await h.controller.refresh();
  const module = dto.modules[0]; assert.equal(h.get("architecture-relative").checked, false);
  assert.equal(h.renders.at(-1).options.moduleBand(module), 1);
  h.get("architecture-relative").checked = true; await h.get("architecture-relative").fire("change");
  assert.match(text(h.get("architecture-legend")), /Relative to visible set.*≤7.*>7–7.*>7–7.*>7/);
  await h.renders.at(-1).options.onToggleLayer(dto.layers[0]);
  assert.match(text(h.get("architecture-legend")), /≤7.*>7–7/); assert.equal(h.calls.length, 1);
  h.get("architecture-relative").checked = false; await h.get("architecture-relative").fire("change");
  assert.equal(h.renders.at(-1).options.moduleBand(module), 1);
  assert.match(h.get("architecture-exclusions").textContent, /6 callables excluded from heat.*1 authored hidden-layer files/, "heat exclusions move out of the status line");
  h.setResponse(async () => ({payload:fixture("sum")})); h.get("architecture-aggregate").value = "sum"; await h.get("architecture-aggregate").fire("change");
  const rendered = h.renders.at(-1); assert.equal(rendered.options.moduleBand(rendered.view.modules[0]), null);
  assert.match(text(h.get("architecture-legend")), /relative totals/);
  h.get("architecture-relative").checked = true; await h.get("architecture-relative").fire("change");
  assert.equal(h.renders.at(-1).options.moduleBand(h.renders.at(-1).view.modules[0]), 0); assert.equal(h.calls.length, 2);
});

const edge = (sourceLayer, destinationLayer, count, status = "conforms", extra = {}) =>
  ({sourceLayer,destinationLayer,role:"production",count,status,excepted:false,exceptionReason:null,pairs:[],...extra});
test("layer stats chip only non-zero production counts by verdict, with deviations in the error tone", async () => {
  const h = harness(), dto = fixture();
  dto.layers.push({name:"Peer",order:2,shared:false},{name:"Shared",order:null,shared:true});
  dto.edges = [edge("UI","Storage",7), edge("Storage","UI",56,"deviates"), edge("Storage","UI",4,"deviates",{excepted:true,exceptionReason:"Reviewed"}),
    edge("Storage","Peer",3,"deviates"), edge("Storage","Shared",9), edge("Shared","UI",2,"deviates"),
    {...edge("Storage","UI",11,"deviates"),role:"test"}];
  // Storage has unresolved production sites, so its counts are lower bounds (#449).
  dto.references.coverage = [{layer:"UI",role:"production",sites:7,mappedSites:7,unresolvedSites:0},{layer:"Storage",role:"production",sites:80,mappedSites:72,unresolvedSites:9},
    {layer:"Shared",role:"production",sites:2,mappedSites:2,unresolvedSites:0}];
  h.setResponse(async () => ({payload:dto})); await h.controller.refresh();
  const {options} = h.renders[0], stats = name => JSON.parse(JSON.stringify(options.layerStats(dto.layers.find(layer => layer.name === name)).map(stat => [stat.text, stat.tone])));
  assert.deepEqual(stats("Storage"), [["↑≥56","error"],["⇄≥3","error"],["→shared ≥9","neutral"],["excepted ≥4","neutral"]], "test-only sites never count; excepted sites are not deviations");
  assert.deepEqual(stats("UI"), [["↓7","neutral"]]);
  assert.deepEqual(stats("Shared"), [["⇄2","error"]], "shared outbound is a deviation, not upward");
  assert.deepEqual(stats("Peer"), [], "a layer without outgoing references has no chips");
  assert.match(options.layerStats(dto.layers[1])[0].label, /^at least 56 outgoing production reference sites · upward deviation$/);
  // Arrows: deviations highlighted with the source layer's measured coverage; allowed arrows dimmed.
  const up = dto.edges[1], down = dto.edges[0];
  assert.equal(options.edgeTone(up), "deviation"); assert.equal(options.edgeTone(down), "allowed");
  assert.equal(options.edgeTone(dto.edges[2]), "allowed", "an excepted deviation is dimmed with the allowed arrows");
  assert.equal(options.edgeTone(dto.edges[6]), "neutral", "test-only arrows are neither highlighted nor counted");
  assert.deepEqual(JSON.parse(JSON.stringify(options.edgeText(up))), {bound:true, label:"lower bound; 90% of 80 production reference sites from Storage mapped to a module", note:"90% mapped"});
  assert.deepEqual(JSON.parse(JSON.stringify(options.edgeText(down))), {bound:false, label:"100% of 7 production reference sites from UI mapped to a module", note:""});
  assert.equal(options.display.edge(down), true, "allowed arrows stay drawn");
  assert.match(h.get("architecture-coverage").textContent, /^81 of 89 reference sites mapped to a module \(91%\) · ≥ counts are lower bounds$/); assert.match(text(h.get("architecture-notice-content")), /0 of 0 production rule instances unknown/);
});

test("#523: an arrow's own coverage decides its lower bound and names its evidence tier", async () => {
  const h = harness(), dto = fixture();
  const exact = {...edge("UI","Storage",7), evidenceTier:"mixed", semanticSites:3,
    coverage:{sites:7,mappedSites:7,unresolvedSites:0,semanticSites:3,complete:true}};
  const bounded = {...edge("Storage","UI",56,"deviates"), evidenceTier:"semantic", semanticSites:56,
    coverage:{sites:80,mappedSites:72,unresolvedSites:9,semanticSites:60,complete:false}};
  dto.edges = [exact, bounded];
  // A summary with semantic sites is accepted; an unknown tier is not.
  dto.evidenceTier = "mixed";
  // The layer rows would say otherwise; the arrow's own coverage wins.
  dto.references.coverage = [{layer:"UI",role:"production",sites:7,mappedSites:7,unresolvedSites:4},{layer:"Storage",role:"production",sites:80,mappedSites:72,unresolvedSites:9}];
  h.setResponse(async () => ({payload:dto})); await h.controller.refresh();
  const {options} = h.renders[0];
  assert.deepEqual(JSON.parse(JSON.stringify(options.edgeText(exact))), {bound:false, label:"100% of 7 production reference sites from UI mapped to a module; syntax and semantic bindings", note:""});
  assert.deepEqual(JSON.parse(JSON.stringify(options.edgeText(bounded))), {bound:true, label:"lower bound; 90% of 80 production reference sites from Storage mapped to a module; semantic bindings", note:"90% mapped"});
  const invalid = harness(), wrong = fixture(); wrong.evidenceTier = "semantic";
  invalid.setResponse(async () => ({payload:wrong})); await invalid.controller.refresh();
  assert.equal(invalid.renders.length, 0);
  assert.match(invalid.get("architecture-state").textContent, /Invalid Architecture response/);
});

test("one chip per module: a file and its same-named directory form one module that opens to files", async () => {
  const {commonDirectory, groupModules} = harness().window.TrellisArchitecture;
  assert.equal(commonDirectory(["src/store.rs", "src/store/a.rs", "src/model.rs"]), "src/");
  assert.equal(commonDirectory(["web/app.js"]), "web/");
  assert.equal(commonDirectory(["build.rs", "tools/x.py"]), "");
  const modules = paths => paths.map(path => ({identity:identity(path)}));
  assert.deepEqual(JSON.parse(JSON.stringify(groupModules(modules(["src/store.rs", "src/store/a.rs", "src/model.rs", "src/a.b.rs"]), "src/").map(([name, files]) => [name, files.length]))),
    [["a.b", 1], ["model", 1], ["store", 2]]);
  const h = harness(), dto = fixture();
  const base = dto.modules[0];
  dto.modules = ["src/store.rs", "src/store/a.rs", "src/store/deep/b.rs", "src/model.rs"].map((path, i) => ({...base, identity:identity(path), metrics:{cognitive:i}}));
  h.setResponse(async () => ({payload:dto})); await h.controller.refresh();
  const display = () => h.renders.at(-1).options.display;
  assert.equal(display().expanded("UI"), false, "#391: a band without a display hint starts collapsed");
  const top = display().entries("UI", dto.modules);
  const plain = value => JSON.parse(JSON.stringify(value));
  assert.deepEqual(plain(top.map(entry => [entry.type, entry.name ?? entry.shown])), [["module","model"],["directory","store\n2 · 3 files"]]);
  assert.match(top[1].label, /^store\n3 files · max 2\nsrc\/store$/);
  await h.renders.at(-1).options.onToggleDirectory(top[1]);
  const opened = display().entries("UI", dto.modules)[1];
  assert.equal(opened.open, true);
  assert.deepEqual(plain(opened.entries.map(entry => [entry.type, entry.name ?? entry.shown])), [["module","store.rs"],["module","a"],["module","deep"]],
    "the root file comes first, then nested modules one level down");
});

test("mod.rs and __init__.py are their module's root file, never nested mod or __init__ chips", async () => {
  const {groupModules} = harness().window.TrellisArchitecture;
  const modules = paths => paths.map(path => ({identity:identity(path)}));
  const plain = value => JSON.parse(JSON.stringify(value));
  assert.deepEqual(plain(groupModules(modules(["src/daemon/mod.rs", "src/daemon/client.rs", "src/pkg/__init__.py"].filter(path => path.startsWith("src/daemon/"))), "src/daemon/").map(([name]) => name)),
    ["client", "mod.rs"], "a band rooted at the module directory names its root file, not \"mod\"");
  assert.deepEqual(plain(groupModules(modules(["pkg/__init__.py", "pkg/util.py"]), "pkg/").map(([name]) => name)), ["__init__.py", "util"]);
  const h = harness(), dto = fixture(), base = dto.modules[0];
  dto.modules = ["src/daemon/mod.rs", "src/daemon/client.rs", "src/daemon/registry/mod.rs", "src/daemon/registry/table.rs", "src/pkg/__init__.py", "src/pkg/util.py", "src/main.rs"]
    .map(path => ({...base, identity:identity(path)}));
  h.setResponse(async () => ({payload:dto})); await h.controller.refresh();
  const entries = () => h.renders.at(-1).options.display.entries("UI", dto.modules);
  assert.deepEqual(plain(entries().map(entry => entry.name ?? entry.shown.split("\n")[0])), ["daemon", "main", "pkg"]);
  for (const index of [0, 2]) await h.renders.at(-1).options.onToggleDirectory(entries()[index]);
  const [daemon, , pkg] = entries();
  assert.deepEqual(plain(daemon.entries.map(entry => entry.name ?? entry.shown.split("\n")[0])), ["daemon/mod.rs", "client", "registry"], "mod.rs opens first as the module root");
  assert.deepEqual(plain(pkg.entries.map(entry => entry.name)), ["pkg/__init__.py", "util"]);
  await h.renders.at(-1).options.onToggleDirectory(daemon.entries[2]);
  assert.deepEqual(plain(entries()[0].entries[2].entries.map(entry => entry.name)), ["registry/mod.rs", "table"]);
});

// Walk one render's display the way the projection does: band by band, expansion before entries.
function drawnEntries(display, layers, modules) {
  let total = 0;
  const count = list => { for (const entry of list) { total++; if (entry.type === "directory" && entry.open) count(entry.entries); } };
  const lists = [];
  for (const layer of layers) {
    if (!display.expanded(layer)) { lists.push(null); continue; }
    const list = display.entries(layer, modules.filter(module => module.layer === layer)); count(list); lists.push(list);
  }
  return {total, lists};
}
test("one render draws at most the 150-chip cap, notes included, with one overflow note per truncated list", async () => {
  const h = harness(), dto = fixture(), base = dto.modules[0];
  dto.layers = Array.from({length:6}, (_, i) => ({name:`L${i}`, order:i + 1, shared:false, display:"expanded"}));
  // Each band: 60 measured single-file modules, one unmeasured file and one two-file module opened below.
  dto.modules = dto.layers.flatMap(({name}) => [
    ...Array.from({length:60}, (_, i) => ({...base, layer:name, identity:identity(`${name}/m${String(i).padStart(2, "0")}.rs`)})),
    {...base, layer:name, declarationCount:0, identity:identity(`${name}/unmeasured.rs`)},
    ...["a", "b"].map(file => ({...base, layer:name, identity:identity(`${name}/aa/${file}.rs`)}))]);
  dto.edges = [];
  h.setResponse(async () => ({payload:dto})); await h.controller.refresh();
  const first = h.renders.at(-1).options.display.entries("L0", dto.modules.filter(module => module.layer === "L0"));
  await h.renders.at(-1).options.onToggleDirectory(first[0]);
  const {display} = h.renders.at(-1).options;
  const {total, lists} = drawnEntries(display, dto.layers.map(layer => layer.name), dto.modules);
  assert.equal(total, 150, "chips and every kind of note share one exact bound");
  assert.equal(lists[0].length, 41, "40 module entries (39 chips and the overflow note), then the unmeasured note");
  assert.equal(lists[0][0].type, "directory"); assert.equal(lists[0][0].open, true); assert.equal(lists[0][0].entries.length, 2);
  assert.match(lists[0][39].label, /^22 files not drawn · diagram size cap\nselect to list this layer's files$/);
  assert.match(lists[0][40].label, /^1 file · metrics unavailable/);
  assert.deepEqual(lists.map(list => list?.length ?? null), [41, 41, 41, 25, null, null], "bands past the spent budget stay folded instead of drawing empty boxes");
  assert.match(lists[3].at(-1).label, /^37 files not drawn/, "the last list ends on its overflow note; no further notes once the budget is spent");
});

test("unhinted test and fixture layers are hidden until tests and fixtures are included; an authored hint wins", async () => {
  const h = harness();
  let hinted = null;
  const payload = role => {
    const dto = fixture("max", "rules-one", role), base = dto.modules[0]; dto.modules[1].roleCounts = {production:1,test:2};
    dto.layers.push({name:"Fixtures",order:0,shared:false,display:hinted});
    dto.modules.push({...base, identity:identity("tests/fixtures/a.rs"), layer:"Fixtures", roleCounts:{production:3,test:0}, ...(role === "production" ? {excludedReason:"fixture"} : {})},
      {...base, identity:identity("tests/b.rs"), layer:"Fixtures", roleCounts:{production:0,test:2}});
    dto.edges.push(edge("Fixtures","UI",1));
    return dto;
  };
  h.setResponse(async url => ({payload:payload(new URL(url, "http://local").searchParams.get("role"))})); await h.controller.refresh();
  assert.equal(h.renders.at(-1).options.display.layers.has("Fixtures"), false);
  assert.equal(h.get("architecture-hidden").hidden, false);
  assert.match(h.get("architecture-hidden-toggle").textContent, /1 hidden layer · 2 files · 1 production edge/);
  assert.equal(h.get("architecture-hidden-names").textContent, "Fixtures (tests/fixtures; include them to show)");
  h.get("architecture-metric-tests").checked = true; await h.get("architecture-metric-tests").fire("change");
  assert.equal(h.renders.at(-1).options.display.layers.has("Fixtures"), true, "Include tests and fixtures draws the layer");
  assert.equal(h.get("architecture-hidden").hidden, true);
  h.get("architecture-metric-tests").checked = false; await h.get("architecture-metric-tests").fire("change");
  for (const hint of ["expanded", "collapsed"]) {
    hinted = hint; await h.controller.refresh();
    assert.equal(h.renders.at(-1).options.display.layers.has("Fixtures"), true, `an authored ${hint} hint wins over the default test-layer hiding`);
    assert.equal(h.renders.at(-1).options.display.expanded("Fixtures"), hint === "expanded");
  }
});

test("details load on demand at the view's pin and rules hash, page with Show more and render text only", async () => {
  const h = harness(), payload = fixture();
  const hostile = '<img src=x onerror="globalThis.detailExecuted=true">';
  payload.references.unresolved = Array.from({length:150}, (_, i) => ({source:identity(`src/${i}.rs`),
    site:{role:"production",relationKind:"rust_path",spelling:hostile,lines:{start:1,end:1},range:{start:0,end:4}},reason:"rust_module_ownership_unproved"}));
  payload.references.counts.unresolvedSites = 150;
  h.setResponse(async () => ({payload})); await h.controller.refresh();
  assert.equal(h.detailCalls.length, 0, "drawing reads only the summary");
  const notice = label => descendants(h.get("architecture-notice-content")).find(item => item.tagName === "button" && item.textContent === label);
  await notice("Inspect unresolved sites").fire("click"); await flush();
  const query = new URL(h.detailCalls[0].url, "http://local").searchParams;
  assert.equal(query.get("kind"), "unresolved"); assert.equal(query.get("offset"), "0");
  assert.equal(query.get("indexGeneration"), pin.indexGeneration); assert.equal(query.get("indexRevision"), "1");
  assert.equal(query.get("rulesHash"), "rules-one");
  const entries = () => descendants(h.get("architecture-inspector-content")).filter(item => item.tagName === "details").length;
  assert.equal(entries(), 100);
  assert.ok(descendants(h.get("architecture-inspector-content")).some(item => item.tagName === "pre" && item.textContent.includes(JSON.stringify(hostile))),
    "repository text stays text");
  await h.button("Show more (50 more of 150)").fire("click"); await flush();
  assert.equal(entries(), 150);
  assert.equal(new URL(h.detailCalls[1].url, "http://local").searchParams.get("offset"), "100");
  assert.equal(descendants(h.get("architecture-inspector-content")).some(item => item.tagName === "button" && item.textContent.startsWith("Show more")), false);
  // A changed pin or rules file is a typed conflict, never a stale page.
  h.setResponse(async () => ({payload}));
  const conflicted = harness(); conflicted.setResponse(async () => ({payload}));
  await conflicted.controller.refresh();
  const [view, options] = [conflicted.renders[0].view, conflicted.renders[0].options];
  conflicted.setContext({rootKey:"checkout-one",session:1,pin:{...pin,indexRevision:2}});
  options.onSelectModule(view.modules[0]); await flush();
  assert.equal(conflicted.detailCalls.length, 0, "an outdated view never reads details");
});

// #449: the full stack is drawn with allowed arrows dimmed; counted chips head the inspector.
test("the full stack stays drawn with deviations, and counted review chips open their lists", async () => {
  const h = harness(), payload = authoringFixture();
  payload.layers.push({name:"Quiet",order:3,shared:false}, {name:"Shared",order:null,shared:true});
  payload.rules.counts = {comparable:897, conforms:311, deviates:23, unknown:563, excepted:0}; payload.rules.unassignedCount = 156; payload.exceptionCount = 0;
  payload.modules[1].roleCounts = {production:1, test:0};
  h.setResponse(async () => ({payload})); await h.controller.refresh();
  const {options} = h.renders[0];
  assert.deepEqual([...options.display.layers].sort(), ["Quiet", "Shared", "Storage", "UI"], "no layer is dropped for having no deviations or edges");
  assert.equal(payload.edges.every(edge => options.display.edge(edge)), true);
  assert.equal(h.get("architecture-inspector").hidden, false, "the review overview rests in the inspector");
  assert.equal(h.get("architecture-inspector-title").textContent, "Architecture review");
  const chips = () => descendants(h.get("architecture-inspector-content")).filter(item => item.className === "architecture-chip");
  assert.deepEqual(chips().map(item => item.textContent), ["23 deviations", "1 unassigned file", "0 exceptions"]);
  assert.equal(chips()[0].dataset.tone, "error");
  assert.doesNotMatch(text(h.get("architecture-inspector-content")), /reference sites mapped/, "#525: the coverage sentence is said once, under the arrow key");
  assert.match(text(h.get("architecture-notice-content")), /563 of 897 production rule instances unknown/, "rule counts sit behind the ⓘ");
  await h.get("architecture-review").fire("click");await h.get("architecture-deviations-review").fire("click"); await flush();
  assert.equal(h.get("architecture-review-panel").hidden, false);
  assert.equal(h.get("architecture-review-role").value, "production"); assert.equal(h.get("architecture-review-state").value, "deviates");
  const query = new URL(h.detailCalls.at(-1).url, "http://local").searchParams;
  assert.equal(query.get("role"), "production"); assert.equal(query.get("status"), "deviates"); assert.equal(query.get("excepted"), "false");
  options.onSelectModule(payload.modules[0]); await flush();
  assert.equal(h.get("architecture-inspector-title").textContent, "src/a.rs");
  assert.deepEqual(chips().map(item => item.textContent), ["23 deviations", "1 unassigned file", "0 exceptions"], "every inspection keeps the counted chips");
  await h.get("architecture-unassigned").fire("click");
  assert.match(text(h.get("architecture-review-list")), /src\/no-edges.rs/);
});

// #449 review (P2): the collapsed unassigned row draws metric chips, so its measured modules
// belong to the heat scale whether or not the row is open.
test("an all-unassigned view colours its collapsed chips with fixed heat, not 'No measured values'", async () => {
  const h = harness(), payload = fixture();
  payload.modules = [["src/hot.rs", 40], ["src/mild.rs", 3]].map(([path, cognitive]) => ({...payload.modules[0], layer:null, identity:identity(path), metrics:{cognitive}}));
  payload.edges = [];
  h.setResponse(async () => ({payload})); await h.controller.refresh();
  const {options} = h.renders.at(-1);
  assert.equal(options.display.unassigned.expanded, false, "the unassigned row starts collapsed");
  assert.doesNotMatch(text(h.get("architecture-legend")), /No measured values/);
  assert.deepEqual(payload.modules.map(options.moduleBand), [3, 0]);
  options.display.reset();
  assert.deepEqual(JSON.parse(JSON.stringify(options.display.summary(null, payload.modules, 4))).map(chip => [chip.shown, chip.band]), [["hot\n40", 3], ["mild\n3", 0]]);
});

test("Relative cuts include the collapsed unassigned row and stay stable when it opens", async () => {
  const h = harness(), payload = fixture();
  payload.modules = [...payload.modules.filter(module => module.layer === "UI"),
    ...[["src/x.rs", 50], ["src/y.rs", 20], ["src/z.rs", 9]].map(([path, cognitive]) => ({...payload.modules[0], layer:null, identity:identity(path), metrics:{cognitive}}))];
  h.setResponse(async () => ({payload})); await h.controller.refresh();
  h.get("architecture-relative").checked = true; await h.get("architecture-relative").fire("change");
  const before = h.renders.at(-1), legend = text(h.get("architecture-legend")), bands = payload.modules.map(before.options.moduleBand);
  assert.match(legend, /Relative to visible set.*>20–50|Relative to visible set.*50/, "the unassigned values set the cuts while the row is collapsed");
  await before.options.onToggleUnassigned(); await flush();
  const after = h.renders.at(-1);
  assert.notEqual(after, before); assert.equal(after.options.display.unassigned.expanded, true);
  assert.equal(text(h.get("architecture-legend")), legend, "opening the row does not move the cuts");
  assert.deepEqual(payload.modules.map(after.options.moduleBand), bands);
});

test("without a refresh button, a rules conflict in a detail list offers an explicit Reload Architecture", async () => {
  const h = harness(); await h.controller.refresh();
  const error = new Error("rules changed"); error.code = "rules_conflict"; error.status = 409; h.setDetailError(error);
  h.renders[0].options.onSelectModule(h.renders[0].view.modules[0]); await flush();
  assert.match(text(h.get("architecture-inspector-content")), /Rules changed since this diagram was loaded\./);
  assert.equal(h.calls.length, 1);
  h.setDetailError(null); await h.button("Reload Architecture").fire("click");
  assert.equal(h.calls.length, 2, "the button re-reads the summary at the same pin");
  assert.match(h.calls[1].url, /^\/api\/architecture\?/);
});

test("a collapsed band draws one row of metric-coloured module chips, hottest first, within the size cap", async () => {
  const h = harness(), payload = fixture();
  payload.modules = ["src/cold.rs", "src/hot.rs", "src/warm.rs", "src/warm/inner.rs", "src/extra.rs", "src/none.rs"].map((path, i) =>
    ({...payload.modules[0], identity:identity(path), declarationCount:path === "src/none.rs" ? 0 : 1, metrics:{cognitive:[1, 40, 12, 20, 2, null][i]}}));
  h.setResponse(async () => ({payload})); await h.controller.refresh();
  const {options} = h.renders[0], members = payload.modules.filter(module => module.layer === "UI");
  options.display.reset();
  const chips = JSON.parse(JSON.stringify(options.display.summary("UI", members, 3)));
  assert.deepEqual(chips.map(chip => chip.shown), ["hot\n40", "warm\n20", "+2 more\nopen band"], "measured modules only; a file and its directory are one chip");
  assert.deepEqual(chips.map(chip => chip.band), [3, 2, null]);
  assert.equal(chips[1].label, "warm · 2 files · cognitive 20");
  assert.equal(chips[2].label, "2 more modules · open the band to see them");
  assert.equal(options.display.summary("UI", members, 10).length, 4, "everything fits when the row is wide enough");
});

test("#523: inspectors describe semantic evidence for mixed edges and bound rule instances", async () => {
  // Shapes from the semantic fixture: a mixed low → high edge whose sites are partly bound.
  const h = harness(), payload = authoringFixture();
  payload.evidenceTier = "mixed";
  Object.assign(payload.edges[0], {evidenceTier:"mixed", semanticSites:2,
    coverage:{sites:3,mappedSites:3,unresolvedSites:0,semanticSites:2,complete:true}});
  const binding = {document:{sourceSetId:"s",language:"rust",path:"src/a.rs"},revisionId:"r",range:{start:1,end:2},
    kind:"semanticBinding",syntaxId:null,callId:null,provenanceId:"call-orphan-0"};
  payload.rules.instances[0].evidence = [binding];
  h.setResponse(async () => ({payload})); await h.controller.refresh();
  h.renders[0].options.onSelectEdge(payload.edges[0]); await flush();
  const edgeText = text(h.get("architecture-inspector-content"));
  assert.match(edgeText, /Syntax evidence and fresh semantic bindings · not semantically complete/);
  assert.match(edgeText, /reference sites, 2 bound by semantic evidence/);
  assert.doesNotMatch(edgeText, /not compiler-resolved calls/);
  const rule = descendants(h.get("architecture-review-list")).find(item => item.tagName === "button" && item.textContent.startsWith("$layers · "));
  assert.ok(rule); await rule.fire("click"); await flush();
  const ruleText = text(h.get("architecture-inspector-content"));
  assert.match(ruleText, /Semantic evidence names a fresh producer binding/);
  assert.doesNotMatch(ruleText, /not destination symbol bindings/);
  // A syntax-only snapshot keeps the syntax wording.
  const plain = harness(), syntax = authoringFixture();
  plain.setResponse(async () => ({payload:syntax})); await plain.controller.refresh();
  plain.renders[0].options.onSelectEdge(syntax.edges[0]); await flush();
  assert.match(text(plain.get("architecture-inspector-content")), /Syntax evidence · not compiler-resolved calls.*structural reference sites/s);
});

// Deviations tree (#786): the module loads into node:vm with a small DOM stub and stub deps.
const deviationsSource = fs.readFileSync(path.join(__dirname, "../web/architecture-deviations.js"), "utf8");
const plain = value => JSON.parse(JSON.stringify(value));
const deq = (actual, expected, message) => assert.deepEqual(plain(actual), plain(expected), message);
const callId = letter => `occ:v2:${letter.repeat(32)}`;
const docId = (file, language = "rust") => ({sourceSetId:"workspace",language,path:file,sid:`sid:${file}`});
const evidenceItem = props => ({document:docId("src/store.rs"),kind:"structuralReference",callId:null,caller:null,callerReason:null,spelling:"x",owner:null,
  target:null,targetReason:"structural_only",lines:null,range:{start:0,end:1},...props});
function deviationRows(hostile = "(anonymous)") {
  const store = docId("src/store.rs"), rules = docId("src/rules.rs"), peer = docId("src/peer.rs"), other = docId("src/other.rs");
  const o1 = {syntaxId:"o1",name:"enqueue",stableLookupKey:"S::enqueue"};
  const caller = {stableLookupKey:"S::enqueue",syntaxId:"o1",name:"enqueue",document:store};
  const definition = {syntaxId:"d1",displayKey:"rules::load",document:rules,lines:{start:5,end:9}};
  const row = (target, destination, sourceLayer, destinationLayer, referenceCount, kindCounts, evidence) => ({role:"production",
    instance:{rule:"$layers",status:"deviates",excepted:false,sourceLayer,destinationLayer,target,destination,referenceCount,kindCounts,evidence}});
  return [
    row(store, rules, "Storage", "UI", 10, {call:3,type:4,import:1,reference:2}, [
      evidenceItem({relationKind:"call",callId:callId("a"),caller,owner:o1,lines:{start:20,end:20},range:{start:200,end:210},target:definition,targetReason:null,spelling:"load"}),
      evidenceItem({relationKind:"call",callId:callId("b"),owner:{syntaxId:store.sid,name:null,stableLookupKey:null},lines:{start:10,end:10},range:{start:100,end:110}}),
      evidenceItem({relationKind:"type",owner:o1,lines:{start:12,end:12},range:{start:120,end:125},target:definition,targetReason:null,spelling:"Rules"}),
      evidenceItem({relationKind:"reference",owner:{syntaxId:"o2",name:hostile === "(anonymous)" ? null : hostile,stableLookupKey:null},lines:{start:30,end:31},range:{start:300,end:310},targetReason:"unresolved",spelling:hostile === "(anonymous)" ? "x" : hostile}),
      evidenceItem({relationKind:"import",callId:callId("c"),caller:{...caller,syntaxId:"o3",stableLookupKey:"S::boot",name:"boot"},owner:{syntaxId:"o3",name:"boot",stableLookupKey:"S::boot"},lines:{start:2,end:2},range:{start:20,end:30},targetReason:"external"}),
    ]),
    row(store, peer, "Storage", "Peer", 2, {call:0,type:0,import:0,reference:2}, [
      evidenceItem({relationKind:"reference",owner:o1,lines:{start:40,end:40},range:{start:400,end:410}}),
      evidenceItem({relationKind:"reference",owner:o1,lines:{start:41,end:41},range:{start:410,end:420}}),
    ]),
    row(other, rules, "Storage", "UI", 1, {call:1,type:0,import:0,reference:0}, [
      evidenceItem({document:other,relationKind:"call",callId:callId("d"),owner:null,lines:{start:3,end:3},range:{start:30,end:40}}),
    ]),
  ];
}
const deviationView = () => ({...pin,state:"ready",
  layers:[{name:"UI",order:1,shared:false},{name:"Storage",order:2,shared:false},{name:"Peer",order:2,shared:false}],
  edges:[
    {sourceLayer:"Storage",destinationLayer:"UI",role:"production",status:"deviates",excepted:false,count:11},
    {sourceLayer:"Storage",destinationLayer:"Peer",role:"production",status:"deviates",excepted:false,count:2},
    {sourceLayer:"Storage",destinationLayer:"UI",role:"production",status:"deviates",excepted:true,count:99},
    {sourceLayer:"Storage",destinationLayer:"UI",role:"test",status:"deviates",excepted:false,count:50},
    {sourceLayer:"UI",destinationLayer:"Storage",role:"production",status:"conforms",excepted:false,count:5}],
  rules:{counts:{unknown:2}}});
const deviationEdgeText = edge => ({bound:edge.destinationLayer === "Peer"});

function dnode(tagName) {
  return {tagName,id:"",className:"",textContent:"",children:[],parent:null,dataset:{},attrs:{},listeners:{},hidden:false,checked:false,type:"",
    append(...items) {for (const item of items) {item.parent = this;this.children.push(item);}},
    replaceChildren(...items) {this.children = [];this.textContent = "";this.append(...items);},
    setAttribute(key, value) {this.attrs[key] = String(value);},
    addEventListener(name, fn) {(this.listeners[name] ||= []).push(fn);},
    closest(selector) {for (let n = this; n; n = n.parent) if (n.className.split(" ").includes(selector.slice(1))) return n; return null;},
    focus() {this.focusedBy.active = this;}, scrollIntoView() {this.scrolled = true;},
    async dispatch(name, props = {}) {
      const event = {type:name,target:this,preventDefault() {this.prevented = true;},stopPropagation() {this.stopped = true;},...props};
      for (let n = this; n && !event.stopped; n = n.parent) {event.currentTarget = n;for (const fn of n.listeners[name] || []) await fn(event);}
      return event;
    }};
}
function deviationsHarness(options = {}) {
  const created = [], statics = new Map(), focus = {active:null}, menus = [], inspections = [], calls = {reads:[],sequences:[],drafts:[],opened:[],details:[],overview:0,conflicts:0,refreshes:0};
  const track = node => {node.focusedBy = focus;return node;};
  const document = {createElement:tag => {const node = track(dnode(tag));created.push(node);return node;},
    getElementById:id => created.findLast(item => item.id === id) || (statics.has(`#${id}`) ? statics.get(`#${id}`) : statics.set(`#${id}`, track(dnode("static"))).get(`#${id}`)),
    querySelector:selector => statics.has(selector) ? statics.get(selector) : statics.set(selector, track(dnode("static"))).get(selector)};
  const rows = options.rows || deviationRows(), state = {stale:false,usable:true,pages:options.pages || [[0, 2], [2, 3]],failAt:options.failAt ?? null,error:options.error ?? null,
    readError:null,eligible:true,view:options.view || deviationView()};
  const sandbox = {document,console,navigator:{clipboard:{writeText:async value => {sandbox.copied = value;}}},window:{TrellisClasses:{showContextMenu(event, actions, opts) {
    menus.push({target:event.currentTarget,actions,options:opts});event.stopPropagation();}}}};
  vm.createContext(sandbox);
  vm.runInContext(pinSource, sandbox);
  vm.runInContext(deviationsSource, sandbox);
  const deps = {
    details:async (params, active, offset) => {
      calls.details.push({params:plain(params),offset});
      if (state.error && calls.details.length > (state.failAt ?? 0)) throw state.error;
      const index = state.pages.findIndex(([from]) => from === offset), [from, to] = state.pages[index];
      return {items:rows.slice(from, to),nextOffset:index + 1 < state.pages.length ? state.pages[index + 1][0] : null,total:rows.length};
    },
    current:() => state.usable,basis:() => ({pin,rulesHash:"rules-one"}),view:() => state.view,targetLabel:identity => identity.path,
    eligible:() => state.eligible,portable:identity => ({path:identity.path}),beginDraft:async (...args) => {calls.drafts.push(plain(args));},
    edgeText:deviationEdgeText,
    inspectLine:(record, render) => {
      const target = track(dnode("div")), title = track(dnode("h3")), active = () => !state.stale && !target.dead;
      inspections.push({record,target,title});render(target, title, active);return active;
    },
    overview:() => {calls.overview++;},openReview:(...args) => {calls.opened.push(args);},onConflict:() => {calls.conflicts++;},
    refresh:async () => {calls.refreshes++;},
    readSource:async (...args) => {if (state.readError) throw state.readError;calls.reads.push(args);},
    openSequence:async (...args) => {calls.sequences.push(args);},snapshot:() => ({snap:true}),
  };
  const dev = sandbox.window.TrellisDeviations.create(deps);
  const get = id => document.getElementById(id);
  const treeRows = () => descendants(get("architecture-deviations-tree")).filter(item => item.className.split(" ").includes("deviation-row"));
  const byLabel = start => treeRows().find(item => item.attrs["aria-label"].startsWith(start));
  const expandAll = async () => {for (let open = treeRows().find(item => item.attrs["aria-expanded"] === "false"); open; open = treeRows().find(item => item.attrs["aria-expanded"] === "false")) await open.dispatch("click");};
  const menuOf = row => descendants(row).find(item => item.className === "deviation-actions");
  return {dev,deps,get,sandbox,state,calls,menus,inspections,focus,rows,treeRows,byLabel,expandAll,menuOf,statics,
    notices:() => text(get("architecture-deviations-notices")),feedback:() => text(get("architecture-deviations-notices").children[3]).trim(),trigger:track(dnode("button"))};
}

test("deviation model: valid is a shape check and one bad row invalidates the read", () => {
  const {model} = deviationsHarness().sandbox.window.TrellisDeviations, rows = deviationRows();
  assert.equal(rows.every(model.valid), true);
  const bad = mutate => {const copy = deviationRows();mutate(copy[0].instance);return model.valid(copy[0]);};
  assert.equal(bad(i => {i.kindCounts.call = -1;}), false);
  assert.equal(bad(i => {i.kindCounts.type = 1.5;}), false);
  assert.equal(bad(i => {delete i.kindCounts.import;}), false);
  assert.equal(bad(i => {i.evidence[0].relationKind = "rust_path";}), false);
  assert.equal(bad(i => {i.evidence[0].range = null;}), false);
  assert.equal(bad(i => {i.evidence = null;}), false);
  assert.equal(bad(i => {i.kindCounts.call = 99;}), true, "no sum re-check");
});

test("deviation model: build groups files, functions and lines, with per-file exactness", () => {
  const {model} = deviationsHarness().sandbox.window.TrellisDeviations;
  const layers = deviationView().layers, tree = model.build(deviationRows(), layers);
  deq(plain(tree.totals), {files:2,references:13,calls:4});
  deq(tree.files.map(file => file.path), ["src/store.rs", "src/other.rs"]);
  const store = tree.files[0];
  deq(plain(store.edges.map(({key, direction, count}) => [key, direction, count])), [["Storage→UI", "up", 10], ["Storage→Peer", "cross", 2]]);
  deq([store.references, store.calls], [12, 3]);
  deq(plain(store.kinds), {call:3,type:4,import:1,reference:4});
  deq(plain(store.unlisted), {call:1,type:3,import:0,reference:1});
  deq(store.functions.map(fn => fn.label), ["S::enqueue", "(module level)", "(anonymous)", "S::boot"], "a call-tagged import is not a call, so S::boot has no call lines");
  deq(store.functions[0].lines.map(line => line.evidence.lines.start), [12, 20, 40, 41], "lines follow the site line");
  assert.equal(store.functions[0].callsExact, false, "2 of 3 calls are listed, so every function of the file is a bound");
  assert.equal(store.functions[2].refsExact, false);
  assert.equal(tree.files[1].functions[0].callsExact, true);
  assert.equal(tree.files[1].functions[0].label, "(module level)", "a null owner is the module");
  deq(plain(tree.files[1].unlisted), {call:0,type:0,import:0,reference:0});
});

test("deviation model: module level only for the file module, otherwise a name or anonymous", () => {
  const {model} = deviationsHarness().sandbox.window.TrellisDeviations, rows = deviationRows();
  rows[2].instance.evidence[0].owner = {syntaxId:"sid:src/elsewhere.rs",name:null,stableLookupKey:null};
  assert.equal(model.build(rows, deviationView().layers).files[1].functions[0].label, "(anonymous)");
  rows[2].instance.evidence[0].owner = {syntaxId:"x",name:"helper",stableLookupKey:null};
  assert.equal(model.build(rows, deviationView().layers).files[1].functions[0].label, "helper");
});

test("deviation model: filter by edge and calls only", () => {
  const {model} = deviationsHarness().sandbox.window.TrellisDeviations, layers = deviationView().layers, tree = model.build(deviationRows(), layers);
  const peer = model.filter(tree, {edge:"Storage→Peer", callsOnly:false});
  deq(plain(peer.totals), {files:1,references:2,calls:0});
  assert.equal(peer.files[0].functions[0].callsExact, true, "exactness follows the instances that remain");
  deq(plain(peer.files[0].unlisted), {call:0,type:0,import:0,reference:0});
  const calls = model.filter(tree, {edge:null, callsOnly:true});
  deq(plain(calls.totals).files, 2);
  deq(calls.files[0].functions.map(fn => fn.label), ["(module level)", "S::enqueue"], "S::boot holds only a call-tagged import");
  assert.equal(calls.files[0].functions.some(fn => fn.label === "(anonymous)"), false, "functions with no call are hidden");
  assert.equal(calls.files[0].functions.find(fn => fn.label === "S::enqueue").lines.length, 1);
  assert.equal(calls.files[0].edges.some(edge => edge.key === "Storage→Peer"), false);
  assert.equal(model.filter(tree, {edge:"Storage→Peer", callsOnly:true}).files.length, 0, "no calls on the peer edge");
});

test("deviation model: a require with a caller is tagged a call but counted as an import", () => {
  const {model} = deviationsHarness().sandbox.window.TrellisDeviations, layers = deviationView().layers;
  const store = docId("src/req.js", "javascript"), rules = docId("src/rules.js", "javascript");
  const boot = {syntaxId:"o3",name:"boot",stableLookupKey:"S::boot"}, caller = {stableLookupKey:"S::boot",syntaxId:"o3",name:"boot",document:store};
  const require = evidenceItem({document:store,relationKind:"import",callId:callId("c"),caller,owner:boot,lines:{start:2,end:2},range:{start:20,end:30}});
  const rows = [{role:"production",instance:{rule:"$layers",status:"deviates",excepted:false,sourceLayer:"Storage",destinationLayer:"UI",target:store,destination:rules,
    referenceCount:1,kindCounts:{call:0,type:0,import:1,reference:0},evidence:[require]}}];
  const tree = model.build(rows, layers), file = tree.files[0];
  assert.equal(file.functions[0].lines[0].kind, "call", "the tag keeps the caller rule");
  deq([file.calls, file.functions[0].calls], [0, 0], "file and function call totals agree");
  const calls = model.filter(tree, {edge:null, callsOnly:true});
  deq(plain(calls.totals), {files:0,references:0,calls:0}, "a file whose only call-tagged line is a require is hidden under Calls only");
  const mixed = deviationRows();
  mixed[0].instance.evidence = mixed[0].instance.evidence.filter(item => item.relationKind === "import");
  mixed[0].instance.kindCounts = {call:0,type:0,import:1,reference:0};
  mixed[0].instance.referenceCount = 1;
  const none = model.filter(model.build(mixed, layers), {edge:null, callsOnly:true});
  deq(none.files.map(item => item.path), ["src/other.rs"], "the require-only file leaves the Calls only list");
});

test("deviation model: kind tag, chips with bound, exception note, sequence test, line key", () => {
  const {model} = deviationsHarness().sandbox.window.TrellisDeviations;
  assert.equal(model.kindOf({caller:{},relationKind:"import"}), "call", "a caller makes it a call");
  assert.equal(model.kindOf({caller:null,relationKind:"type",callId:callId("a")}), "type", "never by callId");
  assert.equal(model.kindOf({relationKind:"call"}), "call");
  const chips = model.chips(deviationView(), deviationEdgeText);
  deq(plain(chips.all), {count:13,bound:true});
  deq(plain(chips.edges.map(({key, direction, count, bound}) => [key, direction, count, bound])),
    [["Storage→UI", "up", 11, false], ["Storage→Peer", "cross", 2, true]]);
  assert.equal(model.exceptionNote(1, "a.rs", "b.rs"), "Approves all 1 reference from a.rs to b.rs, including future ones on this pair.");
  assert.equal(model.exceptionNote(537, "a.rs", "b.rs"), "Approves all 537 references from a.rs to b.rs, including future ones on this pair.");
  assert.equal(model.canOpenSequence({caller:{syntaxId:"s",stableLookupKey:"k"},callId:callId("a"),relationKind:"import"}), true);
  assert.equal(model.canOpenSequence({caller:{syntaxId:"s",stableLookupKey:"k"},callId:null}), false);
  assert.equal(model.canOpenSequence({caller:null,callId:callId("a")}), false);
  const [first] = deviationRows();
  assert.equal(model.lineKey(first.instance, first.instance.evidence[0]),
    JSON.stringify(["workspace", "src/store.rs", JSON.stringify(["workspace", "rust", "src/rules.rs"]), 200, 210, "call", callId("a")]));
});

test("deviations keyboard actions", () => {
  const {model} = deviationsHarness().sandbox.window.TrellisDeviations;
  const rows = [{key:"f1",level:1,expandable:true,expanded:true,parent:null},{key:"fn",level:2,expandable:true,expanded:false,parent:"f1"},
    {key:"l",level:3,expandable:false,expanded:false,parent:"fn"},{key:"f2",level:1,expandable:true,expanded:false,parent:null},
    {key:"more",level:2,expandable:false,expanded:false,parent:"f2"}];
  const act = (index, key) => plain(model.keyAction(rows, index, key));
  deq(act(0, "ArrowDown"), {focus:1});
  deq(act(4, "ArrowDown"), {focus:4}, "clamped at the end");
  deq(act(0, "ArrowUp"), {focus:0}, "clamped at the start");
  deq(act(2, "Home"), {focus:0});
  deq(act(1, "End"), {focus:4});
  deq(act(1, "ArrowRight"), {focus:1,expand:"fn"});
  deq(act(0, "ArrowRight"), {focus:1}, "an expanded row moves to its first child");
  deq(act(2, "ArrowRight"), {focus:2}, "a line does nothing");
  deq(act(0, "ArrowLeft"), {focus:0,collapse:"f1"});
  deq(act(1, "ArrowLeft"), {focus:0}, "a collapsed row moves to its parent");
  deq(act(2, "ArrowLeft"), {focus:1});
  deq(act(3, "ArrowLeft"), {focus:3}, "a level-1 row does nothing");
  deq(act(0, "Enter"), {focus:0,toggle:"f1"});
  deq(act(1, " "), {focus:1,toggle:"fn"});
  deq(act(2, "Enter"), {focus:2,select:"l"});
  deq(act(4, "Enter"), {focus:4}, "the unlisted row has no action");
  assert.equal(model.keyAction(rows, 0, "a"), null);
  assert.equal(model.keyAction(rows, 0, "Tab"), null);
});

test("one unknown rule instance reads in the singular", async () => {
  const h = deviationsHarness({view:{...deviationView(), rules:{counts:{unknown:1}}}});
  await h.dev.open(h.trigger);
  assert.match(h.notices(), /1 unknown rule instance is not a deviation\./);
});

test("deviations read sends the five filters and pages until nextOffset is null", async () => {
  const h = deviationsHarness();
  await h.dev.open(h.trigger);
  deq(h.calls.details.map(call => call.offset), [0, 2]);
  deq(h.calls.details[0].params, {kind:"instances",rule:"$layers",status:"deviates",role:"production",excepted:"false",limit:"500"});
  assert.equal(h.dev.isOpen(), true);
  assert.equal(h.get("architecture-deviations").hidden, false);
  assert.equal(h.statics.get("#architecture-diagram").hidden, true);
  assert.equal(h.get("architecture-deviations-summary").textContent, "2 files · 13 deviating references · 4 calls");
  assert.equal(h.get("architecture-deviations-tree").attrs["aria-busy"], "false");
  assert.equal(h.get("architecture-deviations-notices").attrs.role, "status");
  assert.match(h.notices(), /2 unknown rule instances are not deviations\./);
  await h.get("architecture-deviations-inspect").dispatch("click");
  deq(h.calls.opened, [["rules", "unknown"]]);
  h.dev.close();
  assert.equal(h.statics.get("#architecture-diagram").hidden, false);
  assert.equal(h.calls.overview, 1);
  assert.equal(h.focus.active, h.statics.get("#architecture-review"));
});

test("deviation rows: names, badges, kind tags, tabindex and unlisted footer", async () => {
  const h = deviationsHarness();
  await h.dev.open(h.trigger);
  const file = h.byLabel("src/store.rs");
  assert.equal(file.attrs["aria-label"], "src/store.rs, 3 calls, 12 references, upward deviations: UI 10, deviations: Peer 2");
  assert.match(text(file), /↑ UI · 10 deviations/);
  assert.match(text(file), /⇄ Peer · 2 deviations/);
  assert.equal(file.attrs["aria-expanded"], "false");
  await h.expandAll();
  const enqueue = h.byLabel("S::enqueue");
  assert.equal(enqueue.attrs["aria-label"], "S::enqueue, calls: at least 1, sampled, references: at least 4, sampled");
  assert.equal(h.byLabel("src/other.rs").attrs["aria-label"], "src/other.rs, 1 call, 1 reference, upward deviations: UI 1");
  const more = h.treeRows().find(row => row.attrs["aria-label"].startsWith("+ 5 more not listed"));
  assert.equal(more.attrs["aria-label"], "+ 5 more not listed: 1 call · 3 type uses · 1 reference");
  assert.equal(more.attrs["aria-expanded"], undefined);
  const kinds = h.treeRows().filter(row => row.dataset.level === "3").map(row => descendants(row).find(item => item.className.includes("deviation-kind")));
  deq(kinds.map(tag => [tag.dataset.kind, tag.textContent]).sort(), [["call", "call"], ["call", "call"], ["call", "call"], ["call", "call"], ["reference", "reference"], ["reference", "reference"], ["reference", "reference"], ["type", "type use"]].sort());
  const unresolved = h.treeRows().find(row => /no semantic binding for this site/.test(text(row)));
  assert.match(text(unresolved), /→ src\/rules\.rs · file only/);
  assert.match(unresolved.attrs["aria-label"], /file only: no semantic binding for this site$/);
  const bound = h.treeRows().find(row => /^Line 20,/.test(row.attrs["aria-label"]));
  assert.equal(bound.attrs["aria-label"], "Line 20, rules::load, call, defined at src/rules.rs:5");
  assert.match(text(bound), /→ src\/rules\.rs:5/);
  const stops = h.treeRows().filter(row => row.attrs.tabindex === "0");
  assert.equal(stops.length, 1, "one tab stop in the tree");
  assert.equal(descendants(h.get("architecture-deviations-tree")).filter(item => item.className === "deviation-actions" && item.attrs.tabindex === "0").length, 0);
  await bound.dispatch("click");
  assert.equal(h.treeRows().filter(row => row.attrs.tabindex === "0").length, 1);
  assert.equal(h.menuOf(bound).attrs.tabindex, "0");
  assert.equal(descendants(h.get("architecture-deviations-tree")).filter(item => item.className === "deviation-actions" && item.attrs.tabindex === "0").length, 1);
  assert.equal(h.menuOf(bound).attrs["aria-label"], "Actions for line 20 in src/store.rs");
  assert.equal(h.menuOf(bound).attrs["aria-haspopup"], "menu");
});

test("deviations hostile repository text stays literal", async () => {
  const hostile = "<img src=x onerror=alert(1)>";
  const h = deviationsHarness({rows:deviationRows(hostile)});
  await h.dev.open(h.trigger);
  await h.expandAll();
  assert.equal(descendants(h.get("architecture-deviations")).some(item => item.tagName === "img"), false);
  assert.ok(h.treeRows().some(row => text(row).includes(hostile)));
  const line = h.treeRows().find(row => row.dataset.level === "3" && row.attrs["aria-label"].includes(hostile));
  await line.dispatch("click");
  assert.ok(text(h.inspections.at(-1).target).includes(hostile));
});

test("deviations errors: conflict, rules_conflict, 503 and invalid copy; no partial tree", async () => {
  const cases = [
    [Object.assign(new Error("x"), {status:409,code:"revision_conflict"}), "The index moved to a newer revision. Reload deviations.", 1],
    [Object.assign(new Error("x"), {status:409,code:"pin_expired"}), "The index moved to a newer revision. Reload deviations.", 1],
    [Object.assign(new Error("x"), {status:409,code:"rules_conflict"}), "Rules changed since these deviations were loaded. Reload deviations.", 0],
    [Object.assign(new Error("x"), {status:503,code:"architecture_building"}), "Architecture is still building for this revision.", 0],
  ];
  for (const [error, copy, conflicts] of cases) {
    const h = deviationsHarness({error,failAt:1});
    await h.dev.open(h.trigger);
    assert.equal(h.calls.details.length, 2, "page 2 is the failing read");
    assert.match(h.notices(), new RegExp(copy.replace(/[.]/g, "\\.")));
    assert.equal(h.treeRows().length, 0, "page 1 is discarded");
    assert.equal(h.get("architecture-deviations-summary").textContent, "");
    assert.equal(h.calls.conflicts, conflicts);
    assert.ok(h.get("architecture-deviations-reload"));
  }
  const rows = deviationRows();
  rows[2].instance.kindCounts = {call:1};
  const invalid = deviationsHarness({rows});
  await invalid.dev.open(invalid.trigger);
  assert.match(invalid.notices(), /Invalid Architecture detail response\. Nothing displayed\./);
  assert.equal(invalid.treeRows().length, 0);
});

test("deviations reload reopens with the same filters and no selection", async () => {
  const h = deviationsHarness({error:Object.assign(new Error("x"), {status:409,code:"rules_conflict"}),failAt:1});
  await h.dev.open(h.trigger);
  await h.get("architecture-deviations-reload").dispatch("click");
  assert.equal(h.calls.refreshes, 1);
  h.dev.reset(true);
  assert.equal(h.dev.isOpen(), false);
  assert.equal(h.statics.get("#architecture-diagram").hidden, false);
  h.state.error = null;
  await h.dev.afterRefresh();
  assert.equal(h.dev.isOpen(), true);
  assert.equal(h.treeRows().length, 2);
  const chip = name => descendants(h.get("architecture-deviations-edges")).find(item => item.dataset.key === name);
  await chip("Storage→UI").dispatch("click");
  await h.get("architecture-deviations-calls").dispatch("change", {});
  h.get("architecture-deviations-calls").checked = true;
  await h.get("architecture-deviations-calls").dispatch("change");
  await h.expandAll();
  await h.treeRows().find(row => row.dataset.level === "3").dispatch("click");
  h.dev.reopenAfterRefresh();
  h.dev.reset(true);
  await h.dev.afterRefresh();
  assert.equal(chip("Storage→UI").attrs["aria-pressed"], "true");
  assert.equal(h.get("architecture-deviations-calls").checked, true);
  assert.equal(h.dev.snapshot().selected, null);
  assert.equal(h.treeRows().filter(row => row.attrs["aria-selected"] === "true").length, 0);
  await h.dev.afterRefresh();
});

test("a reset that is not a refresh drops the pending reopen request", async () => {
  const h = deviationsHarness();
  await h.dev.open(h.trigger);
  h.dev.reopenAfterRefresh();
  h.dev.reset();
  await h.dev.afterRefresh();
  assert.equal(h.dev.isOpen(), false, "a later successful refresh does not reopen the tree");
  await h.dev.open(h.trigger);
  h.dev.reopenAfterRefresh();
  h.dev.reset(true);
  await h.dev.afterRefresh();
  assert.equal(h.dev.isOpen(), true, "the refresh's own reset keeps the request");
});

test("deviations line menu: three triggers pass the same actions and label", async () => {
  const h = deviationsHarness();
  await h.dev.open(h.trigger);
  await h.expandAll();
  const line = h.treeRows().find(row => row.dataset.level === "3" && /Line 20,/.test(row.attrs["aria-label"]));
  await line.dispatch("contextmenu");
  await line.dispatch("keydown", {key:"ContextMenu"});
  await line.dispatch("keydown", {key:"F10",shiftKey:true});
  await h.menuOf(line).dispatch("click");
  assert.equal(h.menus.length, 4);
  assert.deepEqual(h.menus.map(menu => menu.target === line ? "row" : menu.target === h.menuOf(line) ? "button" : "other"), ["row", "row", "row", "button"]);
  for (const menu of h.menus) {
    deq(plain(menu.options), {label:"Line actions"});
    deq(menu.actions.map(action => action.label), ["Open in Sequence", "Go to call site", "Go to definition · src/rules.rs:5", "Copy location", "Add exception for this file pair…"]);
  }
  assert.equal(h.inspections.at(-1).record.evidence.range.start, 200, "opening the menu selects the line");
  assert.equal(line.attrs["aria-selected"], "true");
  const plainKey = await line.dispatch("keydown", {key:"a"});
  assert.equal(h.menus.length, 4);
  assert.equal(plainKey.prevented, undefined);
  const noTarget = h.treeRows().find(row => row.dataset.level === "3" && /Line 10,/.test(row.attrs["aria-label"]));
  await noTarget.dispatch("contextmenu");
  deq(h.menus.at(-1).actions.map(action => action.label), ["Go to call site", "Copy location", "Add exception for this file pair…"],
    "unavailable items are omitted: no caller key and no target");
  h.state.eligible = false;
  await noTarget.dispatch("contextmenu");
  deq(h.menus.at(-1).actions.map(action => action.label), ["Go to call site", "Copy location"]);
});

test("deviations line actions call the open hooks with the captured evidence", async () => {
  const h = deviationsHarness();
  await h.dev.open(h.trigger);
  await h.expandAll();
  const line = h.treeRows().find(row => row.dataset.level === "3" && /Line 20,/.test(row.attrs["aria-label"]));
  await line.dispatch("contextmenu");
  const run = label => h.menus.at(-1).actions.find(action => action.label.startsWith(label)).run();
  await run("Open in Sequence");
  const [caller, sites, revision, snap] = h.calls.sequences[0];
  assert.equal(caller.syntaxId, "o1");
  assert.equal(sites.length, 1);
  assert.equal(sites[0].callId, callId("a"));
  assert.equal(revision.indexRevision, 1);
  deq(plain(snap), {snap:true});
  await run("Go to call site");
  deq(plain(h.calls.reads[0][0]), {path:"src/store.rs",range:{startLine:20,endLine:20,startByte:200,endByte:210}});
  await run("Go to definition");
  deq(plain(h.calls.reads[1][0]), {path:"src/rules.rs",range:{startLine:5,endLine:9}});
  await run("Add exception");
  deq(h.calls.drafts[0], [{type:"addExceptions",pairs:[{source:{path:"src/store.rs"},destination:{path:"src/rules.rs"}}],reason:""},
    "Approves all 10 references from src/store.rs to src/rules.rs, including future ones on this pair."]);
  assert.equal(h.focus.active, h.get("architecture-reason"));
});

test("deviations copy location writes, announces and falls back", async () => {
  const h = deviationsHarness();
  await h.dev.open(h.trigger);
  await h.expandAll();
  const line = h.treeRows().find(row => row.dataset.level === "3" && /Line 20,/.test(row.attrs["aria-label"]));
  await line.dispatch("contextmenu");
  const copy = () => h.menus.at(-1).actions.find(action => action.label === "Copy location").run();
  await copy();
  assert.equal(h.sandbox.copied, "src/store.rs:20");
  assert.equal(h.feedback(), "Copied src/store.rs:20");
  h.sandbox.navigator = {clipboard:{writeText:async () => { throw new Error("denied"); }}};
  await copy();
  assert.equal(h.feedback(), "Copy failed. Location: src/store.rs:20");
  const span = descendants(h.get("architecture-deviations-notices")).find(item => item.tagName === "span");
  assert.equal(span.attrs.style, undefined, "selectable fallback text is a plain span");
  h.sandbox.navigator = {};
  h.sandbox.copied = null;
  await copy();
  assert.equal(h.feedback(), "Copy failed. Location: src/store.rs:20");
  assert.equal(h.sandbox.copied, null);
});

test("deviations line action errors stay in the Deviations notices", async () => {
  const h = deviationsHarness();
  await h.dev.open(h.trigger);
  await h.expandAll();
  const line = h.treeRows().find(row => row.dataset.level === "3" && /Line 20,/.test(row.attrs["aria-label"]));
  await line.dispatch("click");
  const panel = h.inspections.at(-1).target;
  const button = label => descendants(panel).find(item => item.tagName === "button" && item.textContent.startsWith(label));
  assert.ok(button("Open in Sequence").className.includes("deviation-primary"), "a call with a caller leads with Sequence");
  assert.match(text(panel), /Call · runs code in a higher layer/);
  assert.match(text(panel), /Semantic binding · run by enqueue/);
  assert.equal(h.inspections.at(-1).title.textContent, "src/store.rs:20");
  h.state.readError = new Error("source gone");
  await button("Go to call site").dispatch("click");
  assert.equal(h.feedback(), "source gone");
  h.state.readError = Object.assign(new Error("cancelled"), {name:"AbortError"});
  await button("Go to definition").dispatch("click");
  assert.equal(h.feedback(), "source gone", "an abort is not reported");
  h.state.readError = new Error("stale");
  h.state.stale = true;
  await button("Go to call site").dispatch("click");
  assert.equal(h.feedback(), "source gone", "an inactive guard reports nothing");
  const no = h.treeRows().find(row => row.dataset.level === "3" && /Line 10,/.test(row.attrs["aria-label"]));
  h.state.stale = false;
  await no.dispatch("click");
  const second = descendants(h.inspections.at(-1).target);
  assert.match(text(h.inspections.at(-1).target), /no measured caller: /);
  assert.ok(second.find(item => item.tagName === "button" && item.textContent === "Go to call site").className.includes("deviation-primary"));
  assert.match(text(h.inspections.at(-1).target), /Approves all 10 references/);
});

test("deviations hidden selection: notice, Clear filter, never auto-selected", async () => {
  const h = deviationsHarness();
  await h.dev.open(h.trigger);
  await h.expandAll();
  const type = h.treeRows().find(row => row.dataset.level === "3" && /Line 12,/.test(row.attrs["aria-label"]));
  await type.dispatch("click");
  assert.match(text(h.inspections.at(-1).target), /Type use · names a type from a higher layer/);
  const selections = h.inspections.length, overview = h.calls.overview, calls = h.get("architecture-deviations-calls");
  calls.checked = true;
  await calls.dispatch("change");
  assert.match(h.notices(), /Selected line is hidden by this filter\./);
  assert.equal(h.calls.overview, overview + 1);
  assert.equal(h.inspections.length, selections);
  assert.equal(h.dev.snapshot().selected, null);
  assert.equal(h.focus.active, calls, "focus stays on the changed control");
  assert.equal(h.get("architecture-deviations-summary").textContent, "2 files · 4 deviating calls");
  await h.get("architecture-deviations-clear").dispatch("click");
  assert.equal(h.notices().includes("hidden by this filter"), false);
  assert.equal(calls.checked, false);
  assert.equal(h.inspections.length, selections, "nothing is selected");
  assert.equal(h.treeRows().filter(row => row.attrs["aria-selected"] === "true").length, 0);
  const chip = descendants(h.get("architecture-deviations-edges")).find(item => item.dataset.key === "Storage→Peer");
  assert.match(chip.attrs["aria-label"], /at least 2 deviations \(lower bound\)/);
  assert.match(chip.textContent, /≥2 deviations/);
  await chip.dispatch("click");
  const pressed = descendants(h.get("architecture-deviations-edges")).find(item => item.dataset.key === "Storage→Peer");
  assert.equal(pressed.attrs["aria-pressed"], "true");
  assert.equal(chip.attrs["aria-pressed"], "false", "the stale node is replaced");
  assert.equal(h.treeRows().filter(row => row.dataset.level === "1").length, 1);
  calls.checked = true;
  await calls.dispatch("change");
  assert.match(h.get("architecture-deviations-state").textContent, /^No calls on this edge at .*Turn off Calls only to see type uses, imports and references\.$/);
});

test("deviations keyboard drives the tree with a roving tabindex", async () => {
  const h = deviationsHarness();
  await h.dev.open(h.trigger);
  const tree = h.get("architecture-deviations-tree"), press = (key, target) => tree.dispatch("keydown", {key,target:target || h.treeRows().find(row => row.attrs.tabindex === "0")});
  const active = () => h.treeRows().find(row => row.attrs.tabindex === "0");
  assert.match(active().attrs["aria-label"], /^src\/store\.rs/);
  await press("ArrowRight");
  assert.equal(h.byLabel("src/store.rs").attrs["aria-expanded"], "true");
  assert.match(active().attrs["aria-label"], /^src\/store\.rs/);
  await press("ArrowRight");
  assert.match(active().attrs["aria-label"], /^S::enqueue/);
  await press("Enter");
  assert.equal(active().attrs["aria-expanded"], "true");
  await press("ArrowDown");
  assert.equal(active().dataset.level, "3");
  const event = await press("Enter");
  assert.equal(event.prevented, true);
  assert.equal(active().attrs["aria-selected"], "true");
  assert.equal(h.inspections.length, 1);
  await press("ArrowLeft");
  assert.match(active().attrs["aria-label"], /^S::enqueue/);
  await press("ArrowLeft");
  assert.equal(active().attrs["aria-expanded"], "false");
  assert.equal(h.dev.snapshot().selected !== null, true, "collapsing a parent keeps the selection");
  await press("End");
  await press("Home");
  assert.match(active().attrs["aria-label"], /^src\/store\.rs/);
  const other = await press("x");
  assert.equal(other.prevented, undefined);
  const menu = h.treeRows().find(row => row.dataset.level === "3");
  assert.equal(menu, undefined, "collapsed lines are not rendered");
});

test("deviations snapshot and restore round trip; a missing line selects nothing", async () => {
  const first = deviationsHarness();
  assert.equal(first.dev.snapshot(), null);
  await first.dev.open(first.trigger);
  await first.expandAll();
  const line = first.treeRows().find(row => row.dataset.level === "3" && /Line 20,/.test(row.attrs["aria-label"]));
  await line.dispatch("click");
  const saved = plain(first.dev.snapshot());
  assert.equal(saved.chip, null);
  assert.equal(saved.callsOnly, false);
  assert.equal(saved.expanded.length, 7, "every file and function was opened");
  assert.equal(typeof saved.selected, "string");

  const second = deviationsHarness();
  await second.dev.restore(saved, () => true);
  assert.equal(second.dev.isOpen(), true);
  const restored = second.treeRows().find(row => row.attrs["aria-selected"] === "true");
  assert.match(restored.attrs["aria-label"], /^Line 20,/);
  assert.equal(restored.scrolled, true);
  assert.equal(second.focus.active, restored);
  assert.equal(second.inspections.length, 1);
  assert.equal(second.inspections[0].record.evidence.range.start, 200);
  deq(plain(second.dev.snapshot()), saved);

  const third = deviationsHarness();
  await third.dev.restore({...saved,selected:JSON.stringify(["gone"])}, () => true);
  assert.match(third.notices(), /The selected line is no longer listed\./);
  assert.equal(third.inspections.length, 0);
  assert.equal(third.treeRows().filter(row => row.attrs["aria-selected"] === "true").length, 0);
  assert.equal(third.calls.overview, 1);

  const stale = deviationsHarness();
  await stale.dev.restore(saved, () => false);
  assert.equal(stale.dev.isOpen(), false);
  assert.equal(stale.calls.details.length, 0);
});
