"use strict";
const test=require("node:test");const assert=require("node:assert/strict");
const {readFileSync,writeFileSync,rmSync}=require("node:fs");const {join}=require("node:path");
const {chromium}=require("playwright");const H=require("./sequence-harness.cjs");
const source='fn measured() {\n    first();\n    if condition() { then_arm(); } else { other_arm(); }\n    last();\n}\nfn first() {}\nfn condition() -> bool { true }\nfn then_arm() {}\nfn other_arm() {}\nfn last() {}\n';
test.before(async()=>H.setup(),{timeout:960000});test.after(async()=>H.cleanup());
async function published(sourceText,callback,filename="flow.rs") {
 const paths=H.fixture(sourceText,filename);let server,browser,context;
 try {
 const indexed=await H.run(H.binary,["index","--workspace",paths.workspace],{env:paths.env,timeout:120000});
 const pin=indexed.json().status.revision;
 server=await H.startDaemon(paths,pin.indexRevision);
 browser=await chromium.launch();context=await browser.newContext();const page=await context.newPage();
 const errors=[];page.on("pageerror",e=>errors.push(e.message));page.on("console",m=>{if(m.type()==="error")errors.push(m.text());});
 const coldStart=Date.now();await H.connect(page,server,paths.token);const coldAppMs=Date.now()-coldStart;
 const file=page.locator(".file-pick").filter({hasText:filename}).first();await file.waitFor();await file.click();
 const firstSelectionStart=Date.now();const method=page.locator(".method-pick").filter({hasText:"measured"}).first();await method.waitFor();await method.click();
 await page.waitForFunction(()=>document.querySelector("#sequence-state")?.textContent?.includes("Pele rendered")||document.querySelector("#sequence-state")?.textContent?.includes("fell back"));
 assert.match(await page.locator("#sequence-state").innerText(),/Pele rendered/);const firstSelectionMs=Date.now()-firstSelectionStart;
 const seed=await H.seedId(server,paths.token,"measured");
 const dto=await H.api(server.base,paths.token,"POST",H.scoped(server.rootKey,"sequence"),{seed,expectedRevision:pin,showAll:false});assert.equal(dto.status,200,JSON.stringify(dto.data));
 await callback({page,paths,server,pin,dto:dto.data,errors,coldAppMs,firstSelectionMs});
 assert.deepEqual(errors.filter(e=>/Content Security Policy|CSP/.test(e)),[]);
 } finally {await context?.close();await browser?.close();await H.stopDaemon(server);rmSync(paths.temp,{recursive:true,force:true,maxRetries:10,retryDelay:100});}
}
test("published Store sequence renders Pele and selects exact measured source by mouse and keyboard",{timeout:240000},async()=>published(source,async({page,pin,dto,paths})=>{
 assert.deepEqual(dto.revision,pin);const original=dto.steps.flatMap(s=>[s,...s.children,...s.alternate]);
 const measured=original.find(step=>step.kind==="call"&&step.callId);assert.ok(measured,JSON.stringify(dto));
 const target=page.locator(`.pele-note[data-trellis-step-id="${measured.id}"]`);await target.click();
 assert.equal(await target.getAttribute("data-trellis-call-id"),measured.callId);
 assert.match(await page.locator("#call-inspector").innerText(),new RegExp(measured.label.replace(/[.*+?^${}()|[\]\\]/g,"\\$&")));
 const sourceRequests=[];page.on("request",request=>{if(new URL(request.url()).pathname.endsWith("/source"))sourceRequests.push(request.url());});
 await page.locator("#inspector-open-source").click();
 await page.waitForFunction(()=>document.querySelector("#source-path")?.textContent?.includes("cached snapshot"));
 assert.equal(await page.locator("#source .source-line").count(),source.split("\n").length);
 assert.ok((await page.locator("#source").innerText()).includes("first();"));
 assert.match(await page.locator("#source-path").innerText(),/flow\.rs.*cached snapshot/);
 assert.equal(await page.locator("#source .source-line").evaluateAll(lines=>lines.map(line=>line.textContent.replace(/^\d+/,"")).join("\n")),source);
 assert.deepEqual(await page.locator("#source .source-line.highlight .line-number").allTextContents(),Array.from({length:measured.range.endLine-measured.range.startLine+1},(_,i)=>String(measured.range.startLine+i)));
 assert.ok(sourceRequests.length>=1,"explicit source action must make a real published request");
 assert.ok(sourceRequests.every(url=>{const query=new URL(url).searchParams;return query.get("indexGeneration")===pin.indexGeneration&&query.get("indexRevision")===String(pin.indexRevision)&&query.get("path")===measured.path;}));
 const other=original.find(step=>step.kind==="call"&&step.callId&&step.id!==measured.id&&step.label!==measured.label);assert.ok(other);
 const otherNote=page.locator(`.pele-note[data-trellis-step-id="${other.id}"]`);
 await otherNote.focus();await page.keyboard.press("Enter");
 assert.equal(await otherNote.getAttribute("data-trellis-call-id"),other.callId);
 assert.equal(await otherNote.getAttribute("aria-pressed"),"true");
 assert.match(await page.locator("#call-inspector").innerText(),new RegExp(other.label.replace(/[.*+?^${}()|[\]\\]/g,"\\$&")));
 await target.focus();await page.keyboard.press("Space");assert.equal(await target.getAttribute("aria-pressed"),"true");
 await otherNote.focus();await page.keyboard.press("Enter");assert.equal(await otherNote.getAttribute("aria-pressed"),"true");
 // The obsolete first call differs from the current selection. Even a held
 // DOM reference cannot change that selection or issue another source read.
 await page.evaluate(id=>{window.savedNote=document.querySelector(`[data-trellis-step-id="${id}"]`);status.revision={...status.revision,indexRevision:status.revision.indexRevision+1};},measured.id);
 const before=await page.locator("#call-inspector").innerText(), sourceCount=sourceRequests.length;
 await page.evaluate(()=>window.savedNote.dispatchEvent(new MouseEvent("click",{bubbles:true})));
 assert.equal(await page.locator("#call-inspector").innerText(),before);
 assert.equal(sourceRequests.length,sourceCount);
}));

