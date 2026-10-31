"use strict";
const test=require("node:test"),assert=require("node:assert/strict"),{readFileSync,writeFileSync}=require("node:fs"),{resolve}=require("node:path"),{gzipSync}=require("node:zlib"),os=require("node:os");
const {published}=require("./sequence.test.cjs");
const ROOT=resolve(__dirname,"../..");
function stats(values){const sorted=[...values].sort((a,b)=>a-b);return {median:sorted[Math.floor(sorted.length/2)],p95:sorted[Math.ceil(sorted.length*.95)-1]};}
for(const count of [80,180])test(`indexed ${count}-call cohort uses Pele and measures 20 same-DTO renders`,{timeout:360000},async()=>{
 const source=readFileSync(resolve(ROOT,`tests/fixtures/sequence-pele/cost-${count}.rs`),"utf8");
 await published(source,async({page,dto,errors,coldAppMs,firstSelectionMs})=>{
 const steps=(function walk(items){return items.flatMap(step=>[step,...walk(step.children||[]),...walk(step.alternate||[])]);})(dto.steps);
 const calls=steps.filter(step=>step.kind==="call");assert.equal(calls.length,count,`actual indexed producer must emit ${count} calls; got ${calls.length}`);
 assert.ok(steps.some(step=>step.kind==="branch"&&step.alternate.length),"measured branch and alternate required");
 assert.ok(steps.some(step=>step.kind==="loop"),"measured loop required");
 assert.ok(steps.some(step=>step.folded?.some(f=>f.label.includes("?")&&f.alternate.length)),"measured ? early-return distinction required (folded into its call, #542)");
 assert.ok(steps.some(step=>step.kind==="group"&&step.children.length>=3),"measured flat call group required");
 assert.ok(calls.some(step=>step.label.includes("unresolved_external_7")),"explicit unknown target call required");
 assert.ok(dto.participants.some(participant=>participant.kind==="unresolvedCallee"),"unknown target must remain unresolved");
 const result=await page.evaluate(async dto=>{
 const wrap=document.createElement("div");document.body.append(wrap);
 const old=[],pele=[];let result;
 for(let i=0;i<20;i++){
   let start=performance.now();TrellisSequence.render(wrap,dto,()=>{},new Set(),{});old.push(performance.now()-start);
   wrap.replaceChildren();start=performance.now();result=await TrellisPeleSequence.render(wrap,dto,()=>{},new Set(),{});pele.push(performance.now()-start);
   if(result.status!=="rendered")return {status:result.status,reason:result.reason};
 }
 const svg=wrap.querySelector("svg");return {status:result.status,old,pele,bytes:new TextEncoder().encode(svg.outerHTML).length,nodes:svg.querySelectorAll("*").length};
 },dto);
 assert.equal(result.status,"rendered",result.reason);
 if(count===80){
   const visible=(function walk(items){return items.flatMap(step=>step.hidden?[]:[step,...(step.kind==="group"?[]:[...walk(step.children||[]),...walk(step.alternate||[])])]);})(dto.steps);
   const notes=await page.locator("#sequence-diagram .pele-note[data-trellis-step-id]").evaluateAll(ns=>ns.map(n=>({id:n.dataset.trellisStepId,callId:n.dataset.trellisCallId,text:n.textContent})));
   assert.deepEqual(notes.map(n=>n.id),visible.map(s=>s.id),"nested guard, alternate, loop, ? exit and folded group retain measured order");
   const branch=steps.find(s=>s.kind==="branch"&&s.alternate.length),first=branch.children.find(s=>s.kind==="call"),alternate=branch.alternate.find(s=>s.kind==="call");
   assert.ok(first&&alternate);assert.ok(notes.findIndex(n=>n.id===branch.id)<notes.findIndex(n=>n.id===first.id));
   assert.ok(notes.findIndex(n=>n.id===first.id)<notes.findIndex(n=>n.id===alternate.id));
   assert.ok(notes.some(n=>n.text.includes("may return early")),"? early-return remains distinct on its call (#542)");
   assert.ok(await page.locator(".pele-frame-alt").count()>=2);
   assert.ok(await page.locator(".pele-frame-loop").count()>=1);
   assert.equal(await page.locator(".pele-message").count(),0,"unknown target must not become a fabricated arrow");
   const unknown=calls.find(s=>s.label.includes("unresolved_external_7")),note=page.locator(`#sequence-diagram .pele-note[data-trellis-step-id="${unknown.id}"]`);
   assert.equal(await note.getAttribute("data-trellis-call-id"),unknown.callId);
   await note.focus();await page.keyboard.press("Space");
   assert.equal(await note.getAttribute("aria-pressed"),"true");
   assert.equal(await page.locator("#inspector-title").getAttribute("title"),unknown.label);
   assert.match(await page.locator("#inspector-target").innerText(),/no verified target|unresolved|unverified/i);
   const requests=[];page.on("request",r=>{if(new URL(r.url()).pathname.endsWith("/source"))requests.push(r.url());});
   await page.locator("#inspector-open-source").click();
   await page.waitForFunction(()=>document.querySelector("#source-path")?.textContent?.includes("cached snapshot"));
   assert.ok(requests.some(raw=>{const q=new URL(raw).searchParams;return q.get("path")===unknown.path&&q.get("indexGeneration")===dto.revision.indexGeneration&&q.get("indexRevision")===String(dto.revision.indexRevision);}));
   assert.deepEqual(await page.locator("#source .source-line.highlight .line-number").allTextContents(),Array.from({length:unknown.range.endLine-unknown.range.startLine+1},(_,i)=>String(unknown.range.startLine+i)));
 }
 const vendor=readFileSync(resolve(ROOT,"web/vendor/pele-1.10.0-trellis.2.js"));
 const record={count,coldAppMs,firstSelectionMs,os:`${os.platform()} ${os.release()} ${os.arch()}`,cpu:os.cpus()[0]?.model,chromium:await page.evaluate(()=>navigator.userAgent),rawBytes:vendor.length,gzipBytes:gzipSync(vendor).length,old:stats(result.old),pele:stats(result.pele),svgBytes:result.bytes,svgNodes:result.nodes,cspViolations:errors.filter(e=>/Content Security Policy|CSP/.test(e)).length};
 writeFileSync(`/tmp/trellis-232-cost-${count}.json`,JSON.stringify(record,null,2));console.log(JSON.stringify(record));
 });
});
