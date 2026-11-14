"use strict";
const test = require("node:test"), assert = require("node:assert/strict"), fs = require("node:fs"), vm = require("node:vm");
// `dom` (the Herdr mod's DOM, herdr-mod/src/dom.js) is for the one test that admits a drawing.
function setup(dom) {
  const window = {};
  vm.runInNewContext(["web/metric-heat.js","web/architecture-apps.js","web/architecture-display.js","web/pele-svg.js","web/architecture-routing.js","web/architecture-pele.js"].map(file=>fs.readFileSync(file,"utf8")).join("\n"),
    {window,TextEncoder,...(dom?{DOMParser:dom.DOMParser,document:dom.createDocument(text=>Array.from(text).length*7)}:{})});
  const vendor = vm.createContext({}); vm.runInContext(fs.readFileSync("web/vendor/pele-1.10.0-trellis.2.js","utf8"),vendor);
  return {adapter:window.TrellisArchitecturePele,pele:vendor.pele,window};
}
const {fixture} = require("./fixtures/pele-1.10.0-trellis.2/architecture-fixture.cjs");
// The adapter's options as the product builds them: the real `createDisplay` (web/architecture-display.js),
// its plan for the whole payload and `options(part, scale)`, as web/architecture.js `stack` and the Herdr
// mod's `summarize` do. `open` names the open bands (every band unless given) and `unassigned` opens that row.
function stack(window,view,{showTests=false,open=view.layers.map(layer=>layer.name),unassigned=false}={}) {
  const shown={open:new Map(open.map(name=>[name,true])),drilled:new Set(),unassigned,hidden:false};
  const shared=window.TrellisArchitectureDisplay.createDisplay({metric:()=>"cognitive",aggregate:()=>"max",role:()=>"production",relative:()=>false,
    showTests:()=>showTests,view:()=>view,model:()=>view,shown:()=>shown,endMark:()=>null,scopeGroup:()=>null});
  const plan=shared.plan(view,null);
  return shared.options(plan.parts[0],plan.scale);
}
// The display draws a file chip only for a file with measured declarations, as the payload reports them.
const measured=view=>{for(const module of view.modules)module.declarationCount=1;return view;};
test("patched block projection maps forty exact native file identities, peers/shared and summary roles",async()=>{
  const {adapter,pele,window}=setup(await import("../herdr-mod/src/dom.js")), view=measured(fixture()), p=adapter.project(view,stack(window,view,{showTests:true}));
  assert.equal([...p.nodes.values()].filter(n=>n.kind==="M").length,40);
  assert.equal(p.edges.size,5);
  assert.equal([...p.nodes.values()].find(n=>n.kind==="M").record,view.modules[0]);
  const raw=pele.render(p.source,{fontSize:14,padding:8,responsive:false,links:false,images:false}).svg;
  assert.match(raw,/class="pele pele-block"/);
  for(const [id,n] of p.nodes) assert.equal((raw.match(new RegExp(`data-id="${id}"`,"g"))||[]).length,1);
  // Pele draws the boxes; the five arrows are the adapter's own, added when the drawing is admitted.
  assert.equal((raw.match(/class="pele-edge"/g)||[]).length,0);
  const {root}=adapter.validatedSvg(raw,p), tags=[...root.querySelectorAll(".pele-edge-label")];
  for(const e of p.edges.values()) {
    const tag=tags.filter(node=>node.querySelector("text").textContent===e.placeholder);
    assert.equal(tag.length,1,`missing generated edge token ${e.id}`);
    assert.match(tag[0].getAttribute("data-id"),new RegExp(`^[1-9][0-9]*-${e.from}-${e.to}$`));
    assert.equal(root.querySelectorAll(`.pele-edge[data-id="${tag[0].getAttribute("data-id")}"]`).length,1);
  }
  assert.equal(root.querySelectorAll(".pele-edge").length,5);
});
test("repository strings never become Mermaid grammar and preserve original DTO identity",()=>{
  const {adapter,window}=setup(), view=measured(fixture()), hostile='x"]\nend\nclick x "javascript:alert(1)" <script>&';
  view.layers[0].name=hostile; view.modules[0].layer=hostile; view.edges.forEach(e=>{ if(e.sourceLayer==="UI")e.sourceLayer=hostile; if(e.destinationLayer==="UI")e.destinationLayer=hostile; });
  view.modules.filter(m=>m.layer==="UI").forEach(m=>m.layer=hostile);
  view.modules[0].identity.path=hostile;
  const p=adapter.project(view,stack(window,view,{showTests:true}));
  assert.ok(!p.source.includes(hostile)); assert.ok(!p.source.includes("javascript:"));
  const module=[...p.nodes.values()].find(n=>n.record===view.modules[0]);
  assert.equal(module.record.identity.path,hostile); assert.ok(module.full.includes(hostile));
  const duplicate=measured(fixture()); duplicate.modules.push(duplicate.modules[0]); assert.throws(()=>adapter.project(duplicate,stack(window,duplicate)),/duplicate Architecture module/);
});
test("test toggle changes only rendered test edges and never production records, counts or modules",()=>{
  const {adapter,window}=setup(), view=measured(fixture()), before=JSON.stringify(view);
  const hidden=adapter.project(view,stack(window,view)), shown=adapter.project(view,stack(window,view,{showTests:true}));
  assert.equal(hidden.edges.size,4);assert.equal(shown.edges.size,5);
  assert.deepEqual([...hidden.edges.values()].map(e=>e.record),view.edges.filter(e=>e.role==="production"));
  assert.equal(JSON.stringify(view),before);
  assert.equal([...hidden.nodes.values()].filter(n=>n.kind==="M").length,40);
});
test("empty authored layers and unassigned modules remain explicit without inventing membership",()=>{
  const {adapter,window}=setup(), view=measured(fixture());view.layers.push({name:"Empty",order:3,shared:false});view.modules[0].layer=null;
  const p=adapter.project(view,stack(window,view,{unassigned:true})), nodes=[...p.nodes.values()];
  // The words are the display's: an open band with no file says so, and the unassigned row counts its files.
  const empty=nodes.find(n=>n.kind==="L"&&n.record.name==="Empty");
  assert.ok(nodes.some(n=>n.full==="No files shown"&&n.host===empty.id));
  const row=nodes.find(n=>n.kind==="U");
  assert.equal(row.full,"Unassigned / unknown\n1 file · no layer");
  const loose=nodes.find(n=>n.record===view.modules[0]);
  assert.equal(loose.record.layer,null);assert.equal(loose.host,row.id);
});
test("all edge policy/exception buckets retain separate exact records without count matching",()=>{
  const {adapter,window}=setup(), view=measured(fixture()); const first=view.edges[0];
  view.edges.push({...first,status:"deviates",excepted:true,exceptionReason:"Reviewed boundary"});
  const p=adapter.project(view,stack(window,view,{showTests:true}));
  assert.equal(p.edges.size,6);assert.equal(new Set([...p.edges.values()].map(e=>e.expected)).size,6);
  const duplicate=measured(fixture());duplicate.edges.push({...duplicate.edges[0]});assert.throws(()=>adapter.project(duplicate,stack(window,duplicate)),/duplicate Architecture edge/);
});