test("method source Sequence uses the clicked declaration, not the left-tree selection, and Back restores source", {timeout:240000}, async () => published(source, async ({page, dto, pin}) => {
 // The tree remains on measured throughout source selection. The helper's
 // measured ID comes from its own source annotation, never the current seed.
 await page.locator("#method-source").click();
 await page.waitForFunction(() => document.querySelector("#source .source-line"));
 const action = page.getByRole("button", {name:/^Sequence for first ·/});
 await action.waitFor();
 const helperId = await action.getAttribute("data-symbol-id");
 assert.ok(helperId && helperId !== dto.seed.id);
 assert.match(await page.locator(".method-pick.selected").textContent(), /measured/);
 const before = await page.locator("#source").textContent();
 const highlighted = await page.locator("#source .highlight .line-number").allTextContents();
 const responsePromise = page.waitForResponse(response => new URL(response.url()).pathname.endsWith("/sequence") && response.request().postDataJSON()?.seed === helperId);
 await action.click();
 const response = await responsePromise, selected = await response.json();
 assert.equal(response.status(), 200, JSON.stringify(selected));
 assert.equal(selected.seed.id, helperId); assert.equal(selected.seed.name, "first"); assert.deepEqual(selected.revision, pin);
 await page.waitForFunction(id => new URLSearchParams(location.hash.slice("#sequence?".length)).get("sid") === id && document.querySelector("#sequence-state")?.textContent.includes("Pele rendered"), helperId);
 await page.goBack();
 await page.waitForFunction(text => document.querySelector("#source")?.textContent === text && !document.querySelector("#workspace-source-panel")?.hidden, before);
 assert.deepEqual(await page.locator("#source .highlight .line-number").allTextContents(), highlighted);
 await page.waitForFunction(id => document.activeElement?.dataset.symbolId === id, helperId);
 assert.match(await page.locator("#source-path").textContent(), /flow\.rs.*cached snapshot/);
 assert.match(await page.locator(".method-pick.selected").textContent(), /measured/, "Back restores the original diagram selection as well as source");
}));

