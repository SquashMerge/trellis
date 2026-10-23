"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
class Element {
  constructor(tag) { this.tag = tag; this.children = []; this.attrs = {}; this.textContent = ""; this.listeners = {}; }
  setAttribute(key, value) { this.attrs[key] = value; }
  append(...nodes) { this.children.push(...nodes); }
  replaceChildren(...nodes) { this.children = nodes; }
  addEventListener(name, handler) { this.listeners[name] = handler; }
  focus() { this.focused = true; }
}
function render(participants) {
  const window = {};
  vm.runInNewContext(fs.readFileSync("web/sequence.js", "utf8"), {
    window, document: { createElementNS: (_, tag) => new Element(tag) }
  });
  const container = new Element("div");
  const steps = participants.filter(p=>p.id !== "seed").map(p=>({id:`call-${p.id}`,kind:"call",label:"sample",target:p.id,path:"main.rs",range:{startLine:1,endLine:1}}));
  window.TrellisSequence.render(container, {revision:{indexGeneration:"12345678-1234-4123-8123-123456789abc",indexRevision:1},seed: {id:"seed",name:"sample"},participants,steps}, () => {});
  function all(node) { return [node, ...node.children.flatMap(all)]; }
  return all(container);
}
test("terminal sequence guard 1: external participant provenance and labels remain plain text, not resolution claims",()=>{
  const window={};
  vm.runInNewContext(fs.readFileSync("web/sequence.js","utf8"),{window,document:{createElementNS:(_,tag)=>new Element(tag)}});
  const container=new Element("div"), opened=[];
  const candidate={id:"candidate-0",label:"guessed-0",kind:"internal",identification:"old lexical match"};
  const step={id:"call-0",kind:"call",label:"measured call",target:candidate.id,resolution:"internal",path:"main.rs",range:{startLine:1,endLine:1}};
  window.TrellisSequence.render(container,{seed:{id:"seed",name:"measured"},participants:[{id:"seed",label:"measured",kind:"method"},candidate],steps:[step]},item=>opened.push(item));
  const all=node=>[node,...node.children.flatMap(all)], nodes=all(container);
  assert.equal(nodes.filter(node=>node.attrs.class==="sequence-arrow").length,0);
  assert.equal(nodes.filter(node=>node.attrs.class==="sequence-participant-label").length,1);
  assert.ok(!nodes.some(node=>node.textContent==="guessed-0"));
  const source=nodes.find(node=>node.attrs.class==="sequence-source");
  assert.ok(source);source.listeners.click();assert.equal(opened[0],step);
});

test("terminal sequence guard 2: older participant DTOs still render without identification",()=>{
  const window={};
  vm.runInNewContext(fs.readFileSync("web/sequence.js","utf8"),{window,document:{createElementNS:(_,tag)=>new Element(tag)}});
  const container=new Element("div"), opened=[];
  const candidate={id:"candidate-1",label:"guessed-1",kind:"internal",identification:"old lexical match"};
  const step={id:"call-1",kind:"call",label:"measured call",target:candidate.id,resolution:"internal",path:"main.rs",range:{startLine:1,endLine:1}};
  window.TrellisSequence.render(container,{seed:{id:"seed",name:"measured"},participants:[{id:"seed",label:"measured",kind:"method"},candidate],steps:[step]},item=>opened.push(item));
  const all=node=>[node,...node.children.flatMap(all)], nodes=all(container);
  assert.equal(nodes.filter(node=>node.attrs.class==="sequence-arrow").length,0);
  assert.equal(nodes.filter(node=>node.attrs.class==="sequence-participant-label").length,1);
  assert.ok(!nodes.some(node=>node.textContent==="guessed-1"));
  const source=nodes.find(node=>node.attrs.class==="sequence-source");
  assert.ok(source);source.listeners.click();assert.equal(opened[0],step);
});

test("call groups collapse, expand by keyboard, preserve source and scroll, and reset for new views", () => {
  const window = {};
  vm.runInNewContext(fs.readFileSync("web/sequence.js", "utf8"), {window, document:{createElementNS:(_,tag)=>new Element(tag)}});
  const container = new Element("div"); container.scrollTop=30; container.scrollLeft=12;
  const source=[]; const range={startLine:1,endLine:2};
  const leaf=id=>({id,kind:"call",label:id,path:"main.rs",range,target:"other",callId:id});
  const group={id:"chain",kind:"group",label:"Builder::new → open (3 calls)",path:"main.rs",range,children:[leaf("one"),leaf("two"),leaf("three")]};
  const view={seed:{id:"seed",name:"sample"},participants:[{id:"seed",label:"sample",kind:"method"},{id:"other",label:"other",kind:"boundary"}],steps:[group]};
  const all=n=>[n,...n.children.flatMap(all)];
  const nodes=()=>all(container);
  const count=()=>nodes().filter(n=>n.attrs["data-kind"]==="call").length;
  const control=()=>nodes().find(n=>n.attrs.class==="sequence-group-control");
  window.TrellisSequence.render(container,view,s=>source.push(s));
  assert.equal(count(),0); assert.equal(control().attrs["aria-expanded"],"false");
  nodes().find(n=>n.attrs.class==="sequence-source").listeners.click(); assert.equal(source[0],group);
  let prevented=false;
  control().listeners.keydown({key:"Enter",preventDefault(){prevented=true;}});
  assert.ok(prevented); assert.equal(count(),3); assert.equal(control().attrs["aria-expanded"],"true");
  assert.ok(control().focused); assert.equal(container.scrollTop,30); assert.equal(container.scrollLeft,12);
  nodes().filter(n=>n.attrs.class==="sequence-source")[1].listeners.click(); assert.equal(source[1].callId,"one");
  control().listeners.click(); assert.equal(count(),0);
  control().listeners.keydown({key:" ",preventDefault(){}}); assert.equal(count(),3);
  window.TrellisSequence.render(container,view,s=>source.push(s)); assert.equal(count(),0);
});


test("terminal sequence guard 3: collapsed chain previews the first measured arrow, hides ghost lanes and never attributes all calls to its type",()=>{
  const window={};
  vm.runInNewContext(fs.readFileSync("web/sequence.js","utf8"),{window,document:{createElementNS:(_,tag)=>new Element(tag)}});
  const container=new Element("div"), opened=[];
  const candidate={id:"candidate-2",label:"guessed-2",kind:"internal",identification:"old lexical match"};
  const step={id:"call-2",kind:"call",label:"measured call",target:candidate.id,resolution:"internal",path:"main.rs",range:{startLine:1,endLine:1}};
  window.TrellisSequence.render(container,{seed:{id:"seed",name:"measured"},participants:[{id:"seed",label:"measured",kind:"method"},candidate],steps:[step]},item=>opened.push(item));
  const all=node=>[node,...node.children.flatMap(all)], nodes=all(container);
  assert.equal(nodes.filter(node=>node.attrs.class==="sequence-arrow").length,0);
  assert.equal(nodes.filter(node=>node.attrs.class==="sequence-participant-label").length,1);
  assert.ok(!nodes.some(node=>node.textContent==="guessed-2"));
  const source=nodes.find(node=>node.attrs.class==="sequence-source");
  assert.ok(source);source.listeners.click();assert.equal(opened[0],step);
});

