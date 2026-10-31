"use strict";
const test = require("node:test"), assert = require("node:assert/strict"), fs = require("node:fs"), vm = require("node:vm");
const asset = "web/vendor/pele-1.10.0-trellis.2.js";
function setup(extra = {}) {
  const window = {}, context = {window,TextEncoder,...extra};
  vm.runInNewContext(fs.readFileSync("web/treemap-pele.js","utf8"),context);
  const vendor = vm.createContext({}); vm.runInContext(fs.readFileSync(asset,"utf8"),vendor);
  return {adapter:window.TrellisTreemapPele,pele:vendor.pele};
}
const file = (sourceSetId,path,physicalLines=20,extra={}) => ({sourceSetId,language:"rust",path,physicalLines,fileRole:"production",roleCounts:{production:1,test:0},declarationCount:1,metrics:{cognitive:2},...extra});
function fixture() { return [file("one","src/a/same.rs",30),file("one","src/b/same.rs",70),file("two","src/a/same.rs",10),file("one","empty.rs",0)]; }
function native(pele,p) { return pele.render(p.source,{config:p.config,fontSize:14,padding:8,responsive:false,links:false,images:false}).svg; }

test("exact opaque projection groups nested source-set directories and preserves duplicate basenames",()=>{
  const {adapter,pele}=setup(), files=fixture(), p=adapter.project(files);
  assert.equal(p.source, 'treemap-beta\n"D0"\n  "D1"\n    "D2"\n      "F3": 30\n    "D4"\n      "F5": 70\n"D6"\n  "D7"\n    "D8"\n      "F9": 10\n');
  assert.equal(p.total,110); assert.equal(p.zeroArea[0],files[3]);
  const records=[...p.nodes.values()].filter(n=>n.kind==="file");
  assert.equal(records.length,3); assert.equal(records[0].record,files[0]);
  assert.equal(new Set(records.map(n=>n.key)).size,3);
  assert.equal([...p.nodes.values()].find(n=>n.key===JSON.stringify(["one","src"])).total,100);
  assert.equal(adapter.project(files.slice().reverse()).source,p.source);
  const raw=native(pele,p);
  assert.match(raw,/class="pele pele-treemap"/);
  for (const [id,item] of p.nodes) {
    assert.equal((raw.match(new RegExp(`data-id="${id}"`,"g"))||[]).length,1);
    assert.match(raw,new RegExp(`class="pele-node pele-${item.kind==="file"?"leaf":"section"}" data-id="${id}"`));
  }
});
test("only positive physical lines create geometry; caller filtering controls included totals",()=>{
  const {adapter,pele}=setup(), files=fixture(), p=adapter.project(files.slice(0,1));
  assert.equal(p.total,30); assert.equal([...p.nodes.values()].filter(n=>n.kind==="file").length,1);
  const empty=adapter.project([files[3]]); assert.equal(empty.nodes.size,0); assert.equal(empty.total,0);
  assert.match(native(pele,empty),/width="656" height="656"/);
  const tiny=adapter.project([file("one","tiny.rs",1),file("one","large.rs",100000000)]);
  const raw=native(pele,tiny); for(const id of tiny.nodes.keys()) assert.ok(raw.includes(`data-id="${id}"`));
  assert.throws(()=>adapter.project([files[0],files[0]]),/duplicate/);
  for(const physicalLines of [-1,0.5,NaN,Number.MAX_SAFE_INTEGER+1]) assert.throws(()=>adapter.project([file("one","a",physicalLines)]),/Invalid/);
});
test("repository strings never enter Mermaid grammar, identity or metric role are never guessed",()=>{
  const {adapter}=setup(), hostile='x"]\nclassDef bad fill:red;\n<script>&', record=file(hostile,`nested/${hostile}`,12,{language:"java"});
  const p=adapter.project([record],{fileValue:()=>"</text><image href=x>",fileBand:()=>3});
  assert.ok(!p.source.includes(hostile)); assert.ok(!p.source.includes("image")); assert.ok(!p.source.includes("classDef"));
  const leaf=[...p.nodes.values()].find(n=>n.kind==="file");
  assert.equal(leaf.record,record); assert.equal(leaf.key,JSON.stringify([hostile,"java",record.path]));
  assert.ok(leaf.label.includes("</text>")); assert.equal(leaf.band,3);
  assert.equal([...adapter.project([record],{moduleValue:()=>7}).nodes.values()].find(n=>n.kind==="file").label.endsWith(" · 7"),true);
});
test("natural scope size reaches admitted 400-unit native config without class font sizes",()=>{
  const {adapter,pele}=setup(), files=Array.from({length:1600},(_,i)=>file("repo",`f${i}.rs`,i+1));
  const p=adapter.project(files); assert.equal(p.size,4000); assert.equal(p.config.treemap.nodeWidth,400); assert.equal(p.config.treemap.nodeHeight,400);
  const raw=native(pele,p); assert.match(raw,/viewBox="0 0 4016 4016"/);
  assert.equal((raw.match(/class="pele-node pele-leaf"/g)||[]).length,1600);
  assert.ok(!p.source.includes("font-size")); assert.ok(!p.source.includes("classDef"));
});
test("generated source 512KiB, SVG 8MiB and native node bounds stay fail closed",()=>{
  let parses=0;
  const {adapter}=setup({DOMParser:class { parseFromString(){parses++;throw Error("unexpected parse");} }});
  assert.throws(()=>adapter.validatedSvg("x".repeat(8*1024*1024+1),adapter.project([])),/too large/); assert.equal(parses,0);
  const deep="x/".repeat(730)+"a.rs";
  assert.throws(()=>adapter.project([file("repo",deep)]),/source too large/);
  assert.throws(()=>adapter.project(Array.from({length:12001},(_,i)=>file("repo",`f${i}`))),/too many nodes/);
});