const rovingSource = `function measured() { helper_0(); }
function helper_0() {} function helper_peer() {}
${Array.from({length:30},(_,i)=>`function helper_${i+1}() {}`).join("\n")}
`;
test("published source roves one tabbable declaration action and invokes exact same-line identity by keyboard", {timeout:240000}, async () => published(rovingSource, async ({page, pin}) => {
 await page.locator("#method-source").click();
 await page.waitForFunction(() => document.querySelectorAll("#source .source-diagram-action").length === 33);
 const sourcePane = page.locator("#source");
 const tabbable = page.locator('#source .source-diagram-action[tabindex="0"]');
 assert.equal(await tabbable.count(), 1, "many declarations must not add many source tab stops");
 await sourcePane.focus();await page.keyboard.press("Home");await page.keyboard.press("ArrowDown");
 assert.equal(await sourcePane.getAttribute("data-navigation-line"), "2");
 assert.equal(await tabbable.count(), 1);
 const sameLine = page.locator("#source .source-line.navigation-active-line .source-diagram-action");
 assert.equal(await sameLine.count(), 2, "both declarations on the line remain reachable");
 const firstId = await sameLine.nth(0).getAttribute("data-symbol-id"), exactId = await sameLine.nth(1).getAttribute("data-symbol-id");
 assert.ok(firstId && exactId && firstId !== exactId);
 await page.keyboard.press("Tab");
 assert.equal(await page.evaluate(() => document.activeElement?.dataset.symbolId), firstId, "Tab from source enters only its active line action");
 assert.equal(await tabbable.evaluate(node => getComputedStyle(node).opacity), "1", "active-line action is revealed");
 await page.keyboard.press("ArrowRight");
 assert.equal(await page.evaluate(() => document.activeElement?.dataset.symbolId), exactId);
 assert.equal(await tabbable.count(), 1);
 assert.equal(await tabbable.evaluate(node => getComputedStyle(node).outlineStyle), "solid");
 const before = await sourcePane.textContent();
 const responsePromise = page.waitForResponse(response => new URL(response.url()).pathname.endsWith("/sequence") && response.request().postDataJSON()?.seed === exactId);
 await page.keyboard.press("Enter");
 const response = await responsePromise, selected = await response.json();
 assert.equal(response.status(), 200, JSON.stringify(selected));assert.equal(selected.seed.id, exactId);assert.deepEqual(selected.revision, pin);
 await page.waitForFunction(id => new URLSearchParams(location.hash.slice("#sequence?".length)).get("sid") === id && document.querySelector("#sequence-state")?.textContent.includes("Pele rendered"), exactId);
 await page.goBack();
 await page.waitForFunction(({id,text}) => document.activeElement?.dataset.symbolId === id && document.querySelector("#source")?.textContent === text, {id:exactId,text:before});
 assert.equal(await sourcePane.getAttribute("data-navigation-line"), "2");assert.equal(await tabbable.count(), 1);
 assert.equal(await tabbable.getAttribute("data-symbol-id"), exactId, "Back restores the exact roving action, not a second tab stop");
 await page.keyboard.press("ArrowLeft");assert.equal(await page.evaluate(() => document.activeElement?.dataset.symbolId), firstId);
 await page.keyboard.press("ArrowDown");
 assert.equal(await sourcePane.evaluate(node => document.activeElement === node), true);
 assert.equal(await sourcePane.getAttribute("data-navigation-line"), "3");assert.equal(await tabbable.count(), 1);
 await page.keyboard.press("Tab");assert.equal(await page.evaluate(() => document.activeElement?.matches('.source-line.navigation-active-line .source-diagram-action')), true);
}, "roving.js"));

module.exports={published};

const jsSource="async function measured(path, value) {\n  console.log(writeDb());\n  if (!valid(path)) throw new Error(\"invalid\");\n  const tmp = tempName(path);\n  try {\n    await fs.writeFile(tmp, encode(value));\n    await fs.rename(tmp, path);\n    return path;\n  } catch (err) {\n    await fs.unlink(tmp);\n    throw err;\n  } finally {\n    release(path);\n  }\n}\n";

