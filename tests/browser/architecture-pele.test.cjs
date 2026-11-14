"use strict";
const test=require("node:test"),assert=require("node:assert/strict"),fs=require("node:fs"),path=require("node:path"),http=require("node:http");
const {chromium}=require("playwright");
const {fixture}=require("../fixtures/pele-1.10.0-trellis.2/architecture-fixture.cjs");
const root=path.resolve(__dirname,"../..");
test("Architecture patched Pele admission is atomic, non-positional and CSP-safe",async()=>{
 const routes={"/vendor.js":"web/vendor/pele-1.10.0-trellis.2.js","/metric-heat.js":"web/metric-heat.js","/apps.js":"web/architecture-apps.js","/display.js":"web/architecture-display.js","/pele-svg.js":"web/pele-svg.js","/routing.js":"web/architecture-routing.js","/adapter.js":"web/architecture-pele.js"};
 const server=http.createServer((req,res)=>{res.setHeader("Content-Security-Policy","default-src 'none'; script-src 'self'; style-src 'self'; font-src 'self'; connect-src 'self'; img-src 'self'");if(routes[req.url]){res.setHeader("Content-Type","text/javascript; charset=utf-8");res.end(fs.readFileSync(path.join(root,routes[req.url])));}else{res.setHeader("Content-Type","text/html; charset=utf-8");res.end('<!doctype html><div id="canvas"></div><script src="/vendor.js"></script><script src="/metric-heat.js"></script><script src="/apps.js"></script><script src="/display.js"></script><script src="/pele-svg.js"></script><script src="/routing.js"></script><script src="/adapter.js"></script>');}});
 await new Promise(resolve=>server.listen(0,"127.0.0.1",resolve));let browser;
 try{browser=await chromium.launch();const page=await browser.newPage({viewport:{width:1440,height:1000}});const errors=[];page.on("pageerror",error=>errors.push(error.message));await page.goto(`http://127.0.0.1:${server.address().port}`);
 const view=fixture();view.edges.push({...view.edges[0],status:"deviates",excepted:true,exceptionReason:"Reviewed fixture exception"});
 const result=await page.evaluate(async view=>{
  const canvas=document.querySelector("#canvas"),A=window.TrellisArchitecturePele,violations=[],selected=[];document.addEventListener("securitypolicyviolation",event=>violations.push(event.violatedDirective));
  const options={showTests:true,moduleValue:module=>module.metrics.cognitive,moduleBand:module=>module.metrics.cognitive%4,layerLabel:layer=>`${layer.order ?? "Shared"} · ${layer.name} · max cognitive (max) 17`,layerStats:()=>[{text:"↑78",label:"78 upward deviation production sites",tone:"error"},{text:"↓23456",label:"23456 downward production sites",tone:"neutral"}],
   display:{layers:new Set(view.layers.map(layer=>layer.name)),expanded:()=>true,edge:()=>true,entries:(key,modules)=>modules.map(module=>({type:"module",module,name:module.identity.path.slice(4,-3)})),unassigned:null},onToggleLayer:layer=>selected.push(layer),onSelectModule:module=>selected.push(module),onSelectEdge:edge=>selected.push(edge),onSelectLayer:layer=>selected.push(layer)};
  const ready=await A.render(canvas,view,options);if(ready.status!=="ready")return{error:ready.error?.message};
  const first=canvas.querySelector('[data-trellis-module-id]');first.dispatchEvent(new KeyboardEvent("keydown",{key:"Enter",bubbles:true}));canvas.querySelector('[data-trellis-edge-id]').dispatchEvent(new KeyboardEvent("keydown",{key:" ",bubbles:true}));
  const productionRects=[...canvas.querySelectorAll('.pele-cluster rect[stroke="#FF8E9A"]')];const chipTexts=[...canvas.querySelectorAll('.pele-cluster text.trellis-architecture-stat')].map(text=>[text.textContent,text.getAttribute("data-tone")]);const edgeLabels=[...canvas.querySelectorAll('[data-trellis-edge-id]')].map(node=>[node.querySelector("text").textContent,node.getAttribute("aria-label")]);
  const badgeGeometry=productionRects.every(rect=>{const group=rect.parentElement;return Number(rect.getAttribute("x"))>=Number(group.children[0].getAttribute("x")) && Number(rect.getAttribute("x"))+Number(rect.getAttribute("width"))<=Number(group.children[0].getAttribute("x"))+Number(group.children[0].getAttribute("width"));});
  // The projection the product makes: the real createDisplay (web/architecture-display.js), its plan and its options for the one part.
  const shown={open:new Map(),drilled:new Set(),unassigned:false,hidden:false};
  const shared=window.TrellisArchitectureDisplay.createDisplay({metric:()=>"cognitive",aggregate:()=>"max",role:()=>"production",relative:()=>false,showTests:()=>true,view:()=>view,model:()=>view,shown:()=>shown,endMark:()=>null,scopeGroup:()=>null});
  const plan=shared.plan(view,null),p=A.project(view,shared.options(plan.parts[0],plan.scale)),renderOptions={fontSize:14,padding:8,responsive:false,links:false,images:false};const original=pele.render,raw=original(p.source,renderOptions).svg;
  // Admission adds the adapter's own arrows (a group of connectors, then a group of count boxes) to Pele's boxes.
  // `arrows` keeps them, as a drawing in which Pele had drawn connectors; otherwise they come off again.
  function admittedMutation(change,arrows=false){const root=A.validatedSvg(raw,p).root;if(!arrows){root.lastElementChild.remove();root.lastElementChild.remove();}change(root);let value=new XMLSerializer().serializeToString(root);return value.replace('class="pele pele-block"','class="pele pele-block" '+raw.match(/style="[^"]*"/)[0]).replaceAll('rx="var(--_r)"','style="rx:var(--_r)"');}
  const mutations={
   missingNode:admittedMutation(root=>root.querySelector('.pele-node').remove()),
   duplicateNode:admittedMutation(root=>{const node=root.querySelector('.pele-node');node.parentElement.append(node.cloneNode(true));}),
   nodePlaceholder:admittedMutation(root=>root.querySelector('.pele-node text').textContent="Changed"),
   nodeClass:admittedMutation(root=>root.querySelector('.pele-node').setAttribute("class","pele-node changed")),
   nodeLocalShape:admittedMutation(root=>{const nodes=root.querySelectorAll('.pele-node');nodes[1].append(nodes[0].querySelector('rect'));}),
   // Pele is given no edges, so it draws none: a connector or a count box in its output is refused, whole or damaged.
   peleConnectors:admittedMutation(()=>{},true),
   peleConnectorOnly:admittedMutation(root=>{root.lastElementChild.remove();const edges=root.querySelectorAll('.pele-edge');for(const edge of [...edges].slice(1))edge.remove();},true),
   peleLabelOnly:admittedMutation(root=>{const labels=root.lastElementChild;root.lastElementChild.previousElementSibling.remove();for(const label of [...labels.children].slice(1))label.remove();},true),
   missingConnector:admittedMutation(root=>root.querySelector('.pele-edge').remove(),true),
   duplicateConnector:admittedMutation(root=>{const node=root.querySelector('.pele-edge');node.parentElement.append(node.cloneNode(true));},true),
   missingLabel:admittedMutation(root=>root.querySelector('.pele-edge-label').remove(),true),
   duplicateLabel:admittedMutation(root=>{const node=root.querySelector('.pele-edge-label');node.parentElement.append(node.cloneNode(true));},true),
   labelLocalShape:admittedMutation(root=>{const labels=root.querySelectorAll('.pele-edge-label');labels[1].append(labels[0].querySelector('rect'));},true),
   connectorLocalShape:admittedMutation(root=>{const edges=root.querySelectorAll('.pele-edge');edges[1].append(edges[0].querySelector('.pele-marker'));},true),
   wrongEndpoint:admittedMutation(root=>{for(const node of root.querySelectorAll('.pele-edge,.pele-edge-label'))node.setAttribute('data-id',node.getAttribute('data-id').replace(/-(L[^-]+)-(L.+)$/,'-$2-$1'));},true),
   unknownAttribute:admittedMutation(root=>root.querySelector('.pele-node').setAttribute("unexpected","yes")),
   inlineStyle:raw.replace('<g class="pele-nodes">','<g class="pele-nodes" style="fill:red">'),
   activeElement:raw.replace('</svg>','<script>throw Error("unexpected")</script></svg>')
  };
  const rejected={},reasons={};for(const [key,mutated] of Object.entries(mutations)){try{A.validatedSvg(mutated,p);rejected[key]=false;}catch(error){rejected[key]=true;reasons[key]=error.message;}}
  const untouched=A.validatedSvg(admittedMutation(()=>{}),p).controls.size;
  const reordered=admittedMutation(root=>{for(const parent of root.querySelectorAll('.pele-clusters,.pele-nodes'))for(const child of [...parent.children].reverse())parent.append(child);});
  const reorderedCount=A.validatedSvg(reordered,p).controls.size;
  // Repeated endpoint buckets (the fixture's first edge twice, with different verdicts) are separate arrows,
  // each found by its record's key and never by the occurrence number in its drawn id.
  const arrowKeys=[...canvas.querySelectorAll('[data-trellis-edge-id]')].map(node=>node.getAttribute("data-trellis-edge-id"));
  const arrowIds=[...canvas.querySelectorAll('.pele-edge-label')].map(node=>node.getAttribute("data-id").replace(/^[0-9]+-/,""));
  const recordKeys=view.edges.map(edge=>A.edgeKey(edge));
  let withoutDisplay;try{A.project(view,{showTests:true});}catch(error){withoutDisplay=error.message;}
  const noDisplay=await A.render(document.createElement("div"),view,{showTests:true});
  const originalPele=window.pele;let failure;
  try { window.pele={render:()=>({svg:mutations.labelLocalShape})};failure=await A.render(canvas,view,options); }
  finally { window.pele=originalPele; }
  await new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)));
  return{ready:ready.status,modules:p.view.modules.length,edges:p.edges.size,rejected,reasons,untouched,reorderedCount,arrowKeys,arrowIds,recordKeys,withoutDisplay,noDisplay:[noDisplay.status,noDisplay.error?.message],expectedControls:p.nodes.size+p.edges.size,selectionCount:selected.length,badges:productionRects.length,chipTexts,edgeLabels,badgeGeometry,failure:failure.status,interactiveAfterFailure:canvas.querySelectorAll('[role="button"]').length,svgAfterFailure:canvas.querySelectorAll('svg').length,violations};
 },view);
 assert.equal(result.error,undefined,result.error);assert.equal(result.ready,"ready");assert.equal(result.modules,40);assert.equal(result.edges,6);assert.equal(result.selectionCount,2);assert.equal(result.badges,5,"one error chip per drawn band");assert.deepEqual(result.chipTexts.slice(0,2),[["↑78","error"],["↓23456","neutral"]]);assert.ok(result.edgeLabels.some(([drawn,label])=>drawn==="↑7"&&label==="7 production sites · upward\ndeviation"),JSON.stringify(result.edgeLabels));assert.ok(result.edgeLabels.some(([drawn])=>drawn==="↓23 excepted"));assert.equal(result.badgeGeometry,true);for(const [name,rejected]of Object.entries(result.rejected))assert.equal(rejected,true,name);
 for(const name of ["peleConnectors","peleConnectorOnly","peleLabelOnly","missingConnector","duplicateConnector","missingLabel","duplicateLabel","labelLocalShape","connectorLocalShape","wrongEndpoint"])assert.equal(result.reasons[name],"Missing or unexpected Architecture edge",name);
 assert.equal(result.untouched,result.expectedControls,"Pele's own drawing is admitted, with the adapter's arrows");assert.equal(result.reorderedCount,result.expectedControls);
 assert.equal(result.arrowKeys.length,6);assert.deepEqual([...result.arrowKeys].sort(),[...result.recordKeys].sort());assert.equal(new Set(result.arrowKeys).size,6);
 assert.equal(new Set(result.arrowIds).size,5,"two of the six arrows share their endpoints");
 assert.equal(result.withoutDisplay,"Architecture display unavailable");assert.deepEqual(result.noDisplay,["error","Architecture display unavailable"]);assert.equal(result.failure,"error");assert.equal(result.interactiveAfterFailure,0);assert.equal(result.svgAfterFailure,0);assert.deepEqual(result.violations,[]);assert.deepEqual(errors,[]);
 }finally{if(browser)await browser.close();await new Promise(resolve=>server.close(resolve));}
});
test("AC9, AC10: an action stat is a focusable sibling of its band, moves with it and activates the callback",async()=>{
 const routes={"/vendor.js":"web/vendor/pele-1.10.0-trellis.2.js","/pele-svg.js":"web/pele-svg.js","/routing.js":"web/architecture-routing.js","/adapter.js":"web/architecture-pele.js"};
 const server=http.createServer((req,res)=>{if(routes[req.url]){res.setHeader("Content-Type","text/javascript; charset=utf-8");res.end(fs.readFileSync(path.join(root,routes[req.url])));}else{res.setHeader("Content-Type","text/html; charset=utf-8");res.end('<!doctype html><div id="canvas"></div><script src="/vendor.js"></script><script src="/pele-svg.js"></script><script src="/routing.js"></script><script src="/adapter.js"></script>');}});
 await new Promise(resolve=>server.listen(0,"127.0.0.1",resolve));let browser;
 try{browser=await chromium.launch();const page=await browser.newPage({viewport:{width:1440,height:1000}});await page.goto(`http://127.0.0.1:${server.address().port}`);
 const result=await page.evaluate(async view=>{
  const canvas=document.querySelector("#canvas"),A=window.TrellisArchitecturePele,opened=[],toggled=[],selected=[];
  const options={width:1200,title:"web · ordered layers",display:{layers:new Set(view.layers.map(l=>l.name)),expanded:()=>false,edge:()=>true,unassigned:null,entries:()=>[],summary:()=>[]},
   layerStats:layer=>layer.name==="Store"?[{text:"↑7 deviation · Core",label:"7 upward production reference sites from Store to Core · deviation · open in Deviations",tone:"error",action:{sourceLayer:"Store",destinationLayer:"Core"}},{text:"↓3",label:"3 downward production sites",tone:"neutral"}]:layer.name==="Services"?[{text:"→ server · 2 routes",label:"client end: 2 routes to server",tone:"neutral",action:{interface:"http:web->daemon"}}]:[],
   onToggleLayer:layer=>toggled.push(layer.name),onOpenDeviation:action=>opened.push(action),onSelectInterface:action=>selected.push(action)};
  const ready=await A.render(canvas,view,options);if(ready.status!=="ready")return{error:ready.error?.message};
  const button=canvas.querySelector('[data-trellis-layer-action="Store"]'),band=canvas.querySelector('[data-trellis-layer-id="Store"]');
  const bandBox=band.querySelector(":scope > rect").getBoundingClientRect(),box=button.getBoundingClientRect();
  button.dispatchEvent(new KeyboardEvent("keydown",{key:"Enter",bubbles:true}));button.dispatchEvent(new KeyboardEvent("keydown",{key:" ",bubbles:true}));button.dispatchEvent(new MouseEvent("click",{bubbles:true}));
  const stub=canvas.querySelector('[data-trellis-layer-action="Services"]');
  for(const event of [new KeyboardEvent("keydown",{key:"Enter",bubbles:true}),new KeyboardEvent("keydown",{key:" ",bubbles:true}),new MouseEvent("click",{bubbles:true})])stub.dispatchEvent(event);
  return{selected,openedAfterStub:opened.length,sibling:band.nextElementSibling===button,role:button.getAttribute("role"),tabindex:button.getAttribute("tabindex"),label:button.getAttribute("aria-label"),inside:band.contains(button),
   within:box.left>=bandBox.left-1&&box.right<=bandBox.right+1&&box.top>=bandBox.top-1&&box.bottom<=bandBox.bottom+1,opened,toggled,plainStat:band.querySelectorAll("text.trellis-architecture-stat").length,bands:[...ready.bands.keys()],frame:ready.bands.get("Store")};
 },fixture());
 assert.equal(result.error,undefined,result.error);assert.equal(result.sibling,true);assert.equal(result.inside,false);assert.equal(result.role,"button");assert.equal(result.tabindex,"0");
 assert.match(result.label,/open in Deviations$/);assert.equal(result.within,true,"the badge sits inside its band's frame");
 assert.deepEqual(result.opened,Array(3).fill({sourceLayer:"Store",destinationLayer:"Core"}));assert.deepEqual(result.toggled,[]);
 assert.deepEqual(result.selected,Array(3).fill({interface:"http:web->daemon"}));assert.equal(result.openedAfterStub,3,"an interface action never calls onOpenDeviation");
 assert.equal(result.plainStat,1,"a stat without an action stays in the band");assert.ok(result.bands.includes("Store"));assert.ok(result.frame.w>0&&result.frame.h>0);
 }finally{if(browser)await browser.close();await new Promise(resolve=>server.close(resolve));}
});
