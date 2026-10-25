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
  const pair = {source:a,target:b,count:7,sites:[site],omittedSites:6,mapping:"structural"};
  return {...pin,state:"ready",rulesFile:{path:".trellis/rules.toml",source:"checkout-config",contentHash:hash},
    aggregate,role,metricsVersion:"metrics:v1",evidenceTier:"syntax",semanticComplete:false,
    layers:[{name:"UI",order:1,shared:false},{name:"Storage",order:2,shared:false}],
    modules:[{identity:a,layer:"UI",physicalLines:4,declarationCount:2,roleCounts:{production:2,test:1},metrics:{cognitive:3,cyclomatic:4,maintainabilityIndex:80,halstead:{volume:12},fanIn:{value:null,unavailable:"not-measured"}},revisionId:"revision-a",contentHash:"hash-a",provenanceId:"native"},
      {identity:b,layer:"Storage",physicalLines:8,declarationCount:0,roleCounts:{production:0,test:2},metrics:{cognitive:null,cyclomatic:null},revisionId:"revision-b",contentHash:"hash-b",provenanceId:"native"}],
    edges:[{sourceLayer:"UI",destinationLayer:"Storage",role:"production",count:7,status:"conforms",excepted:false,exceptionReason:null,pairs:[pair]},
      {sourceLayer:"Storage",destinationLayer:"UI",role:"test",count:2,status:"deviates",excepted:false,exceptionReason:null,pairs:[]}],
    references:{production:[pair],tests:[],unresolved:[],counts:{mappedSites:9,unresolvedSites:0},limitations:["Structural mapping is not binding."]},rules:{instances:[],testOnly:[]}};
}
function harness() {
  const elements = new Map(), calls = [], renders = [], reads = [], conflicts = [];
  let context = {rootKey:"checkout-one",session:1,pin}, response = async () => ({payload:fixture(),catchingUp:false});
  const document = {getElementById:id => {if (!elements.has(id)) elements.set(id, node(id)); return elements.get(id);},createElement:tag => node(tag)};
  function node(tagName) {
    return {tagName,textContent:"",children:[],dataset:{},attrs:{},listeners:{},hidden:false,value:"",checked:false,
      append(...items) {this.children.push(...items);},replaceChildren(...items) {this.children = [...items];this.textContent="";},
      setAttribute(key,value) {this.attrs[key]=String(value);},
      addEventListener(name,fn) {(this.listeners[name] ||= []).push(fn);},
      async fire(name) {for (const fn of this.listeners[name] || []) await fn({target:this});}};
  }
  const get = document.getElementById;
  let renderer = async (_container, view, options) => {renders.push({view,options}); return {status:"ready"};};
  const window = {TrellisArchitecturePele:{render:(...args) => renderer(...args)},TrellisShell:{
    clearArchitectureInspector:() => {get("architecture-inspector").hidden=true;get("architecture-inspector-content").replaceChildren();},
    showArchitectureInspector:() => {get("architecture-inspector").hidden=false;},
  }};
  vm.runInNewContext(pinSource + "\n" + source, {window,document,URLSearchParams,console});
  const controller = window.TrellisArchitecture.create({request:(url,current) => {calls.push({url,current});return response(url);},context:() => context,
    readSource:(site,revision,current) => reads.push({site,revision,current}),onConflict:() => conflicts.push(true)});
  const button = label => {const found = descendants(get("architecture-inspector-content")).find(item => item.tagName === "button" && item.textContent === label);assert.ok(found, `Missing ${label}`);return found;};
  return {window,controller,calls,renders,reads,conflicts,get,button,setContext:value => {context=value;},setResponse:fn => {response=fn;},setRenderer:fn => {renderer=fn;}};
}

test("init is inert; one owned pin/digest supplies metric values and explicit sampled source actions", async () => {
  const h = harness();assert.equal(h.calls.length,0);await h.controller.refresh();
  assert.equal(h.calls.length,1);const query = new URL(h.calls[0].url,"http://local").searchParams;
  assert.equal(query.get("role"),"production");assert.equal(h.get("architecture-metric-tests").checked,false);
  assert.equal(query.get("aggregate"),"max");assert.equal(query.get("indexGeneration"),pin.indexGeneration);assert.equal(query.get("indexRevision"),"1");
  const {view,options} = h.renders[0];assert.equal(options.moduleValue(view.modules[0]),3);assert.equal(options.moduleValue(view.modules[1]),null);
  assert.match(options.layerLabel(view.layers[0]),/Max module cognitive \(max\)\n3/);
  assert.equal(h.reads.length,0);options.onSelectModule(view.modules[0]);
  assert.match(text(h.get("architecture-inspector-content")),/4 physical file lines · 2 production callables/);
  assert.match(text(h.get("architecture-inspector-content")),/not-measured/);
  assert.match(text(h.get("architecture-inspector-content")),/6 omitted sites/);
  await h.button("Read indexed file source").fire("click");assert.equal(h.reads.length,1);assert.equal(h.reads[0].site.path,"src/a.rs");
  assert.equal(h.reads[0].revision.indexGeneration,pin.indexGeneration);
  h.controller.clearInspection();assert.equal(h.reads[0].current(),false);
});

test("test toggle and metric recolor only owned data; aggregate reloads the same pin", async () => {
  const h=harness();await h.controller.refresh();const initial=h.get("architecture-state").textContent;
  h.get("architecture-tests").checked=true;await h.get("architecture-tests").fire("change");
  assert.equal(h.calls.length,1);assert.equal(h.renders.at(-1).options.showTests,true);assert.match(h.get("architecture-state").textContent,/7 production \/ 2 test-only/);
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
  assert.match(h.get("architecture-state").textContent, /tests shown.*all indexed callables/);
  const current = h.renders.at(-1); current.options.onSelectModule(current.view.modules[0]);
  assert.match(text(h.get("architecture-inspector-content")), /metrics:v1 · all indexed callables/);
  h.setResponse(async () => ({payload:fixture()})); h.get("architecture-metric-tests").checked = false;
  await h.get("architecture-metric-tests").fire("change");
  assert.match(h.calls.at(-1).url, /role=production/); assert.equal(h.renders.at(-1).options.showTests, true);
});

test("same-pin role changes fence both late request outcomes and async Pele completion", async () => {
  for (const reject of [false, true]) {
    const h = harness(), pending = deferred(); h.setResponse(() => pending.promise);
    const old = h.controller.refresh();
    h.setResponse(async () => ({payload:fixture("max", "rules-one", "all")}));
    h.get("architecture-metric-tests").checked = true; await h.get("architecture-metric-tests").fire("change");
    const state = h.get("architecture-state").textContent;
    if (reject) pending.reject(Object.assign(new Error("old role conflict"), {status:409, code:"revision_conflict"}));
    else pending.resolve({payload:fixture()});
    await old;
    assert.equal(h.renders.length, 1); assert.equal(h.get("architecture-state").textContent, state);
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
  const state = h.get("architecture-state").textContent;
  assert.equal(oldOptions.isCurrent(), false);
  renderer.resolve({status:"error", reason:"late production render"}); await oldRender;
  assert.equal(h.get("architecture-state").textContent, state);
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