test("published JavaScript nested try/handler/exit retains order and original source",{timeout:240000},async()=>published(jsSource,async({page,dto,pin})=>{
 const steps=(function walk(xs){return xs.flatMap(s=>[s,...walk(s.children||[]),...walk(s.alternate||[])]);})(dto.steps);
 assert.ok(steps.some(s=>s.kind==="try"),"actual JS producer must admit a try");
 assert.ok(steps.some(s=>s.label.includes("catch: only if")),"handler remains distinct");
 assert.ok(steps.some(s=>s.label.includes("finally: on both")),"cleanup remains distinct");
 assert.ok(steps.some(s=>s.kind==="throw"));assert.ok(steps.some(s=>s.kind==="return"));
 const visible=steps.filter(s=>!s.hidden);
 const notes=await page.locator(".pele-note[data-trellis-step-id]").evaluateAll(ns=>ns.map(n=>({id:n.dataset.trellisStepId,text:n.textContent,callId:n.dataset.trellisCallId})));
 assert.deepEqual(notes.map(n=>n.id),visible.map(s=>s.id),"note order follows original control/alternate/handler order");
 for(const kind of ["try","throw","return"]){const step=visible.find(s=>s.kind===kind);assert.ok(step,kind);assert.ok(notes.find(n=>n.id===step.id)?.text.includes(kind));}
 const handler=visible.find(s=>s.label.includes("catch: only if"));assert.ok(handler);
 const target=page.locator(`.pele-note[data-trellis-step-id="${handler.id}"]`);await target.focus();await page.keyboard.press("Enter");
 assert.equal(await target.getAttribute("aria-pressed"),"true");assert.match(await page.locator("#call-inspector").innerText(),/catch: only if/);
 const requests=[];page.on("request",r=>{if(new URL(r.url()).pathname.endsWith("/source"))requests.push(r.url());});
 await page.locator("#inspector-open-source").click();await page.waitForFunction(()=>document.querySelector("#source-path")?.textContent?.includes("cached snapshot"));
 assert.ok(requests.some(raw=>{const q=new URL(raw).searchParams;return q.get("path")===handler.path&&q.get("indexGeneration")===pin.indexGeneration&&q.get("indexRevision")===String(pin.indexRevision);}));
 assert.equal(await page.locator("#source .source-line.highlight .line-number").first().textContent(),String(handler.range.startLine));
 assert.equal(await page.locator(".pele-message").count(),0);
 assert.ok(dto.hiddenSteps>0,"producer-hidden incidental call required");
 assert.match(await page.locator("#sequence-state").textContent(),/incidental steps hidden/);
 const serverRequests=[];page.on("request",r=>{if(new URL(r.url()).pathname.endsWith("/sequence"))serverRequests.push(r.postDataJSON());});
 await page.locator("#all-steps").check();
 await page.waitForFunction(()=>document.querySelector("#sequence-state")?.textContent?.includes("Pele rendered")&&document.querySelector("#sequence-state")?.textContent?.includes("0 incidental steps hidden"));
 assert.ok(serverRequests.some(body=>body.showAll===true&&body.expectedRevision.indexGeneration===pin.indexGeneration&&body.expectedRevision.indexRevision===pin.indexRevision));
 assert.ok((await page.locator(".pele-note[data-trellis-step-id]").allTextContents()).some(text=>text.includes("console.log")));
 // The same published view remains navigable with a narrow scroll canvas and
 // keyboard-visible focus. No library-injected controls or inline CSS.
 await page.setViewportSize({width:540,height:780});
 const narrow=await page.locator("#sequence-diagram").evaluate(node=>({overflow:getComputedStyle(node).overflowX,scroll:node.scrollWidth,client:node.clientWidth}));
 assert.equal(narrow.overflow,"auto");assert.ok(narrow.scroll>=narrow.client);
 const first=page.locator(".pele-note[data-trellis-step-id]").first();await first.focus();
 assert.equal(await first.evaluate(n=>document.activeElement===n),true);
 await page.keyboard.press("Tab");
 assert.equal(await page.evaluate(()=>document.activeElement?.matches(".pele-note[data-trellis-step-id]")),true);
 assert.equal(await page.evaluate(()=>getComputedStyle(document.activeElement).outlineStyle),"solid");
},"flow.js"));

