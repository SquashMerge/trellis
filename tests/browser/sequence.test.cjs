"use strict";
const test=require("node:test");const assert=require("node:assert/strict");
const {readFileSync,writeFileSync,rmSync}=require("node:fs");const {join}=require("node:path");
const {chromium}=require("playwright");const H=require("./sequence-harness.cjs");
const source='fn measured() {\n    first();\n    if condition() { then_arm(); } else { other_arm(); }\n    last();\n}\nfn first() {}\nfn condition() -> bool { true }\nfn then_arm() {}\nfn other_arm() {}\nfn last() {}\n';
test.before(async()=>H.setup(),{timeout:960000});test.after(async()=>H.cleanup());
async function published(sourceText,callback) {
 const paths=H.fixture(sourceText);let server,browser,context;
 try {
 const indexed=await H.run(H.binary,["index","--workspace",paths.workspace],{env:paths.env,timeout:120000});
 const pin=JSON.parse(indexed.stdout).status.revision;
 server=await H.startDaemon(paths,pin.indexRevision);
 browser=await chromium.launch();context=await browser.newContext();const page=await context.newPage();
 const errors=[];page.on("pageerror",e=>errors.push(e.message));page.on("console",m=>{if(m.type()==="error")errors.push(m.text());});
 const coldStart=Date.now();await H.connect(page,server,paths.token);const coldAppMs=Date.now()-coldStart;
 const file=page.locator(".file-pick").filter({hasText:"flow.rs"}).first();await file.waitFor();await file.click();
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
 assert.ok(sourceRequests.every(url=>{const query=new URL(url).searchParams;return query.get("indexGeneration")===pin.indexGeneration&&query.get("indexRevision")===String(pin.indexRevision)&&query.get("path")===measured.path;}));
 await target.focus();await page.keyboard.press("Enter");await page.keyboard.press("Space");
 // An obsolete DOM note must not mutate the selected inspector after the pin changes.
 await page.evaluate(()=>{window.savedNote=document.querySelector('.pele-note[data-trellis-step-id]');status.revision={...status.revision,indexRevision:status.revision.indexRevision+1};});
 const before=await page.locator("#call-inspector").innerText();await page.evaluate(()=>window.savedNote.dispatchEvent(new MouseEvent("click",{bubbles:true})));
 assert.equal(await page.locator("#call-inspector").innerText(),before);
}));
module.exports={published};