test("real pinned SVG map rejects mutations before controls; label fitting respects native slots and strict CSP",async t=>{
  if(!process.env.PLAYWRIGHT_BROWSERS_PATH) { t.skip("Playwright browser is provisioned by ./tools/verify"); return; }
  const {chromium}=require("./browser/node_modules/playwright"), browser=await chromium.launch();
  try {
    const page=await browser.newPage(), errors=[]; page.on("pageerror",error=>errors.push(error.message));
    await page.route("http://127.0.0.1:43989/**",route=>{
      const path=new URL(route.request().url()).pathname;
      const assets={"/pele.js":asset,"/treemap-pele.js":"web/treemap-pele.js"};
      if(assets[path]) return route.fulfill({contentType:"text/javascript; charset=utf-8",body:fs.readFileSync(assets[path])});
      return route.fulfill({contentType:"text/html; charset=utf-8",body:`<meta http-equiv="Content-Security-Policy" content="default-src 'none'; script-src 'self'; style-src 'self'"><script src="/pele.js"></script><script src="/treemap-pele.js"></script><div id="diagram"></div>`});
    });
    await page.goto("http://127.0.0.1:43989/");
    assert.deepEqual(errors,[],"fixture asset parse errors");
    assert.equal(await page.evaluate(()=>typeof window.pele?.render),"function","exact local Pele asset loaded");
    await page.evaluate(()=>document.addEventListener("securitypolicyviolation",event=>(window.cspViolations ||= []).push(event.violatedDirective)));
    const result=await page.evaluate(async files=>{
      const adapter=window.TrellisTreemapPele, projection=adapter.project(files), actual=window.pele.render(projection.source,{config:projection.config,fontSize:14,padding:8,responsive:false,links:false,images:false}).svg;
      const parse=svg=>new DOMParser().parseFromString(svg,"image/svg+xml").documentElement;
      const changed=change=>{const root=parse(actual);change(root);return new XMLSerializer().serializeToString(root);};
      const cases={
        missing:changed(root=>root.querySelector(".pele-leaf").remove()),
        extra:changed(root=>{const node=root.querySelector(".pele-leaf").cloneNode(true);node.setAttribute("data-id","invented");root.querySelector(".pele-leaves").append(node);}),
        duplicate:changed(root=>root.querySelector(".pele-leaves").append(root.querySelector(".pele-leaf").cloneNode(true))),
        type:changed(root=>root.querySelector(".pele-leaf").setAttribute("class","pele-node pele-section")),
        placeholder:changed(root=>root.querySelector(".pele-leaf text").textContent="changed"),
        structure:changed(root=>root.querySelector(".pele-leaf").append(document.createElementNS("http://www.w3.org/2000/svg","g"))),
        active:actual.replace("</svg>","<script>alert(1)</script></svg>"),
        font:changed(root=>root.querySelector(".pele-leaf text").setAttribute("font-size","100")),
        nodes:actual.replace("</svg>","<rect/>".repeat(12001)+"</svg>")
      };
      const rejected={};
      for(const [name,raw] of Object.entries(cases)) { try {adapter.validatedSvg(raw,projection);rejected[name]=false;}catch{rejected[name]=true;} }
      const validated=adapter.validatedSvg(actual,projection);
      const canvas=document.querySelector("#diagram"); let selected=null, current=true;
      const rendered=await adapter.render(canvas,files,{selectedKey:adapter.identity(files[0]),fileBand:()=>2,onSelectFile:file=>selected=file,isCurrent:()=>current});
      const node=[...canvas.querySelectorAll("[data-trellis-file-id]")].find(node=>node.getAttribute("data-trellis-file-id")===adapter.identity(files[0]));
      node.dispatchEvent(new KeyboardEvent("keydown",{key:"Enter",bubbles:true}));
      const keyboard=selected===files[0]; selected=null;
      current=false;node.dispatchEvent(new MouseEvent("click",{bubbles:true}));const staleClick=selected===null;current=true;
      adapter.select(canvas,adapter.identity(files[1]));
      const selection=[...canvas.querySelectorAll('[aria-pressed="true"]')].map(n=>n.getAttribute("data-trellis-file-id"));
      const fits=[...canvas.querySelectorAll(".pele-leaf text")].every(text=>text.getComputedTextLength()<=Number(text.parentElement.querySelector("rect").getAttribute("width"))-8+0.1);
      const before=canvas.firstElementChild;
      const adversarial={...files[0],path:'src/<img onerror="alert(1)">" &.rs'};
      const safe=await adapter.render(canvas,[adversarial],{fileValue:()=>'<script>alert(1)</script>'});
      const safeText=safe.status==="ready" && !canvas.querySelector("img,script,image") && canvas.querySelector(".pele-leaf title").textContent.includes(adversarial.path);
      const tinyFiles=[{...files[0],path:"tiny.rs",physicalLines:1},{...files[0],path:"huge.rs",physicalLines:100000000}];
      const tiny=await adapter.render(canvas,tinyFiles);
      const tinyNode=[...canvas.querySelectorAll(".pele-leaf")].find(n=>n.getAttribute("data-trellis-file-id")===adapter.identity(tinyFiles[0]));
      const tinyCovered=tiny.status==="ready" && !!tinyNode && !tinyNode.querySelector("text") && !!tinyNode.querySelector("title");
      const zero=await adapter.render(canvas,[files[3]]);
      const zeroCovered=zero.status==="ready" && canvas.querySelectorAll(".pele-leaf").length===0 && zero.projection.zeroArea.length===1;
      const nativeRender=window.pele.render;let release;
      window.pele={render:()=>new Promise(resolve=>release=resolve)};
      const pending=adapter.render(canvas,files,{isCurrent:()=>current});
      // Wait for font-ready microtasks to enter the render call.
      await Promise.resolve(); await Promise.resolve(); current=false;release({svg:actual});
      const stale=await pending; current=true;
      window.pele={render:()=>({svg:cases.missing})};
      const failed=await adapter.render(canvas,files,{onSelectFile:()=>{throw Error("invalid handler");}});
      window.pele={render:nativeRender};
      return {rejected,covered:validated.controls.size,expected:projection.nodes.size,status:rendered.status,keyboard,staleClick,selection,fits,safeText,tinyCovered,zeroCovered,stale:stale.status,failed:failed.status,cleared:!canvas.querySelector("svg")&&!!canvas.querySelector(".treemap-render-error"),previous:before.localName,csp:window.cspViolations || []};
    },fixture());
    for(const [name,rejected] of Object.entries(result.rejected)) assert.equal(rejected,true,name);
    assert.equal(result.covered,result.expected);
    assert.equal(result.status,"ready");
    for (const key of ["keyboard","staleClick","fits","safeText","tinyCovered","zeroCovered"]) assert.equal(result[key],true,key);
    assert.deepEqual(result.selection,[JSON.stringify(["one","rust","src/b/same.rs"])]);
    assert.equal(result.stale,"stale");assert.equal(result.failed,"error");assert.equal(result.cleared,true);assert.deepEqual(result.csp,[]);assert.deepEqual(errors,[]);
    const boundary=await page.evaluate(async seed=>{
      const adapter=window.TrellisTreemapPele, container=document.querySelector("#diagram");
      const outcomes=[];
      for (const length of [2998,2999,3500]) {
        const files=Array.from({length},(_,i)=>({...seed,path:`f${i}.rs`,physicalLines:1}));
        const projection=adapter.project(files);
        const raw=window.pele.render(projection.source,{config:projection.config,fontSize:14,padding:8,responsive:false,links:false,images:false}).svg;
        const native=new DOMParser().parseFromString(raw,"image/svg+xml").documentElement;
        const nativeCount=1+native.querySelectorAll("*").length;
        const missingTitles=[...native.querySelectorAll(".pele-node")].filter(node=>!node.querySelector("title")).length;
        const anticipated=nativeCount+missingTitles;
        const outcome=await adapter.render(container,files);
        const installed=container.querySelector("svg");
        outcomes.push({length,nativeCount,anticipated,status:outcome.status,error:outcome.error?.message,
          installedCount:installed ? 1+installed.querySelectorAll("*").length : 0,
          titles:installed?.querySelectorAll(".pele-node > title").length || 0,
          visibleError:!!container.querySelector(".treemap-render-error")});
      }
      return outcomes;
    },file("boundary","seed.rs",1));
    // Adjacent equal-area native scopes bracket the final-DOM limit. Titles
    // are real adapter decoration, not fake extra elements in a mock SVG.
    assert.equal(boundary[0].anticipated,11999);
    assert.equal(boundary[0].status,"ready");
    assert.equal(boundary[0].installedCount,boundary[0].anticipated);
    assert.equal(boundary[0].titles,2999);
    for (const row of boundary.slice(1)) {
      assert.ok(row.nativeCount<=12000,JSON.stringify(row));
      assert.ok(row.anticipated>12000,JSON.stringify(row));
      assert.equal(row.status,"error");assert.match(row.error,/decorated SVG has too many nodes/);
      assert.equal(row.installedCount,0);assert.equal(row.visibleError,true);
    }
    assert.equal(boundary[1].anticipated,12003);
    assert.equal(boundary[2].anticipated,14007);
  } finally {await browser.close();}
});
test("measured viewport sets a non-square native layout; tiny or crowded panels grow within bounds; root shows the checkout name (#425)",()=>{
  const {adapter,pele}=setup(), files=fixture();
  const p=adapter.project(files,{viewport:{width:913.6,height:577.2},rootLabel:id=>id==="one"?"trellis":id});
  assert.equal(p.width,913); assert.equal(p.height,577);
  assert.match(native(pele,p),/viewBox="0 0 929 593" width="929" height="593"/);
  const roots=[...p.nodes.values()].filter(n=>n.kind==="directory"&&!n.record.path);
  assert.deepEqual(roots.map(n=>n.label),["trellis · 100 lines","two · 10 lines"]);
  assert.equal(roots[0].full,"trellis · one · / · 100 physical file lines","exact source set stays in the full label");
  assert.equal(roots[1].full,"two · / · 10 physical file lines");
  assert.deepEqual({...adapter.layoutSize(3,{width:300,height:100})},{width:640,height:360},"readable minimum");
  // #451: a crowded panel keeps its width and grows only downward.
  assert.deepEqual({...adapter.layoutSize(1000,{width:1000,height:500})},{width:1000,height:2500},"grows only in height");
  assert.deepEqual({...adapter.layoutSize(12000,{width:1000,height:500})},{width:1000,height:4000},"never beyond 4000 px tall");
  assert.deepEqual({...adapter.layoutSize(10,{width:1000.6,height:500.4})},{width:1000,height:500},"an uncrowded panel is filled exactly");
  assert.deepEqual({...adapter.layoutSize(4,undefined)},{width:640,height:640});
});