// A source import is only a syntax-level candidate for a module binding, not
// a proven callee. The published app must retain its original call witness.
const candidateSource = "import {parse as candidateParse} from 'candidate-parser';\nfunction measured(input) {\n  return candidateParse(input);\n}\n";
test("published import candidate keeps partial provenance and original source",{timeout:240000},async()=>published(candidateSource,async({page,dto,pin})=>{
 const candidate=dto.participants.find(p=>p.kind==="import"&&p.label==="candidate-parser");
 assert.ok(candidate,JSON.stringify(dto.participants));
 assert.match(candidate.identification,/Static import module/);
 assert.match(candidate.identification,/not objects or resolved implementations/);
 const step=dto.steps.find(s=>s.kind==="call"&&s.label==="candidateParse");
 assert.ok(step?.callId,"candidate selection must be backed by an indexed call");
 assert.equal(step.target,undefined,"a candidate module must not become a verified call target");
 assert.equal(step.path,"flow.js");
 assert.ok(!step.resolution||step.resolution.kind!=="verified","candidate is not verified dispatch");
 assert.equal(await page.locator("#sequence-diagram .pele-message").count(),0,"candidate cannot become an inferred arrow");
 const note=page.locator(`#sequence-diagram .pele-note[data-trellis-step-id="${step.id}"]`);
 assert.equal(await note.getAttribute("data-trellis-call-id"),step.callId);
 await note.focus();await page.keyboard.press("Enter");
 assert.equal(await note.getAttribute("aria-pressed"),"true");
 assert.equal(await page.locator("#inspector-title").getAttribute("title"),step.label);
 assert.match(await page.locator("#inspector-target").innerText(),/no verified target/);
 assert.match(await page.locator("#inspector-detail").innerText(),/candidateParse/);
 const requests=[];page.on("request",r=>{if(new URL(r.url()).pathname.endsWith("/source"))requests.push(r.url());});
 await page.locator("#inspector-open-source").click();
 await page.waitForFunction(()=>document.querySelector("#source-path")?.textContent?.includes("cached snapshot"));
 assert.ok(requests.some(raw=>{const q=new URL(raw).searchParams;return q.get("path")===step.path&&q.get("indexGeneration")===pin.indexGeneration&&q.get("indexRevision")===String(pin.indexRevision);}));
 assert.deepEqual(await page.locator("#source .source-line.highlight .line-number").allTextContents(),[String(step.range.startLine)]);
 assert.match(await page.locator("#source").innerText(),/candidateParse\(input\)/);
 const sourceBefore = await page.locator("#source").textContent();
 const line = page.locator("#source .source-line").filter({hasText:"return candidateParse(input)"});
 const navigationPromise = page.waitForResponse(response => new URL(response.url()).pathname.endsWith("/navigation"));
 await line.click({button:"right"});
 const navigation = await navigationPromise, targets = await navigation.json();
 assert.equal(navigation.status(), 200, JSON.stringify(targets)); assert.deepEqual(targets.revision, pin);
 assert.equal(targets.targets.some(target => target.reason === "call" && target.matchKind === "resolved"), false, "import candidate has no resolved workspace callee");
 const enclosing = page.getByRole("menuitem", {name:/^Sequence · enclosing · syntax · measured/});
 await enclosing.waitFor();
 assert.equal(await page.getByRole("menuitem", {name:/Sequence of callee/}).count(), 0, "never guess a callee from import spelling");
 assert.equal(await page.getByRole("menuitem", {name:/Call graph/}).isDisabled(), true);
 assert.equal(await page.locator('[role="tab"]').filter({hasText:/Call graph/}).count(), 0, "#378 has no browser view");
 await enclosing.click();
 await page.waitForFunction(id => new URLSearchParams(location.hash.slice("#sequence?".length)).get("sid") === id && document.querySelector("#sequence-state")?.textContent.includes("Pele rendered"), dto.seed.id);
 await page.goBack();
 await page.waitForFunction(text => document.querySelector("#source")?.textContent === text && document.querySelector("#source")?.dataset.navigationLine === "3", sourceBefore);
 assert.deepEqual(await page.locator("#source .source-line.highlight .line-number").allTextContents(), [String(step.range.startLine)]);
 assert.equal(await page.locator("#source").evaluate(node => node === document.activeElement), true, "Inspector source menu returns focus to source");
},"flow.js"));