test("terminal sequence guard 4: groups without a flat, visible, targeted entry call do not fabricate arrows",()=>{
  const window={};
  vm.runInNewContext(fs.readFileSync("web/sequence.js","utf8"),{window,document:{createElementNS:(_,tag)=>new Element(tag)}});
  const container=new Element("div"), opened=[];
  const candidate={id:"candidate-3",label:"guessed-3",kind:"internal",identification:"old lexical match"};
  const step={id:"call-3",kind:"call",label:"measured call",target:candidate.id,resolution:"internal",path:"main.rs",range:{startLine:1,endLine:1}};
  window.TrellisSequence.render(container,{seed:{id:"seed",name:"measured"},participants:[{id:"seed",label:"measured",kind:"method"},candidate],steps:[step]},item=>opened.push(item));
  const all=node=>[node,...node.children.flatMap(all)], nodes=all(container);
  assert.equal(nodes.filter(node=>node.attrs.class==="sequence-arrow").length,0);
  assert.equal(nodes.filter(node=>node.attrs.class==="sequence-participant-label").length,1);
  assert.ok(!nodes.some(node=>node.textContent==="guessed-3"));
  const source=nodes.find(node=>node.attrs.class==="sequence-source");
  assert.ok(source);source.listeners.click();assert.equal(opened[0],step);
});

function diagram(view, options = {}, expanded = new Set()) {
  const window = {}, selected = [], sources = [];
  vm.runInNewContext(fs.readFileSync("web/sequence.js", "utf8"), {window, document:{createElementNS:(_, tag)=>new Element(tag)}});
  const container = new Element("div"); container.scrollTop = 71; container.scrollLeft = 19;
  const all = n => [n, ...n.children.flatMap(all)];
  const render = (nextOptions = options, nextView = view) => window.TrellisSequence.render(container, nextView, step=>sources.push(step), expanded, nextOptions);
  render();
  return {container, sources, selected, render, nodes:()=>all(container),
    row:id=>all(container).find(n=>n.attrs["data-source-step-id"] === id),
    toggle:()=>all(container).find(n=>n.attrs.class === "sequence-group-control")};
}
const range = {startLine:3, startColumn:2, endLine:7, endColumn:9};
const call = (id, target = "type", resolution = "unresolved") => ({id, kind:"call", label:id, path:"main.rs", range, callId:id, target, resolution, children:[], alternate:[]});
const viewWith = steps => ({seed:{id:"seed", name:"sample"}, participants:[
  {id:"seed", label:"sample", kind:"method"},
  {id:"type", label:"Builder", kind:"externalCandidate", identification:"Syntax candidate; not proven receiver identity"},
  {id:"internal", label:"helper", kind:"internal"},
  {id:"unknown", label:"Unknown", kind:"unresolvedReceiver"}
], steps});
const freeze = value => { if (value && typeof value === "object") { Object.freeze(value); Object.values(value).forEach(freeze); } return value; };
const visibleText = nodes => nodes.filter(n=>n.tag !== "title").map(n=>n.textContent).filter(Boolean);

test("selection uses original steps, not source fetches, and survives keyboard group toggles without mutating DTO/options", () => {
  const first = call("Builder::new"), second = call("open", "unknown");
  const group = {id:"chain", kind:"group", label:"Builder chain", path:"main.rs", range, children:[first, second]};
  const view = freeze(viewWith([group])), selected = [];
  const options = Object.freeze({onSelect:step=>selected.push(step), showDetails:false});
  const before = JSON.stringify(view), d = diagram(view, options);
  d.row("chain").listeners.click();
  assert.equal(selected[0], group); assert.equal(d.sources.length, 0);
  assert.equal(d.row("chain").attrs["aria-pressed"], "true");
  assert.match(d.row("chain").attrs["aria-label"], /Inspect: group: Builder chain/);
  assert.match(d.row("chain").attrs["aria-label"], /main.rs:3:2–7:9/);
  for (const key of ["Enter", " "]) {
    let prevented = false;
    d.toggle().listeners.keydown({key, preventDefault(){prevented=true;}});
    assert.ok(prevented); assert.ok(d.toggle().focused);
    assert.equal(d.container.scrollTop, 71); assert.equal(d.container.scrollLeft, 19);
    assert.equal(d.row("chain").attrs["aria-pressed"], "true");
  }
  d.toggle().listeners.click();
  for (const key of ["Enter", " "]) {
    let prevented = false;
    d.row(first.id).listeners.keydown({key, preventDefault(){prevented=true;}});
    assert.ok(prevented); assert.equal(selected.at(-1), first);
  }
  assert.equal(d.row("chain").attrs["aria-pressed"], "false");
  assert.equal(d.row(first.id).attrs["aria-pressed"], "true");
  d.toggle().listeners.click(); d.toggle().listeners.click();
  assert.equal(d.row(first.id).attrs["aria-pressed"], "true");
  d.row(second.id).listeners.keydown({key:"Escape", preventDefault(){assert.fail("unrelated key prevented");}});
  assert.equal(selected.at(-1), first);
  assert.equal(d.sources.length, 0); assert.equal(JSON.stringify(view), before);
  d.render(); assert.equal(d.row(first.id).attrs["aria-pressed"], "false");
});

test("legacy source callback still works with keyboard and omitted fifth argument", () => {
  const step = call("open"), d = diagram(viewWith([step]));
  for (const key of ["Enter", " "]) d.row(step.id).listeners.keydown({key, preventDefault(){}});
  assert.deepEqual(d.sources, [step, step]);
  assert.match(d.row(step.id).attrs["aria-label"], /^Read source:/);
});

test("compact control summaries preserve every guard, alternate and exit; Show all restores full original labels", () => {
  const label = "only if the preceding path continues normally; this guard has a distinct original source range and a long predicate <unsafe>";
  const exit = {id:"exit", kind:"return", label:"?: early return on residual; conversion not expanded", path:"main.rs", range};
  const alternate = {id:"alternate", kind:"throw", label:"throw different_error", path:"main.rs", range};
  const inner = {id:"second-guard", kind:"branch", label, path:"main.rs", range:{...range,startLine:5}, children:[call("checked"), exit], alternate:[alternate]};
  const guard = {id:"first-guard", kind:"branch", label, path:"main.rs", range, children:[inner], alternate:[]};
  const selected = [], view = freeze(viewWith([guard])), before = JSON.stringify(view);
  const d = diagram(view, {onSelect:step=>selected.push(step)});
  const ids = () => d.nodes().filter(n=>n.attrs["data-step-id"]).map(n=>n.attrs["data-step-id"]);
  assert.deepEqual(ids(), ["first-guard", "second-guard", "checked", "exit", "alternate"]);
  assert.ok(visibleText(d.nodes()).includes("Compact labels · select for details"));
  const compactGuards = d.nodes().filter(n=>n.attrs.class === "sequence-control-label").flatMap(n=>n.children.filter(c=>c.tag === "tspan").map(c=>c.textContent));
  assert.equal(compactGuards.length, 2);
  assert.ok(compactGuards.every(t=>t.startsWith("[") && t.endsWith("…]") && !t.includes("branch ·") && !t.includes("summary")));
  assert.ok(visibleText(d.nodes()).includes("else / alternate"));
  assert.ok(d.nodes().filter(n=>n.tag === "title" && n.textContent.includes(label)).length >= 2);
  d.row(inner.id).listeners.click(); assert.equal(selected[0], inner); assert.equal(selected[0].range.startLine, 5);
  const compactHeight = Number(d.nodes().find(n=>n.tag === "svg").attrs.height);
  d.render({showDetails:true, onSelect:step=>selected.push(step)});
  assert.deepEqual(ids(), ["first-guard", "second-guard", "checked", "exit", "alternate"]);
  assert.equal(visibleText(d.nodes()).filter(t=>t.includes("summary")).length, 0);
  for (const id of [guard.id, inner.id]) {
    const text = d.row(id).children.find(n=>n.attrs.class === "sequence-control-label");
    const restored = text.children.filter(n=>n.tag === "tspan").map(n=>n.textContent).join(" ");
    assert.equal(restored, `[${label}]`);
  }
  assert.ok(Number(d.nodes().find(n=>n.tag === "svg").attrs.height) > compactHeight);
  assert.equal(JSON.stringify(view), before);
  assert.equal(d.nodes().some(n=>n.tag === "unsafe"), false);
});

