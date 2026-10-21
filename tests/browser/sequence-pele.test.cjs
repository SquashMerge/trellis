"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const {readFileSync} = require("node:fs");
const {createServer} = require("node:http");
const {resolve,join} = require("node:path");
const {chromium} = require("playwright");
const ROOT = resolve(__dirname,"../..");
const files = {"/vendor/pele-1.6.0.js":"web/vendor/pele-1.6.0.js","/sequence-pele.js":"web/sequence-pele.js","/sequence.js":"web/sequence.js","/style.css":"web/style.css"};
const fixture = language => JSON.parse(readFileSync(join(ROOT,`tests/fixtures/sequence-pele/${language}.json`)));
const html = '<!doctype html><html><head><link rel="stylesheet" href="/style.css"><script src="/vendor/pele-1.6.0.js" defer></script><script src="/sequence.js" defer></script><script src="/sequence-pele.js" defer></script></head><body><p id="state"></p><div id="sequence-diagram" class="sequence-scroll"></div></body></html>';
const policy = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; object-src 'none'; base-uri 'none'";
let server,base,browser,page,violations;
test.before(async()=>{
 server=createServer((req,res)=>{res.setHeader("Content-Security-Policy",policy); const file=files[req.url]; if(file){res.setHeader("Content-Type",file.endsWith("css")?"text/css; charset=utf-8":"text/javascript; charset=utf-8");res.end(readFileSync(join(ROOT,file)));return;} if(req.url==="/"){res.setHeader("Content-Type","text/html");res.end(html);return;} res.writeHead(404);res.end();});
 await new Promise(resolve=>server.listen(0,"127.0.0.1",resolve));base=`http://127.0.0.1:${server.address().port}`;
 browser=await chromium.launch();page=await browser.newPage();violations=[];
 page.on("console",msg=>{if(msg.type()==="error")violations.push(msg.text());});page.on("pageerror",e=>violations.push(e.message));
 await page.goto(base);await page.waitForFunction(()=>!!window.TrellisPeleSequence&&!!window.pele,null,{timeout:5000}).catch(error=>{throw new Error(`${error.message}; browser errors: ${violations.join(" | ")}`);});
});
test.after(async()=>{await browser?.close();await new Promise(resolve=>server?.close(resolve));});
async function render(view,options={}) {return page.evaluate(async ({view,options})=>{
 const container=document.querySelector("#sequence-diagram");
 const chosen=[];window.picks=chosen;window.current=true;
 const result=await TrellisPeleSequence.render(container,view,null,new Set(),{...options,isCurrent:()=>window.current,onSelect:step=>chosen.push(step)});
 return {status:result.status,reason:result.reason,notes:[...container.querySelectorAll(".pele-note[data-trellis-step-id]")].map(n=>({id:n.dataset.trellisStepId,callId:n.dataset.trellisCallId,text:n.textContent})),svg:!!container.querySelector("svg.pele.trellis-pele"),source:result.source};
 },{view,options});}