const truncatedSource=`function measured() {\n${Array.from({length:202},(_,i)=>`  observed_${i}();\n`).join("")}}\n`;
test("published truncated producer shows bound while retaining original call witness",{timeout:240000},async()=>published(truncatedSource,async({page,dto,pin})=>{
 assert.equal(dto.truncated,true,"the actual producer must reach its 200-step cap");
 assert.ok(dto.warnings.some(w=>w.includes("200 steps")));
 assert.match(await page.locator("#sequence-state").innerText(),/truncated/);
 assert.match(await page.locator("#sequence-state").innerText(),/Pele rendered/);
 const first=dto.steps.find(step=>step.kind==="call"&&step.label==="observed_0");
 assert.ok(first?.callId);
 const note=page.locator(`#sequence-diagram .pele-note[data-trellis-step-id="${first.id}"]`);
 assert.equal(await note.getAttribute("data-trellis-call-id"),first.callId);
 await note.click();
 assert.equal(await page.locator("#inspector-title").getAttribute("title"),first.label);
 assert.match(await page.locator("#inspector-detail").innerText(),/observed_0/);
 const requests=[];page.on("request",r=>{if(new URL(r.url()).pathname.endsWith("/source"))requests.push(r.url());});
 await page.locator("#inspector-open-source").click();
 await page.waitForFunction(()=>document.querySelector("#source-path")?.textContent?.includes("cached snapshot"));
 assert.ok(requests.some(raw=>{const q=new URL(raw).searchParams;return q.get("path")===first.path&&q.get("indexGeneration")===pin.indexGeneration&&q.get("indexRevision")===String(pin.indexRevision);}));
 assert.deepEqual(await page.locator("#source .source-line.highlight .line-number").allTextContents(),[String(first.range.startLine)]);
 assert.match(await page.locator("#source").innerText(),/observed_0\(\)/);
},"flow.js"));