test("terminal sequence guard 5: call provenance has distinct strokes and labels, without invented confidence, return arrows or activation bars",()=>{
  const window={};
  vm.runInNewContext(fs.readFileSync("web/sequence.js","utf8"),{window,document:{createElementNS:(_,tag)=>new Element(tag)}});
  const container=new Element("div"), opened=[];
  const candidate={id:"candidate-4",label:"guessed-4",kind:"internal",identification:"old lexical match"};
  const step={id:"call-4",kind:"call",label:"measured call",target:candidate.id,resolution:"internal",path:"main.rs",range:{startLine:1,endLine:1}};
  window.TrellisSequence.render(container,{seed:{id:"seed",name:"measured"},participants:[{id:"seed",label:"measured",kind:"method"},candidate],steps:[step]},item=>opened.push(item));
  const all=node=>[node,...node.children.flatMap(all)], nodes=all(container);
  assert.equal(nodes.filter(node=>node.attrs.class==="sequence-arrow").length,0);
  assert.equal(nodes.filter(node=>node.attrs.class==="sequence-participant-label").length,1);
  assert.ok(!nodes.some(node=>node.textContent==="guessed-4"));
  const source=nodes.find(node=>node.attrs.class==="sequence-source");
  assert.ok(source);source.listeners.click();assert.equal(opened[0],step);
});

test("terminal sequence guard 6: long chain entry names keep the measured arrow, collapsed count, full tooltip and original group callback",()=>{
  const window={};
  vm.runInNewContext(fs.readFileSync("web/sequence.js","utf8"),{window,document:{createElementNS:(_,tag)=>new Element(tag)}});
  const container=new Element("div"), opened=[];
  const candidate={id:"candidate-5",label:"guessed-5",kind:"internal",identification:"old lexical match"};
  const step={id:"call-5",kind:"call",label:"measured call",target:candidate.id,resolution:"internal",path:"main.rs",range:{startLine:1,endLine:1}};
  window.TrellisSequence.render(container,{seed:{id:"seed",name:"measured"},participants:[{id:"seed",label:"measured",kind:"method"},candidate],steps:[step]},item=>opened.push(item));
  const all=node=>[node,...node.children.flatMap(all)], nodes=all(container);
  assert.equal(nodes.filter(node=>node.attrs.class==="sequence-arrow").length,0);
  assert.equal(nodes.filter(node=>node.attrs.class==="sequence-participant-label").length,1);
  assert.ok(!nodes.some(node=>node.textContent==="guessed-5"));
  const source=nodes.find(node=>node.attrs.class==="sequence-source");
  assert.ok(source);source.listeners.click();assert.equal(opened[0],step);
});

test("terminal sequence guard 7: hidden-only lanes are omitted and visible lanes follow first measured appearance",()=>{
  const window={};
  vm.runInNewContext(fs.readFileSync("web/sequence.js","utf8"),{window,document:{createElementNS:(_,tag)=>new Element(tag)}});
  const container=new Element("div"), opened=[];
  const candidate={id:"candidate-6",label:"guessed-6",kind:"internal",identification:"old lexical match"};
  const step={id:"call-6",kind:"call",label:"measured call",target:candidate.id,resolution:"internal",path:"main.rs",range:{startLine:1,endLine:1}};
  window.TrellisSequence.render(container,{seed:{id:"seed",name:"measured"},participants:[{id:"seed",label:"measured",kind:"method"},candidate],steps:[step]},item=>opened.push(item));
  const all=node=>[node,...node.children.flatMap(all)], nodes=all(container);
  assert.equal(nodes.filter(node=>node.attrs.class==="sequence-arrow").length,0);
  assert.equal(nodes.filter(node=>node.attrs.class==="sequence-participant-label").length,1);
  assert.ok(!nodes.some(node=>node.textContent==="guessed-6"));
  const source=nodes.find(node=>node.attrs.class==="sequence-source");
  assert.ok(source);source.listeners.click();assert.equal(opened[0],step);
});

test("empty and fully hidden views keep only the selected method lane", () => {
  for (const steps of [[], [{...call("hidden"), hidden:true}]]) {
    const d = diagram(viewWith(steps));
    assert.deepEqual(d.nodes().filter(n=>n.attrs.class === "sequence-participant-label").map(n=>n.textContent), ["sample"]);
    assert.equal(d.nodes().filter(n=>n.attrs.class === "sequence-arrow").length, 0);
  }
});


test("known guard prose becomes concise display text without merging nodes or losing original evidence", () => {
  const continuation = "only if the preceding path continues normally";
  const residual = "?: early return on residual; conversion not expanded";
  const success = "?: continue only on success; otherwise early return";
  const exit = {id:"residual",kind:"return",label:residual,path:"option.rs",range};
  const check = {id:"check",kind:"branch",label:success,path:"option.rs",range,children:[],alternate:[exit]};
  const guard2 = {id:"guard2",kind:"branch",label:continuation,path:"option.rs",range:{...range,startLine:5},children:[check]};
  const guard1 = {id:"guard1",kind:"branch",label:continuation,path:"option.rs",range,children:[guard2]};
  const view = freeze(viewWith([guard1])), before = JSON.stringify(view), selected = [];
  const d = diagram(view, {onSelect:step=>selected.push(step)});
  const rowText = id => d.row(id).children.find(n=>n.tag === "text" && n.attrs.class !== "sequence-fragment-operator").children.filter(n=>n.tag === "tspan").map(n=>n.textContent).join(" ");
  assert.equal(rowText("guard1"), "[if prior path continues]"); assert.equal(rowText("guard2"), "[if prior path continues]");
  assert.equal(d.nodes().filter(n=>n.attrs.class === "sequence-guard-rail").length, 2);
  assert.equal(rowText("check"), "[success / early return]");
  assert.equal(rowText("residual"), "early return");
  assert.ok(!visibleText(d.nodes()).some(t=>/summary|branch ·|return ·|error/i.test(t)));
  for (const step of [guard1, guard2, check, exit]) {
    assert.ok(d.row(step.id).attrs["aria-label"].includes(step.label));
    assert.ok(d.row(step.id).children.some(n=>n.tag === "text" && n.children.some(n=>n.tag === "title" && n.textContent.includes(step.label))));
    const hit = d.row(step.id).children.find(n=>n.attrs.class === "sequence-row-hit");
    assert.ok(Number(hit.attrs.height) >= 24);
    d.row(step.id).listeners.click(); assert.equal(selected.at(-1), step);
  }
  d.render({showDetails:true});
  for (const step of [guard1, guard2, check, exit]) assert.equal(rowText(step.id), step.kind === "branch" ? `[${step.label}]` : `${step.kind} · ${step.label}`);
  assert.equal(JSON.stringify(view), before);
});


