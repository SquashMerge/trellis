"use strict";
const test = require("node:test"), assert = require("node:assert/strict"), fs = require("node:fs"), vm = require("node:vm");
function setup() {
  const window = {};
  vm.runInNewContext(fs.readFileSync("web/architecture-pele.js","utf8"),{window,TextEncoder});
  const vendor = vm.createContext({}); vm.runInContext(fs.readFileSync("web/vendor/pele-1.10.0-trellis.2.js","utf8"),vendor);
  return {adapter:window.TrellisArchitecturePele,pele:vendor.pele};
}
const {fixture} = require("./fixtures/pele-1.10.0-trellis.2/architecture-fixture.cjs");
test("patched block projection maps forty exact native file identities, peers/shared and summary roles",()=>{
  const {adapter,pele}=setup(), view=fixture(), p=adapter.project(view,{showTests:true});
  assert.equal([...p.nodes.values()].filter(n=>n.kind==="M").length,40);
  assert.equal(p.edges.size,5);
  assert.equal([...p.nodes.values()].find(n=>n.kind==="M").record,view.modules[0]);
  const raw=pele.render(p.source,{fontSize:14,padding:8,responsive:false,links:false,images:false}).svg;
  assert.match(raw,/class="pele pele-block"/);
  for(const [id,n] of p.nodes) assert.equal((raw.match(new RegExp(`data-id="${id}"`,"g"))||[]).length,1);
  for(const e of p.edges.values()) {
    for (const line of e.placeholder.split("<br/>")) assert.ok(raw.includes(line),`missing generated edge token ${e.id}`);
    assert.match(raw,new RegExp(`data-id="[1-9][0-9]*-${e.from}-${e.to}"`));
  }
  assert.equal((raw.match(/class="pele-edge"/g)||[]).length,5);
});
test("repository strings never become Mermaid grammar and preserve original DTO identity",()=>{
  const {adapter}=setup(), view=fixture(), hostile='x"]\nend\nclick x "javascript:alert(1)" <script>&';
  view.layers[0].name=hostile; view.modules[0].layer=hostile; view.edges.forEach(e=>{ if(e.sourceLayer==="UI")e.sourceLayer=hostile; if(e.destinationLayer==="UI")e.destinationLayer=hostile; });
  view.modules.filter(m=>m.layer==="UI").forEach(m=>m.layer=hostile);
  view.modules[0].identity.path=hostile;
  const p=adapter.project(view,{showTests:true});
  assert.ok(!p.source.includes(hostile)); assert.ok(!p.source.includes("javascript:"));
  const module=[...p.nodes.values()].find(n=>n.record===view.modules[0]);
  assert.equal(module.record.identity.path,hostile); assert.ok(module.full.includes(hostile));
  const duplicate=fixture(); duplicate.modules.push(duplicate.modules[0]); assert.throws(()=>adapter.project(duplicate),/duplicate Architecture module/);
});
test("test toggle changes only rendered test edges and never production records, counts or modules",()=>{
  const {adapter}=setup(), view=fixture(), before=JSON.stringify(view);
  const hidden=adapter.project(view), shown=adapter.project(view,{showTests:true});
  assert.equal(hidden.edges.size,4);assert.equal(shown.edges.size,5);
  assert.deepEqual([...hidden.edges.values()].map(e=>e.record),view.edges.filter(e=>e.role==="production"));
  assert.equal(JSON.stringify(view),before);
  assert.equal([...hidden.nodes.values()].filter(n=>n.kind==="M").length,40);
});
test("empty authored layers and unassigned modules remain explicit without inventing membership",()=>{
  const {adapter}=setup(), view=fixture();view.layers.push({name:"Empty",order:3,shared:false});view.modules[0].layer=null;
  const p=adapter.project(view);
  assert.ok([...p.nodes.values()].some(n=>n.full==="No indexed modules"));
  assert.ok([...p.nodes.values()].some(n=>n.full==="Unassigned / unknown"));
  assert.equal([...p.nodes.values()].find(n=>n.record===view.modules[0]).record.layer,null);
});
test("all edge policy/exception buckets retain separate exact records without count matching",()=>{
  const {adapter}=setup(), view=fixture(); const first=view.edges[0];
  view.edges.push({...first,status:"deviates",excepted:true,exceptionReason:"Reviewed boundary"});
  const p=adapter.project(view,{showTests:true});
  assert.equal(p.edges.size,6);assert.equal(new Set([...p.edges.values()].map(e=>e.expected)).size,6);
  const duplicate=fixture();duplicate.edges.push({...duplicate.edges[0]});assert.throws(()=>adapter.project(duplicate),/duplicate Architecture edge/);
});

test("layer counters keep separate rows and reserve room beyond compact module chips",()=>{
  const {adapter}=setup(),view=fixture();
  const p=adapter.project(view,{layerLabel:layer=>`${layer.name}\nMax module cognitive (max)\n17\nProd refs down: 23456\nProd refs up: 78\nUp deviation: 78\nPeer/shared deviation: 900\nExcepted refs: 12`});
  const layer=[...p.nodes.values()].find(n=>n.kind==="L");
  assert.equal(layer.display.length,8);assert.equal(layer.display[4],"Prod refs up: 78");
  assert.equal(layer.display[6],"Peer/shared deviation: 900");
  assert.ok(p.source.includes(']:4'));
});
module.exports={fixture};
test("display projection: module chips, a folded band as a short composite, stats line and compact arrow counts",()=>{
  const {adapter,pele}=setup(), view=fixture();
  const display={layers:new Set(view.layers.map(l=>l.name)),expanded:name=>name!=="Store",edge:()=>true,unassigned:null,
    entries:(key,modules)=>modules.map(module=>({type:"module",module,name:module.identity.path.slice(4,-3)}))};
  const p=adapter.project(view,{display,measureText:text=>Array.from(text).length*7,layerStats:layer=>layer.name==="Store"?[{text:"↑7",label:"7 upward deviation production sites",tone:"error"}]:[]});
  const store=[...p.nodes.values()].find(n=>n.kind==="L"&&n.record.name==="Store");
  assert.equal(store.composite,true,"a folded band is a composite, so its row is not as tall as the stack");
  assert.equal(store.display[store.statsLine],"↑7");assert.match(store.full,/7 upward deviation production sites/);
  const folded=[...p.nodes.values()].find(n=>n.kind==="S");assert.equal(folded.owner,store);assert.equal(folded.full,"8 files folded");
  const chip=[...p.nodes.values()].find(n=>n.kind==="M");assert.equal(chip.display[0],"module_0");assert.equal(chip.full.split("\n")[0],"src/module_0.rs");
  const up=[...p.edges.values()].find(e=>e.record.sourceLayer==="Store");assert.deepEqual([...up.display],["↑7"]);assert.equal(up.full,"7 production sites · upward\ndeviation");
  const raw=pele.render(p.source,{fontSize:14,padding:8,responsive:false,links:false,images:false}).svg;
  for(const id of p.nodes.keys()) assert.equal((raw.match(new RegExp(`data-id="${id}"`,"g"))||[]).length,1);
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