test("layer counters keep separate rows and reserve room beyond compact module chips",()=>{
  const {adapter,window}=setup(),view=measured(fixture());
  // The display's own options, with a header of eight rows in place of its label.
  const p=adapter.project(view,{...stack(window,view),layerLabel:layer=>`${layer.name}\nMax module cognitive (max)\n17\nProd refs down: 23456\nProd refs up: 78\nUp deviation: 78\nPeer/shared deviation: 900\nExcepted refs: 12`});
  const layer=[...p.nodes.values()].find(n=>n.kind==="L");
  // The eight rows stay whole, above the band's own stat line.
  assert.equal(layer.statLines.length,1);assert.equal(layer.display.length,8+layer.statLines.length);assert.equal(layer.display[4],"Prod refs up: 78");
  assert.equal(layer.display[6],"Peer/shared deviation: 900");
  // The header reserves its band's whole row of five chips, and the stack spans those five columns.
  assert.equal(layer.lineWidth,5*(104+32)+4*12-32);
  assert.ok(p.source.includes(`block:${p.layout.stack}["${p.nodes.get(p.layout.stack).placeholder}"]:5`));
});
module.exports={fixture};
test("display projection: module chips, a folded band as a short composite, stats line and compact arrow counts",()=>{
  const {adapter,pele}=setup(), view=fixture();
  const display={layers:new Set(view.layers.map(l=>l.name)),expanded:name=>name!=="Store",edge:()=>true,unassigned:null,
    entries:(key,modules)=>modules.map(module=>({type:"module",module,name:module.identity.path.slice(4,-3)}))};
  const p=adapter.project(view,{display,measureText:text=>Array.from(text).length*7,layerStats:layer=>layer.name==="Store"?[{text:"↑7",label:"7 upward deviation production sites",tone:"error"}]:[]});
  const store=[...p.nodes.values()].find(n=>n.kind==="L"&&n.record.name==="Store");
  assert.equal(store.composite,true,"a folded band is a composite, so its row is not as tall as the stack");
  assert.equal(store.display[store.statLines[0].line],"↑7");assert.match(store.full,/7 upward deviation production sites/);
  const folded=[...p.nodes.values()].find(n=>n.kind==="S");assert.equal(folded.owner,store);assert.equal(folded.full,"8 files folded");
  const chip=[...p.nodes.values()].find(n=>n.kind==="M");assert.equal(chip.display[0],"module_0");assert.equal(chip.full.split("\n")[0],"src/module_0.rs");
  const up=[...p.edges.values()].find(e=>e.record.sourceLayer==="Store");assert.deepEqual([...up.display],["↑7"]);assert.equal(up.full,"7 production sites · upward\ndeviation");
  const raw=pele.render(p.source,{fontSize:14,padding:8,responsive:false,links:false,images:false}).svg;
  for(const id of p.nodes.keys()) assert.equal((raw.match(new RegExp(`data-id="${id}"`,"g"))||[]).length,1);
});
test("#449: open bands charge the chip budget before collapsed summary rows, whatever their order",()=>{
  const {adapter}=setup(), view=fixture(), budget={left:6}, calls=[];
  const last=[...view.layers].filter(l=>!l.shared).sort((a,b)=>b.order-a.order||b.name.localeCompare(a.name))[0].name;
  const display={layers:new Set(view.layers.map(l=>l.name)),edge:()=>true,unassigned:null,reset:()=>{budget.left=6;},
    expanded:name=>name===last&&budget.left>0,
    entries:(key,modules)=>{calls.push(["entries",key]);const kept=modules.slice(0,budget.left);budget.left-=kept.length;return kept.map(module=>({type:"module",module,name:module.identity.path}));},
    summary:(key,modules)=>{calls.push(["summary",key]);const kept=modules.slice(0,budget.left);budget.left-=kept.length;return kept.map(module=>({type:"chip",band:null,label:"x",shown:"x"}));}};
  const p=adapter.project(view,{display,columns:4,measureText:text=>Array.from(text).length*7});
  assert.deepEqual(calls[0],["entries",last],"the opened band is charged first even though it is drawn last");
  assert.equal([...p.nodes.values()].find(n=>n.kind==="L"&&n.record.name===last).expanded,true,"summary rows cannot fold a band the viewer opened");
  assert.ok([...p.nodes.values()].filter(n=>n.kind==="M").length>0);
});
test("#449: collapsed bands draw summary chips, width sets chip columns, arrows carry lower bounds and coverage",()=>{
  const {adapter,pele}=setup(), view=fixture(); let resets=0;
  const summary=(key,modules,columns)=>modules.slice(0,columns).map((module,i)=>({type:"chip",band:i%4,label:`${module.identity.path} · chip`,shown:`m${i}\n${i}`}));
  const display={layers:new Set(view.layers.map(l=>l.name)),expanded:()=>false,edge:()=>true,unassigned:null,entries:()=>[],summary,reset:()=>{resets++;}};
  const edgeText=edge=>({bound:true,label:"lower bound; 9% of 100 production reference sites mapped",note:edge.status==="deviates"?"9% mapped":""});
  const edgeTone=edge=>edge.status==="deviates"?"deviation":"allowed";
  const options={display,edgeText,edgeTone,measureText:text=>Array.from(text).length*7};
  const narrow=adapter.project(view,{...options,width:600}), wide=adapter.project(view,{...options,width:1800});
  assert.equal(resets,2,"each projection resets the caller's chip budget");
  const chips=p=>[...p.nodes.values()].filter(n=>n.kind==="C");
  assert.ok(chips(narrow).length>0,"no band says only 'N files folded' when it has measured modules");
  assert.equal([...narrow.nodes.values()].some(n=>n.kind==="S"),false);
  assert.ok(chips(wide).length>chips(narrow).length,"a wider canvas holds more chips per row");
  assert.ok(adapter.project(view,{...options,columns:3}).source.includes("columns 3"),"an explicit column count wins");
  const up=[...wide.edges.values()].find(e=>e.record.status==="deviates"&&e.record.role==="production");
  assert.match(up.display[0],/^↑≥\d+ · 9% mapped$/); assert.match(up.full,/lower bound; 9% of 100 production reference sites mapped$/); assert.equal(up.tone,"deviation");
  const allowed=[...wide.edges.values()].find(e=>e.record.status==="conforms");
  assert.match(allowed.display[0],/^.≥\d+$/,"allowed arrows keep the bound but not the coverage note"); assert.equal(allowed.tone,"allowed");
  const raw=pele.render(wide.source,{fontSize:14,padding:8,responsive:false,links:false,images:false}).svg;
  for(const id of wide.nodes.keys()) assert.equal((raw.match(new RegExp(`data-id="${id}"`,"g"))||[]).length,1);
});
test("AC26: a long label wraps to at most twelve lines, keeps its full accessible name, and the title is the stack header",()=>{
  const {adapter}=setup(), view=fixture(), long="segment/".repeat(240);
  const display={layers:new Set(view.layers.map(l=>l.name)),expanded:()=>false,edge:()=>true,unassigned:null,entries:()=>[]};
  const p=adapter.project(view,{display,title:"web · ordered layers",measureText:text=>Array.from(text).length*7,layerLabel:layer=>layer.name==="Store"?`${long}\nOrder 1`:`${layer.name}\nOrder ${layer.order}`});
  const store=[...p.nodes.values()].find(n=>n.kind==="L"&&n.record.name==="Store");
  assert.equal(store.display.length,13,"twelve wrapped lines plus the order line");
  assert.ok(store.display[11].endsWith("…"));assert.ok(store.full.startsWith(long));
  assert.ok([...p.nodes.values()].some(n=>n.kind==="B"&&n.full==="web · ordered layers"));
});
test("AC26: a label within the twelve-line cap wraps whole, with nothing dropped and the full accessible name",()=>{
  const {adapter}=setup(), view=fixture(), label="src/web_app/layer_name-with.separators/".repeat(4).slice(0,120);
  assert.equal(label.length,120);
  const display={layers:new Set(view.layers.map(l=>l.name)),expanded:()=>false,edge:()=>true,unassigned:null,entries:()=>[]};
  const p=adapter.project(view,{display,measureText:text=>Array.from(text).length*7,layerLabel:layer=>layer.name==="Store"?`${label}\nOrder 1`:`${layer.name}\nOrder ${layer.order}`});
  const store=[...p.nodes.values()].find(n=>n.kind==="L"&&n.record.name==="Store");
  const lines=store.display.slice(0,-1);
  assert.ok(lines.length>1&&lines.length<=12);
  assert.ok(lines.every(line=>!line.includes("…")));
  assert.equal(lines.join(""),label);
  assert.equal(store.full,`${label}\nOrder 1`);
});
test("AC25: a stat with a tag always reserves room for it, so selecting an interface stub never moves the layout",()=>{
  const {adapter}=setup(), view=fixture();
  const display={layers:new Set(view.layers.map(l=>l.name)),expanded:()=>false,edge:()=>true,unassigned:null,entries:()=>[]};
  const stats=tag=>layer=>layer.name==="Store"?[{text:"→ daemon · 3 routes · 5 call sites".padEnd(33,"."),label:"to daemon",tone:"neutral",tag,action:{interface:{client:"web",server:"daemon"}}},
    {text:"↑2 deviation · Core".padEnd(33,"."),label:"up",tone:"error"}]:[];
  const lines=tag=>[...adapter.project(view,{display,measureText:text=>Array.from(text).length*9,layerStats:stats(tag)}).nodes.values()].find(n=>n.kind==="L"&&n.record.name==="Store").statLines.map(line=>line.stats.length);
  const plain=lines(undefined), tagged=lines("Selected");
  assert.deepEqual([...plain],[2],"both chips fit one header line without the tag");
  assert.deepEqual([...tagged],[1,1],"the reserved tag takes room the chips would otherwise share");
});
test("#850: sharedSide left draws the shared boxes left of the stack, nearest first beside it, with the same chip budget order",()=>{
  const {adapter,pele}=setup(), drawn={};
  for (const side of [undefined,"left"]) {
    const view=fixture(), calls=[];
    view.layers.push({name:"Common",shared:true});
    const display={layers:new Set(view.layers.map(l=>l.name)),expanded:()=>false,edge:()=>true,unassigned:null,entries:()=>[],
      summary:(key,modules)=>{calls.push(key);return modules.slice(0,1).map(()=>({type:"chip",band:null,label:"x",shown:"x"}));}};
    const p=adapter.project(view,{display,sharedSide:side,measureText:text=>Array.from(text).length*7});
    const raw=pele.render(p.source,{fontSize:14,padding:8,responsive:false,links:false,images:false}).svg;
    const x=id=>Number(new RegExp(`data-id="${id}"[^>]*><rect x="([\\d.]+)"`).exec(raw)[1]);
    const name=id=>[...p.nodes.values()].find(n=>n.id===id).record.name;
    drawn[side||"right"]={calls,sharedLeft:p.layout.sharedLeft,stack:x(p.layout.stack),shared:p.layout.shared.map(id=>[name(id),x(id)]),
      tags:[...p.edges.values()].filter(e=>/into shared|shared outbound/.test(e.full)).map(e=>[e.full.split("\n")[0].split(" · ")[1],e.display[0][0]])};
  }
  const {right,left}=drawn;
  assert.deepEqual(right.shared.map(([n])=>n),["Common","Shared"]); assert.deepEqual(left.shared.map(([n])=>n),["Common","Shared"],"layout.shared stays nearest first");
  assert.equal(right.sharedLeft,false); assert.equal(left.sharedLeft,true);
  assert.ok(right.stack<right.shared[0][1]&&right.shared[0][1]<right.shared[1][1],`right: stack, Common, Shared ${JSON.stringify(right)}`);
  assert.ok(left.shared[1][1]<left.shared[0][1]&&left.shared[0][1]<left.stack,`left: Shared, Common, stack ${JSON.stringify(left)}`);
  assert.deepEqual(left.calls,right.calls,"chip budgets charge in the same order on either side");
  assert.ok(right.tags.length>0,"the fixture has arrows to or from a shared box");
  for(const [way,glyph] of right.tags) assert.equal(glyph,way==="into shared"?"→":"←");
  for(const [way,glyph] of left.tags) assert.equal(glyph,way==="into shared"?"←":"→","a tag points the way its arrow runs");
});
test("#850: sharedSide is ignored without a shared layer",()=>{
  const {adapter}=setup();
  const ordered=fixture(); ordered.layers=ordered.layers.filter(l=>!l.shared); ordered.modules=ordered.modules.filter(m=>ordered.layers.some(l=>l.name===m.layer)||m.layer===null);
  ordered.edges=ordered.edges.filter(e=>ordered.layers.some(l=>l.name===e.sourceLayer)&&ordered.layers.some(l=>l.name===e.destinationLayer));
  const display={layers:new Set(ordered.layers.map(l=>l.name)),expanded:()=>false,edge:()=>true,unassigned:null,entries:()=>[]};
  const p=adapter.project(ordered,{display,sharedSide:"left",measureText:text=>Array.from(text).length*7});
  assert.equal(p.layout.sharedLeft,false);
  assert.equal(p.source,adapter.project(ordered,{display,measureText:text=>Array.from(text).length*7}).source);
});
test("#1066: a call without a display is refused",()=>{
  const {adapter}=setup();
  assert.throws(()=>adapter.project(fixture()),error=>error.message==="Architecture display unavailable"&&error.code==="unsupported");
  assert.throws(()=>adapter.project(fixture(),{showTests:true,display:null}),/Architecture display unavailable/);
});