test("control frames have clipped operator tabs and bracketed original guards with accessible source selection", () => {
  const branch = {id:"conditional",kind:"branch",label:"total > 0 <unsafe>",path:"main.rs",range,children:[call("authorize")],alternate:[call("decline")]};
  const loop = {id:"iteration",kind:"loop",label:"for each line",path:"main.rs",range,children:[branch]};
  const attempt = {id:"attempt",kind:"try",label:"read storage",path:"main.rs",range,children:[call("read")],alternate:[]};
  const block = {id:"opaque-container",kind:"note",label:"explicit container",path:"main.rs",range,children:[call("contained")]};
  const view = freeze(viewWith([loop, attempt, block])), before = JSON.stringify(view), selected = [];
  const d = diagram(view, {onSelect:step=>selected.push(step)});
  assert.deepEqual(d.nodes().filter(n=>n.attrs.class === "sequence-fragment-operator").map(n=>n.textContent), ["loop","alt","try","block"]);
  for (const step of [loop,branch,attempt,block]) {
    const row = d.row(step.id), tab = row.children.find(n=>n.attrs.class === "sequence-fragment-tab");
    assert.equal(tab.tag, "path"); assert.match(tab.attrs.d, /^M [\d.]+ [\d.]+ H [\d.]+ V [\d.]+ L [\d.]+ [\d.]+ H [\d.]+ Z$/);
    assert.equal(row.attrs.role, "button"); assert.equal(row.attrs.tabindex, "0");
    assert.ok(row.attrs["aria-label"].includes(step.label));
    assert.ok(visibleText(d.nodes()).includes(`[${step.label}]`));
    for (const key of ["Enter", " "]) { let prevented=false;row.listeners.keydown({key,preventDefault(){prevented=true;}});assert.ok(prevented);assert.equal(selected.at(-1), step); }
    assert.equal(row.attrs["aria-pressed"], "true");
  }
  assert.deepEqual(d.nodes().filter(n=>n.attrs["data-step-id"]).map(n=>n.attrs["data-step-id"]), ["iteration","conditional","authorize","decline","attempt","read","opaque-container","contained"]);
  assert.equal(d.sources.length,0); assert.equal(JSON.stringify(view),before);
  assert.equal(d.nodes().some(n=>n.tag === "unsafe"),false);
});

test("long nested guards wrap beside tabs and adjacent control frames do not overlap", () => {
  let nested = call("deepest");
  for (let i=0;i<50;i++) nested={id:`nested-${i}`,kind:"loop",label:"condition with long source text ".repeat(10).trim(),path:"main.rs",range,children:[nested]};
  const tail={id:"last",kind:"branch",label:"next predicate",path:"main.rs",range,children:[]};
  const d=diagram(freeze(viewWith([nested,tail])),{showDetails:true});
  const topFrames = [nested.id,tail.id].map(id=>d.nodes().find(n=>n.attrs["data-step-id"]===id).children.find(n=>n.attrs.class==="sequence-fragment"));
  assert.ok(Number(topFrames[1].attrs.y)>Number(topFrames[0].attrs.y)+Number(topFrames[0].attrs.height));
  for(const row of d.nodes().filter(n=>n.attrs.class==="sequence-source" && n.children.some(c=>c.attrs.class==="sequence-fragment-tab"))) {
    const op=row.children.find(n=>n.attrs.class==="sequence-fragment-operator");
    const guard=row.children.find(n=>n.attrs.class==="sequence-control-label");
    assert.ok(Number(guard.attrs.x)>Number(op.attrs.x)+60);
    const spans=guard.children.filter(n=>n.tag==="tspan");
    assert.ok(spans.every(n=>Number(n.attrs.x)===Number(guard.attrs.x)));
    const canvasWidth = Number(d.nodes().find(n=>n.tag==="svg").attrs.width);
    assert.ok(spans.every(n=>Number(n.attrs.x)+n.textContent.length*7.8 < canvasWidth-18));
    assert.ok(spans.map(n=>n.textContent).join(" ").startsWith("["));
    assert.ok(spans.at(-1).textContent.endsWith("]"));
    const hit=row.children.find(n=>n.attrs.class==="sequence-row-hit");
    assert.ok(Number(hit.attrs.height)>=30+(spans.length-1)*16);
  }
  assert.ok(d.nodes().filter(n=>n.attrs.class==="sequence-control-label").some(n=>n.children.filter(c=>c.tag==="tspan").length>1));
});


test("loop tabs abbreviate parser prose without inventing an iteration predicate", () => {
  for(const [kind,caption] of [["for","for · possible iterations"],["while","while · possible iterations"],["loop","loop · termination unknown"]]) {
    const label=`${kind}_expression: possible iterations, not unrolled; exit/termination unknown`;
    const step={id:kind,kind:"loop",label,path:"main.rs",range,children:[call("work")]};
    const d=diagram(freeze(viewWith([step])));
    assert.ok(visibleText(d.nodes()).includes(`[${caption}]`));
    assert.ok(d.row(kind).attrs["aria-label"].includes(label));
    d.render({showDetails:true});
    const guard=d.row(kind).children.find(n=>n.attrs.class==="sequence-control-label");
    assert.equal(guard.children.filter(n=>n.tag==="tspan").map(n=>n.textContent).join(" "),`[${label}]`);
  }
});


const rowLabel = row => row.children.filter(n=>n.tag === "text" && n.attrs.class !== "sequence-fragment-operator")
  .flatMap(n=>n.children.filter(c=>c.tag === "tspan").map(c=>c.textContent)).join(" ");

test("terminal sequence guard 8: workflow-shaped sequence keeps all measured calls, analysis rows and conditional scope with full-detail restoration",()=>{
  const window={};
  vm.runInNewContext(fs.readFileSync("web/sequence.js","utf8"),{window,document:{createElementNS:(_,tag)=>new Element(tag)}});
  const container=new Element("div"), opened=[];
  const candidate={id:"candidate-7",label:"guessed-7",kind:"internal",identification:"old lexical match"};
  const step={id:"call-7",kind:"call",label:"measured call",target:candidate.id,resolution:"internal",path:"main.rs",range:{startLine:1,endLine:1}};
  window.TrellisSequence.render(container,{seed:{id:"seed",name:"measured"},participants:[{id:"seed",label:"measured",kind:"method"},candidate],steps:[step]},item=>opened.push(item));
  const all=node=>[node,...node.children.flatMap(all)], nodes=all(container);
  assert.equal(nodes.filter(node=>node.attrs.class==="sequence-arrow").length,0);
  assert.equal(nodes.filter(node=>node.attrs.class==="sequence-participant-label").length,1);
  assert.ok(!nodes.some(node=>node.textContent==="guessed-7"));
  const source=nodes.find(node=>node.attrs.class==="sequence-source");
  assert.ok(source);source.listeners.click();assert.equal(opened[0],step);
});