function steps(roots){return roots.flatMap(step=>[step,...steps(step.children||[]),...steps(step.alternate||[])]).filter(step=>!step.hidden);}
for(const language of ["javascript","rust","java","python"])test(`${language} producer DTO projects original ordered measured notes in Chromium`,async()=>{
 const view=fixture(language), result=await render(view);
 assert.equal(result.status,"rendered",`${language}: ${result.reason}`);assert.ok(result.svg);
 assert.equal(await page.locator("svg.pele.trellis-pele").evaluate(svg=>getComputedStyle(svg).display),"block");
 const original=steps(view.steps);const visible=result.notes.map(note=>original.find(step=>step.id===note.id));
 assert.equal(result.notes.length,original.length,`${language} note count`);
 assert.deepEqual(visible.map(step=>step.id),original.map(step=>step.id));
 assert.ok(!result.source.includes("->>"),"no invented target arrows");
 const note=page.locator(".pele-note[data-trellis-step-id]").first();await note.click();
 assert.equal(await page.evaluate(()=>window.picks[0]?.id),original[0].id);
 await note.focus();await page.keyboard.press("Enter");await page.keyboard.press("Space");
 assert.equal(await page.evaluate(()=>window.picks.length),3);
 await page.evaluate(()=>window.current=false);await note.click({force:true});assert.equal(await page.evaluate(()=>window.picks.length),3);
 assert.deepEqual(violations.filter(error=>/Content Security Policy|CSP/.test(error)),[]);
});
test("malformed SVG and duplicate IDs fall back without partial diagram",async()=>{
 const view=fixture("rust");const duplicate=structuredClone(view);duplicate.steps.push(structuredClone(duplicate.steps[0]));
 assert.equal((await render(duplicate)).status,"unsupported");
 const result=await page.evaluate(async view=>{const original=window.pele;window.pele={render:()=>({svg:'<svg onload="alert(1)"></svg>'})};
 try{return await TrellisPeleSequence.render(document.querySelector("#sequence-diagram"),view,null,new Set(),{});}finally{window.pele=original;}},view);
 assert.equal(result.status,"unsupported");
 assert.deepEqual(violations.filter(error=>/Content Security Policy|CSP/.test(error)),[]);
});

test("groups retain original selection, separate keyboard toggle and scroll/focus",async()=>{
 const view=fixture("rust"),group=structuredClone(view.steps[0]);group.id="group:original";group.kind="group";group.label="measured chain";group.callId=null;
 const entry=structuredClone(view.steps[0]);entry.id="group:entry";entry.callId="group:entry-call";
 group.children=[entry];view.steps=[group];
 const result=await render(view);assert.equal(result.status,"rendered",result.reason);
 const toggle=page.locator(".trellis-pele button.sequence-group-control");await toggle.waitFor();
 assert.equal(await toggle.getAttribute("aria-expanded"),"false");
 await page.locator('.pele-note[data-trellis-step-id="group:original"]').click({position:{x:10,y:10}});
 assert.equal(await page.evaluate(()=>window.picks.at(-1)?.id),group.id);
 await toggle.focus();await page.keyboard.press("Enter");
 assert.equal(await page.locator(".trellis-pele button.sequence-group-control").getAttribute("aria-expanded"),"true");
 assert.equal(await page.locator('.pele-note[data-trellis-step-id="group:entry"]').getAttribute("data-trellis-call-id"),entry.callId);
 assert.equal(await page.locator(".trellis-pele button.sequence-group-control").evaluate(e=>document.activeElement===e),true);
 await page.keyboard.press("Space");assert.equal(await page.locator(".trellis-pele button.sequence-group-control").getAttribute("aria-expanded"),"false");
});
test("untrusted labels stay text; invalid source and stale redraw cannot install",async()=>{
 const view=fixture("rust");view.steps[0].label='<img src=x onerror=alert(1)> :; \n🔒';
 const result=await render(view);assert.equal(result.status,"rendered",result.reason);
 assert.equal(await page.locator("#sequence-diagram img").count(),0);
 assert.ok(!result.source.includes("<img"));
 const bad=structuredClone(view);bad.steps[0].range.startLine=0;
 assert.equal((await render(bad)).status,"unsupported");
 assert.equal(await page.evaluate(async view=>{
   window.current=false;
   return (await TrellisPeleSequence.render(document.querySelector("#sequence-diagram"),view,null,new Set(),{isCurrent:()=>window.current})).status;
 },view),"stale");
 assert.deepEqual(violations.filter(error=>/Content Security Policy|CSP/.test(error)),[]);
});

