"use strict";
const test=require("node:test"),assert=require("node:assert/strict"),fs=require("node:fs"),path=require("node:path"),http=require("node:http");
const {chromium}=require("playwright");
const {fixture}=require("../fixtures/pele-1.10.0-trellis.2/architecture-fixture.cjs");
const root=path.resolve(__dirname,"../..");
test("Architecture patched Pele admission is atomic, non-positional and CSP-safe",async()=>{
 const routes={"/vendor.js":"web/vendor/pele-1.10.0-trellis.2.js","/adapter.js":"web/architecture-pele.js"};
 const server=http.createServer((req,res)=>{res.setHeader("Content-Security-Policy","default-src 'none'; script-src 'self'; style-src 'self'; font-src 'self'; connect-src 'self'; img-src 'self'");if(routes[req.url]){res.setHeader("Content-Type","text/javascript; charset=utf-8");res.end(fs.readFileSync(path.join(root,routes[req.url])));}else{res.setHeader("Content-Type","text/html; charset=utf-8");res.end('<!doctype html><div id="canvas"></div><script src="/vendor.js"></script><script src="/adapter.js"></script>');}});
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
  const p=A.project(view,{showTests:true}),renderOptions={fontSize:14,padding:8,responsive:false,links:false,images:false};const original=pele.render,raw=original(p.source,renderOptions).svg;
  function admittedMutation(change){const root=A.validatedSvg(raw,p).root;change(root);let value=new XMLSerializer().serializeToString(root);return value.replace('class="pele pele-block"','class="pele pele-block" '+raw.match(/style="[^"]*"/)[0]).replaceAll('rx="var(--_r)"','style="rx:var(--_r)"');}
  const mutations={
   missingNode:admittedMutation(root=>root.querySelector('.pele-node').remove()),
   duplicateNode:admittedMutation(root=>{const node=root.querySelector('.pele-node');node.parentElement.append(node.cloneNode(true));}),
   nodePlaceholder:admittedMutation(root=>root.querySelector('.pele-node text').textContent="Changed"),
   nodeClass:admittedMutation(root=>root.querySelector('.pele-node').setAttribute("class","pele-node changed")),
   nodeLocalShape:admittedMutation(root=>{const nodes=root.querySelectorAll('.pele-node');nodes[1].append(nodes[0].querySelector('rect'));}),
   missingConnector:admittedMutation(root=>root.querySelector('.pele-edge').remove()),
   duplicateConnector:admittedMutation(root=>{const node=root.querySelector('.pele-edge');node.parentElement.append(node.cloneNode(true));}),
   missingLabel:admittedMutation(root=>root.querySelector('.pele-edge-label').remove()),
   duplicateLabel:admittedMutation(root=>{const node=root.querySelector('.pele-edge-label');node.parentElement.append(node.cloneNode(true));}),
   labelLocalShape:admittedMutation(root=>{const labels=root.querySelectorAll('.pele-edge-label');labels[1].append(labels[0].querySelector('rect'));}),
   connectorLocalShape:admittedMutation(root=>{const edges=root.querySelectorAll('.pele-edge');edges[1].append(edges[0].querySelector('.pele-marker'));}),
   wrongEndpoint:admittedMutation(root=>{for(const node of root.querySelectorAll('.pele-edge,.pele-edge-label'))node.setAttribute('data-id',node.getAttribute('data-id').replace(/-(L[^-]+)-(L.+)$/,'-$2-$1'));}),
   unknownAttribute:admittedMutation(root=>root.querySelector('.pele-node').setAttribute("unexpected","yes")),
   inlineStyle:raw.replace('<g class="pele-nodes">','<g class="pele-nodes" style="fill:red">'),
   activeElement:raw.replace('</svg>','<script>throw Error("unexpected")</script></svg>')
  };
  const rejected={};for(const [key,mutated] of Object.entries(mutations)){try{A.validatedSvg(mutated,p);rejected[key]=false;}catch{rejected[key]=true;}}
  const reordered=admittedMutation(root=>{for(const parent of root.querySelectorAll('.pele-clusters,.pele-nodes,.pele-edges,.pele-edge-labels'))for(const child of [...parent.children].reverse())parent.append(child);});
  const reorderedCount=A.validatedSvg(reordered,p).controls.size;
  // Repeated endpoint buckets need distinct native associations but none has a
  // semantic dependency on the renderer's occurrence number.
  let occurrenceSerial=100;const idMap=new Map();const perturbed=reordered.replace(/data-id="([1-9][0-9]*-L[^"]+)"/g,(all,id)=>{if(!idMap.has(id))idMap.set(id,`${occurrenceSerial++}-${id.replace(/^[0-9]+-/,"")}`);return`data-id="${idMap.get(id)}"`;});
  const occurrenceCount=A.validatedSvg(perturbed,p).controls.size;
  const originalPele=window.pele;let failure;
  try { window.pele={render:()=>({svg:mutations.labelLocalShape})};failure=await A.render(canvas,view,options); }
  finally { window.pele=originalPele; }
  await new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)));
  return{ready:ready.status,modules:p.view.modules.length,edges:p.edges.size,rejected,reorderedCount,occurrenceCount,expectedControls:p.nodes.size+p.edges.size,selectionCount:selected.length,badges:productionRects.length,chipTexts,edgeLabels,badgeGeometry,failure:failure.status,interactiveAfterFailure:canvas.querySelectorAll('[role="button"]').length,svgAfterFailure:canvas.querySelectorAll('svg').length,violations};
 },view);
 assert.equal(result.error,undefined,result.error);assert.equal(result.ready,"ready");assert.equal(result.modules,40);assert.equal(result.edges,6);assert.equal(result.selectionCount,2);assert.equal(result.badges,5,"one error chip per drawn band");assert.deepEqual(result.chipTexts.slice(0,2),[["↑78","error"],["↓23456","neutral"]]);assert.ok(result.edgeLabels.some(([drawn,label])=>drawn==="↑7"&&label==="7 production sites · upward\ndeviation"),JSON.stringify(result.edgeLabels));assert.ok(result.edgeLabels.some(([drawn])=>drawn==="↓23 excepted"));assert.equal(result.badgeGeometry,true);for(const [name,rejected]of Object.entries(result.rejected))assert.equal(rejected,true,name);assert.equal(result.reorderedCount,result.expectedControls);assert.equal(result.occurrenceCount,result.expectedControls);assert.equal(result.failure,"error");assert.equal(result.interactiveAfterFailure,0);assert.equal(result.svgAfterFailure,0);assert.deepEqual(result.violations,[]);assert.deepEqual(errors,[]);
 }finally{if(browser)await browser.close();await new Promise(resolve=>server.close(resolve));}
});