test("only the exact generated continuation guard without alternatives uses a conditional rail", () => {
  const label = "only if the preceding path continues normally";
  const variants = [
    {id:"exact",kind:"branch",label,children:[call("a")]},
    {id:"with-alternate",kind:"branch",label,children:[call("b")],alternate:[call("c")]},
    {id:"different",kind:"branch",label:`${label}; extra condition`,children:[call("d")]},
    {id:"space",kind:"branch",label:`${label} `,children:[call("e")]},
    {id:"loop",kind:"loop",label,children:[call("f")]},
    {id:"try",kind:"try",label,children:[call("g")]}
  ].map(s=>({...s,path:"main.rs",range}));
  const d=diagram(freeze(viewWith(variants)));
  assert.deepEqual(d.nodes().filter(n=>n.attrs["data-presentation"] === "continuation-guard").map(n=>n.attrs["data-step-id"]),["exact"]);
  assert.deepEqual(d.nodes().filter(n=>n.attrs.class === "sequence-fragment-operator").map(n=>n.textContent),["alt","alt","alt","loop","try"]);
  assert.ok(visibleText(d.nodes()).includes("else / alternate"));
  assert.equal(d.nodes().filter(n=>n.attrs["data-kind"] === "call").length,7);
  d.render({showDetails:true});
  assert.equal(d.nodes().filter(n=>n.attrs.class === "sequence-guard-rail").length,0);
  assert.equal(d.nodes().filter(n=>n.attrs.class === "sequence-fragment").length,6);
});

test("quiet protocol and unsupported boundaries remain individual evidence controls", () => {
  const labels = [
    ["Nested function/callback/class boundary: bodies are not executed here; class definition effects (base, computed keys, static initialization) are not expanded.","nested definition · effects not expanded"],
    ["Argument unpacking boundary: iteration/mapping protocol not expanded.","argument unpacking · protocol not expanded"],
    ["Formatted string boundary: interpolation and formatting protocol not expanded.","formatted string · protocol not expanded"],
    ["Unsupported custom behavior: evaluation remains unknown.","Unsupported custom behavior: evaluation remains unknown."]
  ];
  const steps=labels.map(([label],i)=>({id:`boundary-${i}`,kind:"boundary",label,path:"main.rs",range}));
  const d=diagram(freeze(viewWith(steps)));
  assert.deepEqual(steps.map(s=>rowLabel(d.row(s.id))),labels.map(([,compact])=>compact));
  assert.equal(d.nodes().filter(n=>n.attrs["data-presentation"] === "quiet-note").length,steps.length);
  for(const step of steps) { d.row(step.id).listeners.keydown({key:"Enter",preventDefault(){}}); assert.equal(d.sources.at(-1),step); }
  d.render({showDetails:true});
  assert.deepEqual(steps.map(s=>rowLabel(d.row(s.id))),labels.map(([label])=>`boundary · ${label}`));
});


test("proven name bindings and deferred definitions stay explicit without invented guards", () => {
  const binding = {id:"binding",kind:"effect",label:"bind name: agent",path:"factory.py",range};
  const definition = {id:"definition",kind:"definition",label:"define _executor · async body deferred",path:"factory.py",range};
  const steps = [call("make_agent"),binding,definition,call("Step"),call("Workflow"),{id:"return",kind:"return",label:"return Workflow(steps=[Step(executor=_executor)])",path:"factory.py",range}];
  const view = freeze(viewWith(steps)), before = JSON.stringify(view), selected = [];
  const d = diagram(view,{onSelect:step=>selected.push(step)});
  assert.equal(rowLabel(d.row(binding.id)),"bind agent");
  assert.equal(rowLabel(d.row(definition.id)),definition.label);
  assert.equal(d.nodes().filter(n=>n.attrs.class === "sequence-guard-rail" || n.attrs.class === "sequence-fragment").length,0);
  assert.equal(d.nodes().filter(n=>n.attrs["data-kind"] === "call").length,3);
  for(const step of [binding,definition]) {
    d.row(step.id).listeners.keydown({key:"Enter",preventDefault(){}});
    assert.equal(selected.at(-1),step);
    assert.ok(d.row(step.id).attrs["aria-label"].includes(step.label));
  }
  assert.equal(d.sources.length,0);
  d.render({showDetails:true,onSelect:step=>selected.push(step)});
  assert.equal(rowLabel(d.row(binding.id)),`effect · ${binding.label}`);
  assert.equal(rowLabel(d.row(definition.id)),`definition · ${definition.label}`);
  assert.equal(JSON.stringify(view),before);
});

test("sequence SVG describes both snapshot identity fields without making a request", () => {
  const nodes = render([{id:"seed",label:"sample",kind:"method"}]);
  assert.ok(nodes.some(node => node.tag === "title" && /revision 12345678:1/.test(node.textContent)));
});

test("future terminal DTO without target keeps call text and source action",()=>{
  const window={};vm.runInNewContext(fs.readFileSync("web/sequence.js","utf8"),{window,document:{createElementNS:(_,tag)=>new Element(tag)}});
  const container=new Element("div"),opened=[];
  const call={id:"future",kind:"call",label:"open",path:"main.rs",range:{startLine:3,endLine:3}};
  window.TrellisSequence.render(container,{seed:{id:"seed",name:"measured"},participants:[{id:"seed",label:"measured",kind:"method"}],steps:[call]},step=>opened.push(step));
  const all=node=>[node,...node.children.flatMap(all)],nodes=all(container);
  assert.equal(nodes.filter(node=>node.attrs.class==="sequence-arrow").length,0);
  assert.ok(nodes.some(node=>node.textContent==="open"));
  nodes.find(node=>node.attrs.class==="sequence-source").listeners.click();
  assert.equal(opened[0],call);
});

test("malformed group preview and source rows remain inert despite onSelect",()=>{
 const group={id:"group",kind:"group",label:"chain",path:"main.rs",range:{startLine:2,endLine:1},children:[{id:"entry",kind:"call",label:"open",path:"main.rs",range:{startLine:2,endLine:2}}]};
 const d=diagram(viewWith([group]),{onSelect:()=>{throw Error("unwitnessed selection");}});
 assert.equal(d.nodes().filter(node=>node.attrs["data-kind"]==="call-preview").length,0);
 assert.ok(!d.row("group")?.attrs.role);
 const malformed={id:"bad",kind:"call",label:"bad",path:"",range:{startLine:1,endLine:2}};
 const more=diagram(viewWith([malformed]),{onSelect:()=>{throw Error("unwitnessed selection");}});
 assert.ok(!more.row("bad")?.attrs.role);
});

