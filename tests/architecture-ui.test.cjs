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
  const controller = window.TrellisArchitecture.create({request:(url,current,options) => {calls.push({url,current,options});return response(url,options);},context:() => context,
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


test("external package evidence and deny results display without fabricated native metrics or source", async () => {
  const h = harness(), payload = fixture();
  const external = {kind:"externalPackage",sourceSetId:"workspace",language:"rust",package:"legacy-db",standardLibrary:false};
  const descriptor = {identity:external,ecosystem:"cargo",evidence:[{
    kind:"manifest",sourceSetId:"workspace",nativeRevisionId:"native-revision",dependencyHash:"dependencies-hash",
    path:"Cargo.toml",contentHash:"manifest-hash",importRoot:"legacy_db",section:"dependencies",key:"legacy-db",versionConstraint:"1",
  }]};
  const site = {role:"production",relationKind:"import",spelling:"legacy_db::*",lines:{start:2,end:2},range:{start:10,end:22},ownerSyntaxId:"owner",callId:null};
  const pair = {source:payload.modules[0].identity,target:external,count:1,sites:[site],omittedSites:0,mapping:"structural"};
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
  assert.match(text(h.get("architecture-notice-content")), /External policy results.*\$external.*legacy-db.*deviates/s);
  assert.match(text(h.get("architecture-notice-content")), /External packages and captured evidence.*Cargo\.toml.*manifest-hash/s);
  const layer = view.layers.find(layer => layer.name === "External");
  assert.match(options.layerLabel(layer), /0 files · 1 external packages/);
  options.onSelectLayer(layer);
  const inspector = text(h.get("architecture-inspector-content"));
  assert.match(inspector, /External packages \(no native source or metrics\)/);
  assert.match(inspector, /src\/a\.rs → legacy-db \(rust package\)/);
  assert.match(inspector, /legacy-db.*manifest-hash/s);
  assert.equal(descendants(h.get("architecture-inspector-content")).some(item => item.tagName === "button" && item.textContent === "Read indexed file source"), false);
  await h.button("Read indexed reference source").fire("click");
  assert.equal(h.reads.length, 1);
  assert.equal(h.reads[0].site.path, "src/a.rs", "only the genuine importing native file can be read");
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
const byLabel = (node,label) => descendants(node).find(item => item.attrs["aria-label"] === label);
async function selectTwo(h) {
  await h.get("architecture-review").fire("click");
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
  await h.get("architecture-review").fire("click");
  assert.equal(h.get("architecture-review-list").children.length,3,"two production deviations plus test-only row");
  const testCheckbox=descendants(h.get("architecture-review-list")).filter(item=>item.tagName==="input").at(-1);assert.equal(testCheckbox.disabled,true);
  h.get("architecture-review-role").value="test";await h.get("architecture-review-role").fire("change");assert.equal(h.get("architecture-review-list").children.length,1);
  assert.match(text(h.get("architecture-review-list")),/test-only rows cannot be waived/);assert.equal(h.calls.length,1);
  h.get("architecture-review-role").value="all";h.get("architecture-review-state").value="unknown";await h.get("architecture-review-state").fire("change");assert.match(text(h.get("architecture-review-list")),/Unknown destination/);
  h.renders[0].options.onSelectEdge(payload.edges[0]);assert.match(text(h.get("architecture-inspector-content")),/\$layers/);assert.equal(h.get("architecture-review-list").children.length,1);
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
  await h.controller.refresh();await h.get("architecture-review").fire("click");h.get("architecture-source-glob").value="src/**/a.*";h.get("architecture-destination-layer").value="Storage";
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
      return {payload:{...fixture("max",hash),state:"no_layers",layers:[],modules:[],edges:[],references:null,rules:null}};
    });
    await h.controller.refresh();assert.equal(h.renders.length,0);assert.match(h.get("architecture-state").textContent,/Existing authored exceptions/);
    await h.get("architecture-color").fire("change");assert.equal(h.renders.length,0);
    await h.get("architecture-authored").fire("click");assert.match(text(h.get("architecture-exceptions")),/orphan.rs/);
    const entry=h.get("architecture-exceptions").children[0];await descendants(entry).find(item=>item.textContent===mode).fire("click");
    if(mode==="Edit reason") h.get("architecture-reason").value="Revised orphan reason";
    await h.get("architecture-preview").fire("click");const body=h.calls.at(-1).options.body;
    assert.equal(body.baseHash,"rules-one");assert.equal(body.operations[0].index,2);assert.equal(body.operations[0].type,mode==="Edit reason" ? "editException" : "removeException");
    await h.get("architecture-confirm").fire("click");assert.match(h.get("architecture-state").textContent,/Rules committed/);assert.equal(h.renders.length,0);
    await h.get("architecture-unassigned").fire("click");assert.match(text(h.get("architecture-review-list")),/File assignment inventory is unavailable/);
  }
});