test("nested loop/guard frames validate close order; changed frame fails closed",async()=>{
 const view=fixture("rust"),loop=structuredClone(view.steps[1]);loop.kind="loop";loop.id="nested-loop";loop.label="possible iterations";loop.alternate=[];
 const guard=structuredClone(view.steps[1]);guard.id="nested-guard";guard.label="only if measured condition";guard.alternate=[];
 guard.children=[structuredClone(view.steps[0])];guard.children[0].id="nested-call";
 loop.children=[guard];view.steps=[loop];
 const result=await render(view);assert.equal(result.status,"rendered",result.reason);
 assert.match(result.source,/loop possible iterations/);
 assert.match(result.source,/opt only if measured condition/);
 assert.deepEqual(await page.locator(".pele-frame").evaluateAll(frames=>frames.map(n=>n.querySelector(".pele-frame-kind")?.textContent)),["opt","loop"]);
 const failed=await page.evaluate(async view=>{
   const original=window.pele;
   window.pele={render:(...args)=>{const output=original.render(...args);return {...output,svg:output.svg.replace('class="pele-frame pele-frame-opt"','class="pele-frame pele-frame-alt"')};}};
   try{return (await TrellisPeleSequence.render(document.querySelector("#sequence-diagram"),view,null,new Set(),{})).status;}
   finally{window.pele=original;}
 },view);
 assert.equal(failed,"unsupported");
 assert.deepEqual(violations.filter(error=>/Content Security Policy|CSP/.test(error)),[]);
});

test("changed visible positional note text with intact IDs refuses source action",async()=>{
 const view=fixture("rust");
 const result=await page.evaluate(async view=>{
   const container=document.querySelector("#sequence-diagram");container.replaceChildren();window.picks=[];window.sourceActions=[];
   const original=window.pele;
   window.pele={render:(...args)=>{
     const output=original.render(...args);
     return {...output,svg:output.svg.replace("Static possible paths - target and timing unknown","Static possible paths - target and timing altered")};
   }};
   try{return await TrellisPeleSequence.render(container,view,step=>window.sourceActions.push(step),new Set(),{onSelect:step=>window.picks.push(step)});}
   finally{window.pele=original;}
 },view);
 assert.equal(result.status,"unsupported");assert.match(result.reason,/Changed Pele note text/);
 assert.equal(await page.locator("#sequence-diagram .pele-note").count(),0);
 assert.equal(await page.evaluate(()=>window.picks.length),0);
 assert.equal(await page.evaluate(()=>window.sourceActions.length),0);
});
test("failed async group expansion visibly installs complete legacy DTO",async()=>{
 const view=fixture("rust"),group=structuredClone(view.steps[0]),entry=structuredClone(view.steps[0]);
 group.id="unsupported-group";group.kind="group";group.callId=null;group.label="original complete group";
 entry.id="unsupported-child";delete entry.hidden;group.children=[entry];view.steps=[group];
 const initial=await page.evaluate(async view=>{
   const container=document.querySelector("#sequence-diagram");container.replaceChildren();
   const expanded=new Set();document.querySelector("#state").textContent="Pele rendered";
   const options={isCurrent:()=>true,onUnsupported:reason=>{
     TrellisSequence.render(container,view,null,expanded,options);
     document.querySelector("#state").textContent=`Pele fell back: ${reason}`;
   }};
   return (await TrellisPeleSequence.render(container,view,null,expanded,options)).status;
 },view);
 assert.equal(initial,"rendered");
 await page.locator('.trellis-pele button.sequence-group-control').click();
 await page.waitForFunction(()=>document.querySelector("#state")?.textContent?.includes("Pele fell back"));
 assert.match(await page.locator("#state").textContent(),/Unsupported sequence step/);
 assert.equal(await page.locator("#sequence-diagram svg.pele").count(),0);
 assert.equal(await page.locator("#sequence-diagram svg").count(),1);
 assert.match(await page.locator("#sequence-diagram").innerText(),/original complete group/);
 assert.ok(await page.locator('[data-source-step-id="unsupported-child"]').count(),"expanded legacy view retains original child");
});