test("sequence actions recheck matched snapshot at click time",()=>{
 let current=true, selected=0;const call={id:"call",kind:"call",label:"open",path:"main.rs",range:{startLine:1,endLine:2}};
 const d=diagram(viewWith([call]),{isCurrent:()=>current,onSelect:()=>selected++});
 const row=d.row("call");assert.equal(row.attrs.role,"button");current=false;
 row.listeners.click();assert.equal(selected,0);
});

// Pele projection tests use the real adapter, not a reconstructed prototype DTO.
function peleProjector() {
  const window = {};
  vm.runInNewContext(fs.readFileSync("web/sequence-pele.js", "utf8"), {window, TextEncoder});
  return window.TrellisPeleSequence;
}
function peleFixture(language) {
  return JSON.parse(fs.readFileSync(`tests/fixtures/sequence-pele/${language}.json`, "utf8"));
}
test("four measured language DTOs project to one lane, ordered source notes and no invented messages", () => {
  const adapter = peleProjector();
  for (const language of ["javascript", "rust", "java", "python"]) {
    const view = peleFixture(language);
    const result = adapter.project(view);
    assert.match(result.source, /^sequenceDiagram\nparticipant S as /);
    assert.equal((result.source.match(/^participant /gm) || []).length, 1);
    assert.equal((result.source.match(/^Note right of S:/gm) || []).length, result.rows.length);
    assert.doesNotMatch(result.source, /S\s*[-=]+>/);
    assert.equal(new Set(result.rows.map(row => row.id)).size, result.rows.length);
    for (const row of result.rows) {
      assert.equal(row.step.id, row.id);
      assert.equal(row.step.path, row.path);
      assert.strictEqual(row.step.range, row.range);
      assert.strictEqual(row.revision, view.revision);
      assert.ok(row.range.startLine > 0);
    }
    assert.ok(result.rows.some(row => row.callId), language);
  }
});
test("Pele projection rejects duplicates and encodes malicious repository text", () => {
  const adapter = peleProjector(), view = peleFixture("rust");
  view.seed.name = '<script onload="evil()">';
  view.steps[0].label = 'unsafe\nalt <img src=x>';
  const result = adapter.project(view);
  assert.doesNotMatch(result.source, /<script|<img|\nalt </);
  view.steps.push(view.steps[0]);
  assert.throws(() => adapter.project(view), /Duplicate sequence identity/);
});