// Exercise the app's own unsupported branch, not a test-side legacy render.
// The response is based on a real published SequenceView and keeps its pin/seed.
test("published app visibly falls back for malformed target evidence, depth, Mermaid, SVG and renderer failures",{timeout:240000},async()=>published(source,async({page,dto,pin})=>{
 const original=structuredClone(dto),leaf=original.steps.find(s=>s.kind==="call");
 assert.ok(leaf,"published call witness required");
 const cases=["dangling-target","depth","mermaid","svg","renderer-failure"];
 for(const scenario of cases) {
   const changed=structuredClone(original);
   const ids=[];
   if(scenario==="dangling-target") {
     changed.steps=[{...structuredClone(leaf),id:"fallback-target",label:"measured target evidence",
       target:"missing:declaration",callEvidence:{boundaryReason:"none",resolution:"resolved",dispatch:"direct",freshness:"fresh",provenanceId:"fixture:proof"},children:[],alternate:[],hidden:false}];
     ids.push(changed.steps[0].id);
   } else if(scenario==="depth") {
     let child={...structuredClone(leaf),id:"fallback-depth-24",label:"deep original evidence",children:[],alternate:[],hidden:false};
     ids.push(child.id);
     for(let n=23;n>=0;n--) {
       child={...structuredClone(leaf),id:`fallback-depth-${n}`,kind:"note",label:`depth evidence ${n}`,
         children:[child],alternate:[],hidden:false};
       ids.push(child.id);
     }
     changed.steps=[child];
   } else if(scenario==="mermaid") {
     changed.steps=[{...structuredClone(leaf),id:"fallback-large-source",label:"M".repeat(512*1024),
       children:[],alternate:[],hidden:false}];ids.push(changed.steps[0].id);
   } else {
     changed.steps=[{...structuredClone(leaf),id:`fallback-${scenario}`,label:`${scenario} measured evidence`,
       children:[],alternate:[],hidden:false}];ids.push(changed.steps[0].id);
   }
   assert.deepEqual(changed.revision,pin);
   assert.equal(changed.seed.id,original.seed.id);
   const responseRequests=[];const sourceRequests=[];
   const capture=request=>{if(new URL(request.url()).pathname.endsWith("/source"))sourceRequests.push(request.url());};
   page.on("request",capture);
   const routePattern="**/api/checkouts/*/sequence";
   const respond=async route=>{
     const body=route.request().postDataJSON();responseRequests.push(body);
     assert.equal(body.seed,changed.seed.id);assert.deepEqual(body.expectedRevision,pin);
     await route.fulfill({status:200,contentType:"application/json",body:JSON.stringify(changed)});
   };
   await page.route(routePattern,respond);
   try {
     await page.evaluate(scenario=>{
       const originalPele=window.pele,originalRender=originalPele.render;
       window.testOriginalPele=originalPele;
       if(scenario==="svg") window.pele={render:(...args)=>{
         const result=originalRender(...args),svg=result.svg;
         // Counting known SVG opening tags avoids DOM-parsing Pele's raw
         // inline styles, which would itself violate the production CSP.
         const count=(svg.match(/<(?:svg|g|path|rect|text|tspan)(?=[\s>])/g)||[]).length;
         const modified=svg.replace("</svg>",'<g class="pele-test-padding"></g>'.repeat(12001-count)+"</svg>");
         return {...result,svg:modified};
       }};
       if(scenario==="renderer-failure") window.pele={render:()=>{throw new Error("Pele renderer failed");}};
     },scenario);
     // This actual app control invokes loadSequence and consumes the routed,
     // published-pinned response. Show all is required to retain large labels.
     const checked=await page.locator("#all-steps").isChecked();
     if(checked) await page.locator("#all-steps").uncheck();
     else await page.locator("#all-steps").check();
     if(scenario==="mermaid" && !await page.locator("#all-steps").isChecked())
       await page.locator("#all-steps").check();
     await page.waitForFunction(()=>document.querySelector("#sequence-state")?.textContent?.includes("Pele fell back:"),null,{timeout:15000})
       .catch(async error=>{throw new Error(`${scenario}: ${error.message}; state=${await page.locator("#sequence-state").textContent()}; responses=${JSON.stringify(responseRequests)}; pageErrors=${JSON.stringify(await page.evaluate(()=>({text:document.querySelector("#sequence-diagram")?.textContent?.slice(0,500),nodes:window.__testPeleNodes})))}`);});
     const state=await page.locator("#sequence-state").textContent();
     assert.match(state,/Pele fell back:/,scenario);
     const reason={"dangling-target":/target|evidence|declaration/i,depth:/depth/,mermaid:/Mermaid source too large/,svg:/SVG element/,"renderer-failure":/Pele renderer failed/}[scenario];
     assert.match(state,reason,scenario);
     assert.ok(responseRequests.length>=1,`${scenario}: app did not fetch sequence`);
     assert.equal(await page.locator("#sequence-diagram svg.pele").count(),0,scenario);
     assert.equal(await page.locator("#sequence-diagram svg:not(.pele)").count(),1,scenario);
     const visible=await page.locator("#sequence-diagram [data-source-step-id]").evaluateAll(rows=>rows.map(n=>n.dataset.sourceStepId));
     for(const id of ids) assert.ok(visible.includes(id),`${scenario}: lost original ${id}`);
     if(scenario==="dangling-target") {
       assert.equal(await page.locator("#sequence-diagram .sequence-arrow").count(),0);
       assert.equal(await page.locator("#sequence-diagram .sequence-participant-label").count(),1);
     }
     assert.equal(sourceRequests.length,0,`${scenario}: source fetched without action`);
     assert.equal(await page.locator("#source .source-line").count(),0,scenario);
   } finally {
     await page.evaluate(()=>{if(window.testOriginalPele)window.pele=window.testOriginalPele;
       delete window.testOriginalPele;});
     await page.unroute(routePattern,respond);page.off("request",capture);
   }
 }
}));
