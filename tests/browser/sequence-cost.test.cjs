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
 assert.ok(steps.some(step=>step.label.includes("?")&&step.alternate.length),"measured ? early-return distinction required");
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
 const vendor=readFileSync(resolve(ROOT,"web/vendor/pele-1.6.0.js"));
 const record={count,coldAppMs,firstSelectionMs,os:`${os.platform()} ${os.release()} ${os.arch()}`,cpu:os.cpus()[0]?.model,chromium:await page.evaluate(()=>navigator.userAgent),rawBytes:vendor.length,gzipBytes:gzipSync(vendor).length,old:stats(result.old),pele:stats(result.pele),svgBytes:result.bytes,svgNodes:result.nodes,cspViolations:errors.filter(e=>/Content Security Policy|CSP/.test(e)).length};
 writeFileSync(`/tmp/trellis-232-cost-${count}.json`,JSON.stringify(record,null,2));console.log(JSON.stringify(record));
 });
});