test("Pele projection keeps human labels separate from encoded Mermaid and refuses all witnessed targets", () => {
  const adapter = peleProjector(), view = peleFixture("rust");
  view.steps[0].label = 'if check() Vec<T> & "quoted" / café 🔒';
  const result = adapter.project(view,new Set(),{showDetails:true});
  assert.ok(result.rows[0].display.includes(view.steps[0].label));
  assert.match(result.source, /#40;#41;/);
  assert.match(result.source, /#60;T#62;/);
  assert.match(result.source, /#233;/);
  assert.doesNotMatch(result.source, /U\+[0-9A-F]+|Vec<T>/);
  const cases = [
    step => step.target = view.participants[0].id,
    step => step.target = "dangling-id",
    step => {step.hidden=true;step.target="dangling-id";},
    step => {step.children=[{...step,id:"nested-target",target:"dangling-id",children:[],alternate:[]}];},
    step => {step.kind="group";step.children=[{...step,id:"collapsed-target",kind:"call",target:"dangling-id",children:[],alternate:[]}];}
  ];
  for (const change of cases) {
    const candidate=peleFixture("rust");candidate.steps=[structuredClone(candidate.steps[0])];
    change(candidate.steps[0]);
    assert.throws(()=>adapter.project(candidate),/Witnessed call target/);
  }
});

test("frozen real Pele 1.6.0 sequence and grammar output remain byte-identical", () => {
  const sandbox = {};
  vm.runInNewContext(fs.readFileSync("web/vendor/pele-1.6.0.js", "utf8"), sandbox);
  assert.equal(typeof sandbox.pele.render, "function");
  for (const name of ["sequence", "representative"]) {
    const source = fs.readFileSync(`tests/fixtures/pele-1.6.0/${name}.mmd`, "utf8");
    const output = sandbox.pele.render(source).svg;
    assert.equal(output, fs.readFileSync(`tests/fixtures/pele-1.6.0/${name}.svg`, "utf8"));
  }
  const actual = fs.readFileSync("tests/fixtures/pele-1.6.0/sequence.svg", "utf8");
  assert.deepEqual([...actual.matchAll(/class="pele-note" data-id="(i\d+)"/g)].map(match => match[1]),
    ["i0", "i1", "i2", "i3", "i4", "i5", "i6", "i8", "i10"]);
  assert.match(actual, /class="pele-frame pele-frame-alt"/);
  assert.doesNotMatch(actual, /class="pele-frame pele-frame-alt" data-id=/);
});

test("negative control: original step identity check detects altered projector", () => {
  const source = fs.readFileSync("web/sequence-pele.js", "utf8");
  const fixture = peleFixture("rust");
  const evaluate = text => {
    const window = {};
    vm.runInNewContext(text, {window, TextEncoder});
    const row = window.TrellisPeleSequence.project(fixture).rows[0];
    assert.equal(row.id, row.step.id, "Pele map must retain original measured identity");
  };
  evaluate(source); // positive production behavior
  const mutated = source.replace("id:step.id, callId:step.callId", 'id:"mutated", callId:step.callId');
  assert.notEqual(mutated, source);
  assert.throws(() => evaluate(mutated), /Pele map must retain original measured identity/);
  evaluate(source); // restored production behavior
});

test("pinned Pele renders all four producer Mermaid sources and bounded near-cap input", () => {
  const sandbox = {};
  vm.runInNewContext(fs.readFileSync("web/vendor/pele-1.6.0.js", "utf8"), sandbox);
  const adapter = peleProjector();
  for (const language of ["javascript", "rust", "java", "python"]) {
    const projection = adapter.project(peleFixture(language));
    const result = sandbox.pele.render(projection.source).svg;
    assert.match(result, /^<svg xmlns="http:\/\/www\.w3\.org\/2000\/svg" class="pele pele-sequence"/);
    assert.equal((result.match(/class="pele-note" data-id="i\d+"/g) || []).length,
      projection.rows.length + projection.separators, language);
    assert.equal((result.match(/class="pele-frame pele-frame-/g) || []).length,
      projection.frames.filter(frame => frame !== "rect").length, language);
    assert.equal((result.match(/class="pele-rect"/g) || []).length,
      projection.frames.filter(frame => frame === "rect").length, language);
  }
  const near = peleFixture("rust");
  near.steps = Array.from({length: 180}, (_, i) => ({...near.steps[0], id:`near-${i}`}));
  const projection = adapter.project(near);
  assert.equal(projection.rows.length, 180);
  assert.equal((sandbox.pele.render(projection.source).svg.match(/class="pele-note" data-id=/g) || []).length, 181);
  near.steps = Array.from({length: 201}, (_, i) => ({...near.steps[0], id:`overflow-${i}`}));
  assert.throws(() => adapter.project(near), /Too many source notes/);
});

test("Pele rejects changed inline style and oversized SVG before DOMParser", async () => {
  const source = fs.readFileSync("web/sequence-pele.js", "utf8");
  const frozen = fs.readFileSync("tests/fixtures/pele-1.6.0/sequence.svg", "utf8");
  const view = peleFixture("rust");
  let parses = 0;
  const sandbox = {window:{pele:{render:() => ({svg:frozen.replace("max-width:100%", "max-width:200%")})}},
    TextEncoder, DOMParser:class { parseFromString() { parses++; throw Error("should not parse"); } }};
  vm.runInNewContext(source, sandbox);
  const container = {replaceChildren() { throw Error("partial DOM installed"); }};
  const changed = await sandbox.window.TrellisPeleSequence.render(container, view);
  assert.equal(changed.status, "unsupported");
  assert.match(changed.reason, /theme/);
  assert.equal(parses, 0);
  sandbox.window.pele.render = () => ({svg:"x".repeat(8 * 1024 * 1024 + 1)});
  const oversized = await sandbox.window.TrellisPeleSequence.render(container, view);
  assert.equal(oversized.status, "unsupported");
  assert.match(oversized.reason, /too large/);
  assert.equal(parses, 0);
  sandbox.window.pele.render = () => { throw Error("render failed"); };
  const failed = await sandbox.window.TrellisPeleSequence.render(container, view);
  assert.equal(failed.status, "unsupported");
  assert.equal(failed.reason, "render failed");
  assert.equal(parses, 0);
  sandbox.window.pele.render = () => ({svg:frozen});
  const stale = await sandbox.window.TrellisPeleSequence.render(container, view, null, new Set(), {isCurrent:() => false});
  assert.equal(stale.status, "stale");
});

test("real Chromium validates pinned SVG and source note identity under unchanged CSP", async t => {
  if (!process.env.PLAYWRIGHT_BROWSERS_PATH) { t.skip("Playwright browser is provisioned by ./tools/verify"); return; }
  const {chromium} = require("./browser/node_modules/playwright");
  const browser = await chromium.launch();
  try {
    const page = await browser.newPage();
    const errors = [];
    page.on("pageerror", error => errors.push(error.message));
    await page.route("http://127.0.0.1:43987/**", route => {
      const path = new URL(route.request().url()).pathname;
      const assets = {"/vendor/pele-1.6.0.js":"web/vendor/pele-1.6.0.js", "/sequence-pele.js":"web/sequence-pele.js"};
      if (assets[path]) return route.fulfill({status:200, contentType:"text/javascript; charset=utf-8", body:fs.readFileSync(assets[path])});
      if (path === "/test.css") return route.fulfill({status:200, contentType:"text/css", body:"#diagram { width:240px; height:110px; overflow:auto }"});
      return route.fulfill({status:200, contentType:"text/html; charset=utf-8", body:
        `<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'"><script src="/vendor/pele-1.6.0.js"></script><script src="/sequence-pele.js"></script><link rel="stylesheet" href="/test.css"><div id="diagram"></div>`});
    });
    await page.goto("http://127.0.0.1:43987/");
    await page.evaluate(() => document.addEventListener("securitypolicyviolation", event =>
      (window.cspViolations ||= []).push(event.violatedDirective)));
    for (const language of ["javascript", "rust", "java", "python"]) {
      const fixture = peleFixture(language);
      const actual = await page.evaluate(async view => {
        let selected;
        const result = await window.TrellisPeleSequence.render(document.querySelector("#diagram"), view,
          null, new Set(), {onSelect:step => selected=step});
        const note = document.querySelector('.pele-note[data-trellis-step-id]');
        note?.dispatchEvent(new MouseEvent("click", {bubbles:true}));
        const sameOriginalObject = selected === view.steps[0];
        selected = undefined;
        note?.dispatchEvent(new KeyboardEvent("keydown", {key:"Enter", bubbles:true}));
        const keyboardOriginalObject = selected === view.steps[0];
        selected = undefined;
        return {status:result.status, reason:result.reason, notes:document.querySelectorAll('.pele-note[data-trellis-step-id]').length,
          original:sameOriginalObject, keyboard:keyboardOriginalObject, id:note?.getAttribute("data-trellis-step-id"), csp:window.cspViolations || []};
      }, fixture);
      assert.equal(actual.status, "rendered", `${language}: ${actual.reason}`);
      assert.equal(actual.notes, 5, language);
      assert.equal(actual.original, true, language);
      assert.equal(actual.keyboard, true, language);
      assert.deepEqual(actual.csp, [], language);
    }
    // Mutate actual Pele output, then run the production validator rather than
    // merely inspecting static fixture markup. No partial DOM may be installed.
    const mutated = await page.evaluate(async view => {
      const container = document.querySelector("#diagram");
      const originalRender = window.pele.render;
      const source = window.TrellisPeleSequence.project(view).source;
      const actual = originalRender(source, {}).svg;
      const cases = {
        guard:actual.replace(/(<text class="pele-frame-label"[^>]*>)([^<]+)(<\/text>)/g,
          (whole, open, label, close) => label === "[alternate path]" ? whole : `${open}[changed guard]${close}`),
        alternate:actual.replace(/(<text class="pele-frame-label"[^>]*>)[^<]+/, "$1[changed alternate]"),
        altIdentity:actual.replace('class="pele-frame pele-frame-alt"', 'class="pele-frame pele-frame-alt" data-id="i7"'),
        positionalNotes:actual.replace(/class="pele-note" data-id="i(\d+)"/g, (_, n) => `class="pele-note" data-id="i${Number(n) + 10}"`)
      };
      const results = {};
      for (const [name, svg] of Object.entries(cases)) {
        let selected = false;
        const previous = container.firstElementChild;
        window.pele = {render:() => ({svg})};
        const outcome = await window.TrellisPeleSequence.render(container, view, null, new Set(), {onSelect:() => selected=true});
        results[name] = {status:outcome.status, reason:outcome.reason, unchanged:container.firstElementChild === previous, selected};
      }
      window.pele = {render:originalRender};
      return results;
    }, peleFixture("rust"));
    for (const [name, expected] of Object.entries({guard:/frame header/, alternate:/frame header/, altIdentity:/frame identity/, positionalNotes:/note order/})) {
      assert.equal(mutated[name].status, "unsupported", name);
      assert.match(mutated[name].reason, expected, name);
      assert.equal(mutated[name].unchanged, true, name);
      assert.equal(mutated[name].selected, false, name);
    }
    const groupView = peleFixture("rust");
    const first = groupView.steps[0], last = groupView.steps[2];
    const nested = groupView.steps[1];
    const groupLabel = "GROUP".repeat(36), childLabel = "ENTRY".repeat(36);
    groupView.steps = [
      {...first, id:"group-alpha", kind:"group", label:groupLabel, callId:null,
        children:[{...first, label:childLabel}, last], alternate:[]},
      {...nested.children[0], id:"group-beta", kind:"group", label:"second measured chain", callId:null,
        children:[nested.children[0], nested.alternate[0]], alternate:[]}
    ];
    const group = await page.evaluate(async view => {
      window.groupView = view;
      window.groupSelected = null;
      const container = document.querySelector("#diagram");
      const projection = window.TrellisPeleSequence.project(view);
      const originalLabel = view.steps[0].label, originalChildLabel = view.steps[0].children[0].label;
      const result = await window.TrellisPeleSequence.render(container, view, null, new Set(),
        {onSelect:step => window.groupSelected=step});
      const note = container.querySelector('.pele-note[data-trellis-step-id="group-alpha"]');
      const second = container.querySelector('.pele-note[data-trellis-step-id="group-beta"]');
      const toggle = note?.querySelector("button.sequence-group-control");
      const secondToggle = second?.querySelector("button.sequence-group-control");
      if (result.status !== "rendered" || !toggle || !secondToggle) return {status:result.status, reason:result.reason};
      const text = note.textContent.replace(/\s+/g, "");
      note.dispatchEvent(new MouseEvent("click", {bubbles:true}));
      const originalSelection = window.groupSelected === view.steps[0];
      window.groupSelected = null;
      container.scrollTop = 24; container.scrollLeft = 35;
      const before = [container.scrollTop, container.scrollLeft];
      toggle.click();
      // The async toggle restores focus after its render promise resolves.
      await Promise.resolve();
      for (let i=0; i<50 && container.querySelector('[data-trellis-step-id="group-alpha"] button')?.getAttribute("aria-expanded") !== "true"; i++)
        await new Promise(resolve => setTimeout(resolve, 10));
      const successor = container.querySelector('[data-trellis-step-id="group-alpha"] button');
      return {status:result.status, collapsed:2,
        longEvidence:projection.rows[0].display.includes("first measured entry: ENTRY") &&
          projection.rows[0].display.includes(view.steps[0].children[0].callId) &&
          projection.rows[0].display.includes("+1 calls collapsed (entry preview)") &&
          projection.source.includes("#43;1 calls collapsed #40;entry preview#41;"),
        visibleEvidence:text.includes("ENTRY") && text.includes("+1callscollapsed(entrypreview)") &&
          text.includes(view.steps[0].children[0].callId),
        unchanged:originalLabel === view.steps[0].label && originalChildLabel === view.steps[0].children[0].label,
        identityBound:toggle.parentElement.localName === "foreignObject" && toggle.parentElement.parentElement === note &&
          secondToggle.parentElement.parentElement === second && toggle.getAttribute("aria-controls") === note.id &&
          secondToggle.getAttribute("aria-controls") === second.id && note.getAttribute("aria-describedby") === toggle.id,
        originalSelection, noToggleSelection:window.groupSelected === null,
        previewCallId:note.getAttribute("data-trellis-preview-call-id"),
        expanded:container.querySelectorAll('.pele-note[data-trellis-step-id]').length,
        focused:document.activeElement === successor,
        scroll:[container.scrollTop, container.scrollLeft], before, csp:window.cspViolations || []};
    }, groupView);
    assert.equal(group.status, "rendered", group.reason);
    for (const key of ["longEvidence", "visibleEvidence", "unchanged", "identityBound", "originalSelection", "noToggleSelection", "focused"])
      assert.equal(group[key], true, key);
    assert.equal(group.previewCallId, first.callId);
    assert.equal(group.expanded, 4);
    assert.deepEqual(group.scroll, group.before);
    assert.deepEqual(group.csp, []);
    const secondToggle = page.locator('button.sequence-group-control[data-trellis-step-id="group-beta"]');
    await secondToggle.focus();
    const beforeEnter = await page.evaluate(() => {
      const c = document.querySelector("#diagram"); c.scrollTop = 31; c.scrollLeft = 28;
      return [c.scrollTop, c.scrollLeft];
    });
    await secondToggle.press("Enter");
    assert.deepEqual(await page.evaluate(() => {
      const c = document.querySelector("#diagram");
      return {expanded:c.querySelectorAll('.pele-note[data-trellis-step-id]').length,
        focus:document.activeElement?.dataset.trellisStepId, scroll:[c.scrollTop,c.scrollLeft],
        selected:window.groupSelected === null};
    }), {expanded:6, focus:"group-beta", scroll:beforeEnter, selected:true});
    const firstToggle = page.locator('button.sequence-group-control[data-trellis-step-id="group-alpha"]');
    await firstToggle.focus();
    await firstToggle.press("Space");
    assert.equal(await page.locator('.pele-note[data-trellis-step-id]').count(), 4);
    assert.equal(await page.evaluate(() => document.activeElement?.dataset.trellisStepId), "group-alpha");
    await secondToggle.focus();
    await secondToggle.press("Space");
    assert.equal(await page.locator('.pele-note[data-trellis-step-id]').count(), 2);
    assert.equal(await page.evaluate(() => document.activeElement?.dataset.trellisStepId), "group-beta");
    const controlView = peleFixture("rust");
    const measured = controlView.steps[0], measuredNext = controlView.steps[2];
    controlView.steps = [
      {...measured, id:"loop-1", kind:"loop", label:"possible iterations", callId:null,
        children:[{...measured, id:"loop-call"}], alternate:[]},
      {...measured, id:"try-1", kind:"try", label:"try: paths and cleanup unknown", callId:null,
        children:[{...measuredNext, id:"try-call"}], alternate:[{...measured, id:"handler-call"}]}
    ];
    const controls = await page.evaluate(async view => {
      const container = document.querySelector("#diagram");
      const result = await window.TrellisPeleSequence.render(container, view, null, new Set(), {});
      return {status:result.status, loops:container.querySelectorAll(".pele-frame-loop").length,
        rects:container.querySelectorAll(".pele-rect").length, notes:container.querySelectorAll('.pele-note[data-trellis-step-id]').length};
    }, controlView);
    assert.deepEqual(controls, {status:"rendered", loops:1, rects:1, notes:5});
    const emptyView = peleFixture("rust"); emptyView.steps = [];
    const empty = await page.evaluate(view => window.TrellisPeleSequence.render(
      document.querySelector("#diagram"), view, null, new Set(), {}), emptyView);
    assert.equal(empty.status, "rendered", empty.reason);
    assert.deepEqual(errors, []);
  } finally { await browser.close(); }
});

test("Pele projector enforces 24 visible levels and the exact 512 KiB UTF-8 Mermaid boundary", () => {
 const adapter=peleProjector(), view=peleFixture("rust"), leaf=view.steps[0];
 const chain=levels=>{let node={...leaf,id:`level-${levels-1}`,children:[],alternate:[]};
   for(let i=levels-2;i>=0;i--) node={...leaf,id:`level-${i}`,kind:"note",children:[node],alternate:[]};
   return node;};
 view.steps=[chain(24)];
 assert.equal(adapter.project(view).rows.length,24);
 view.steps=[chain(25)];assert.throws(()=>adapter.project(view),/Unsupported sequence depth/);
 view.steps=[{...leaf,id:"mermaid-bytes",label:""}];
 const sourceSize=label=>new TextEncoder().encode(adapter.project({...view,steps:[{...view.steps[0],label}]},new Set(),{showDetails:true}).source).length;
 const fixed=sourceSize("call");
 assert.ok(fixed<512*1024);
 const limit="x".repeat(512*1024-fixed+4);
 assert.equal(sourceSize(limit),512*1024);
 assert.throws(()=>adapter.project({...view,steps:[{...view.steps[0],label:limit+"x"}]},new Set(),{showDetails:true}),/Mermaid source too large/);
});
