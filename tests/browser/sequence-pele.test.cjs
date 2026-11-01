"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const {readFileSync} = require("node:fs");
const {createServer} = require("node:http");
const {resolve,join} = require("node:path");
const {chromium} = require("playwright");
const ROOT = resolve(__dirname,"../..");
const files = {"/vendor/pele-1.10.0-trellis.2.js":"web/vendor/pele-1.10.0-trellis.2.js","/sequence-pele.js":"web/sequence-pele.js","/sequence.js":"web/sequence.js","/style.css":"web/style.css"};
const fixture = language => JSON.parse(readFileSync(join(ROOT,`tests/fixtures/sequence-pele/${language}.json`)));
const html = '<!doctype html><html><head><link rel="stylesheet" href="/style.css"><script src="/vendor/pele-1.10.0-trellis.2.js" defer></script><script src="/sequence.js" defer></script><script src="/sequence-pele.js" defer></script></head><body><p id="state"></p><div id="sequence-diagram" class="sequence-scroll"></div></body></html>';
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
 const chosen=[];window.picks=chosen;window.targetPicks=[];window.current=true;
 const result=await TrellisPeleSequence.render(container,view,null,new Set(),{...options,isCurrent:()=>window.current,onSelect:step=>chosen.push(step),
   onOpenTarget:(declaration,step)=>window.targetPicks.push({declaration,step,exact:view.participants.some(participant=>participant.declaration===declaration)&&stepsContain(view.steps,step)})});
 function stepsContain(roots,original){return roots.some(step=>step===original||stepsContain(step.children||[],original)||stepsContain(step.alternate||[],original));}
 return {status:result.status,reason:result.reason,notes:[...container.querySelectorAll(".pele-note[data-trellis-step-id]")].map(n=>({id:n.dataset.trellisStepId,callId:n.dataset.trellisCallId,text:n.textContent})),svg:!!container.querySelector("svg.pele.trellis-pele"),source:result.source};
 },{view,options});}
function resolvedView() {
 const view=fixture("rust"),declaration={...structuredClone(view.seed),id:"exact:target",name:'actual <T> & café',path:"target.rs"};
 view.participants.push({id:declaration.id,label:declaration.name,kind:"internal",identification:"Fresh exact resolved declaration",declaration});
 view.steps=[{...view.steps[0],id:"resolved:occurrence",label:"lexical_decoy",target:declaration.id,
   callEvidence:{boundaryReason:"none",resolution:"resolved",dispatch:"direct",freshness:"fresh",provenanceId:"proof:occurrence"}}];
 return view;
}
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
test("pinned Pele shows original punctuation, Unicode and injection as text",async()=>{
 const view=fixture("rust");
 const labels=["if check()", "s.chars().count() > 180", "Vec<T>", 'a & "quotes" / back\\slash', "café 🔒", "bad\nalt injected ->> S: nope"];
 view.seed.name="seed <T> & café";
 view.steps=labels.map((label,i)=>({...structuredClone(view.steps[0]),id:`text-${i}`,label,children:[],alternate:[]}));
 const result=await render(view,{showDetails:true});assert.equal(result.status,"rendered",result.reason);
 assert.deepEqual(result.notes.map(note=>note.text),labels.map((label,i)=>`[${String(i+1).padStart(3,"0")}] call ${label} - measured syntax; target unverified`));
 assert.ok(!result.source.includes("U+"));assert.ok(!result.source.includes("->>"));
 assert.equal(await page.locator(".pele-note").count(),labels.length+1);
 assert.equal(await page.locator(".pele-heading").count(),0); // only one participant lane
 assert.equal(await page.locator("svg.pele script, svg.pele image").count(),0);
 assert.deepEqual(violations.filter(error=>/Content Security Policy|CSP/.test(error)),[]);
});

test("resolved declaration adds a safe lane and exact measured message with keyboard target action",async()=>{
 const view=resolvedView(),call=view.steps[0],target=view.participants.at(-1);
 const result=await render(view);assert.equal(result.status,"rendered",result.reason);
 assert.match(result.source,/S->>P1:/);assert.ok(!result.source.includes(target.label),"repository punctuation must be encoded");
 assert.equal(await page.locator(".pele-actor:not(.pele-actor-footer)").count(),2);
 const message=page.locator(`.pele-edge-label[data-trellis-step-id="${call.id}"]`);
 assert.equal(await message.getAttribute("data-trellis-call-id"),call.callId);
 // The arrow names the exact declaration; the measured spelling stays on the step (#558).
 assert.equal(await message.textContent(),"[001] actual <T> & café");
 assert.match(await message.getAttribute("aria-label"),/lexical_decoy/);
 await message.click();await message.focus();await page.keyboard.press("Enter");
 assert.deepEqual(await page.evaluate(()=>window.picks.map(step=>step.id)),[call.id,call.id]);
 const action=page.locator(`.pele-actor[data-trellis-target-id="${target.id}"]`).first();
 assert.match(await action.getAttribute("aria-label"),/Open callee sequence/);
 await action.click();await action.focus();await page.keyboard.press("Enter");await page.keyboard.press("Space");
 assert.deepEqual(await page.evaluate(()=>window.targetPicks.map(item=>({id:item.declaration.id,callId:item.step.callId,exact:item.exact}))),
   Array.from({length:3},()=>({id:target.id,callId:call.callId,exact:true})));
 assert.equal(await page.evaluate(()=>window.picks.length),2,"opening declaration must not select or read the call site");
 await page.evaluate(()=>window.current=false);await action.click({force:true});
 assert.equal(await page.evaluate(()=>window.targetPicks.length),3);
 assert.deepEqual(violations.filter(error=>/Content Security Policy|CSP/.test(error)),[]);
});

test("hidden resolved calls leave no lane; nested and collapsed calls preserve exact occurrence identity",async()=>{
 const view=resolvedView(),leaf=structuredClone(view.steps[0]);
 view.steps=[{...leaf,hidden:true}];
 assert.equal((await render(view)).status,"rendered");
 assert.equal(await page.locator(".pele-message").count(),0);
 assert.equal(await page.locator("[data-trellis-target-id]").count(),0);
 assert.equal(await page.locator(".pele-actor:not(.pele-actor-footer)").count(),1);
 view.steps=[{...leaf,id:"resolved:guard",kind:"branch",callId:null,target:null,callEvidence:null,children:[leaf]}];
 assert.equal((await render(view)).status,"rendered");
 assert.equal(await page.locator(`.pele-edge-label[data-trellis-step-id="${leaf.id}"]`).getAttribute("data-trellis-call-id"),leaf.callId);
 const second={...structuredClone(leaf),id:"resolved:second",callId:"second:call",target:null,callEvidence:null};
 view.steps=[{...leaf,id:"resolved:group",kind:"group",callId:null,target:null,callEvidence:null,children:[leaf,second]}];
 const collapsed=await render(view);assert.equal(collapsed.status,"rendered",collapsed.reason);
 const preview=page.locator('.pele-edge-label[data-trellis-step-id="resolved:group"]');
 assert.equal(await preview.getAttribute("data-trellis-preview-call-id"),leaf.callId);
 assert.match(await preview.textContent(),/entry preview/);
 await page.locator("button.sequence-group-control").click();
 assert.equal(await page.locator(`.pele-edge-label[data-trellis-step-id="${leaf.id}"]`).getAttribute("data-trellis-call-id"),leaf.callId);
 assert.equal(await page.locator(`.pele-note[data-trellis-step-id="${second.id}"]`).getAttribute("data-trellis-call-id"),second.callId);
});

test("self targets reuse seed lane and same-name declarations keep separate exact lanes",async()=>{
 const self=resolvedView();self.participants[0].declaration=structuredClone(self.seed);self.steps[0].target=self.seed.id;
 const selfResult=await render(self);assert.equal(selfResult.status,"rendered",selfResult.reason);
 assert.match(selfResult.source,/S->>S:/);
 assert.equal(await page.locator(".pele-actor:not(.pele-actor-footer)").count(),1);
 const selfAction=page.locator(`.pele-actor[data-trellis-target-id="${self.seed.id}"]`).first();
 await selfAction.focus();await page.keyboard.press("Enter");
 assert.equal(await page.evaluate(()=>window.targetPicks[0]?.declaration.id),self.seed.id);
 const view=resolvedView(),first=view.participants.at(-1),other=structuredClone(first);
 other.id="exact:other-target";other.declaration.id=other.id;other.declaration.path="other-target.rs";
 view.participants.push(other);view.steps.push({...structuredClone(view.steps[0]),id:"resolved:same-name",callId:"same-name:call",target:other.id});
 const result=await render(view);assert.equal(result.status,"rendered",result.reason);
 assert.equal(await page.locator(".pele-actor:not(.pele-actor-footer)").count(),3);
 assert.equal(await page.locator(".pele-message").count(),2);
 for(const participant of [first,other]) await page.locator(`.pele-actor[data-trellis-target-id="${participant.id}"]`).first().click();
 assert.deepEqual(await page.evaluate(()=>window.targetPicks.map(item=>({id:item.declaration.id,path:item.declaration.path}))),
   [first,other].map(item=>({id:item.id,path:item.declaration.path})));
});

test("malformed, contradictory and dangling target evidence refuses Pele without fabricated legacy targets",async()=>{
 const cases=[
   view=>{delete view.steps[0].callEvidence;},
   view=>{delete view.steps[0].target;},
   view=>{view.steps[0].callEvidence.resolution="ambiguous";},
   view=>{view.steps[0].target="not-a-participant";},
   view=>{view.participants.at(-1).declaration.id="different:declaration";},
   view=>{delete view.participants.at(-1).declaration;},
   view=>{view.participants.at(-1).declaration.range.startLine=0;},
   view=>{view.steps[0].callEvidence.freshness="stale";},
   view=>{view.steps[0].callEvidence.boundaryReason="ambiguous";},
   view=>{view.steps[0].callEvidence.dispatch="dynamic";},
   view=>{delete view.steps[0].callEvidence.provenanceId;}
 ];
 for(const change of cases) {
   const view=resolvedView();change(view);
   const outcome=await renderWithVisibleFallback(view);
   assertVisibleFallback(outcome,[view.steps[0].id],view.steps[0].label);
   assert.match(outcome.reason,/target|evidence|declaration/i);
   assert.equal(await page.locator(".sequence-arrow").count(),0);
   assert.equal(await page.locator("[data-trellis-target-id]").count(),0);
   assert.equal(await page.locator(".sequence-participant-label").count(),1);
 }
 for(const enclosing of ["hidden","branch","group"]) {
   const view=resolvedView(),leaf=view.steps[0];leaf.target="not-a-participant";
   if(enclosing==="hidden") leaf.hidden=true;
   else view.steps=[{...structuredClone(leaf),id:`invalid:${enclosing}`,kind:enclosing,callId:null,target:null,callEvidence:null,children:[leaf]}];
   const result=await render(view);
   assert.equal(result.status,"unsupported",`${enclosing} contradictory evidence must not be ignored`);
   assert.match(result.reason,/target|evidence|declaration/i);
 }
});

test("engine boundary states remain labelled source notes without navigation targets",async()=>{
 const states={external:"External boundary",unresolved:"Unresolved target",ambiguous:"Ambiguous target",dispatch:"Dispatch boundary",
   stale:"Stale evidence",missingEvidence:"missing semantic evidence",nodeLimit:"participant limit"};
 const view=resolvedView(),leaf=view.steps[0];
 view.steps=Object.keys(states).map((reason,i)=>({...structuredClone(leaf),id:`boundary:${reason}`,callId:`boundary:call:${i}`,target:null,
   callEvidence:{boundaryReason:reason,resolution:reason==="ambiguous"?"ambiguous":"unresolved",dispatch:reason==="dispatch"?"dynamic":"direct",freshness:reason==="stale"?"stale":"fresh",provenanceId:`proof:${i}`}}));
 const outcome=await render(view);assert.equal(outcome.status,"rendered",outcome.reason);
 for(const [reason,label] of Object.entries(states)) assert.ok((await page.locator(`.pele-note[data-trellis-step-id="boundary:${reason}"]`).textContent()).includes(label),reason);
 assert.equal(await page.locator(".pele-message,[data-trellis-target-id]").count(),0);
 assert.equal(await page.locator(".pele-actor:not(.pele-actor-footer)").count(),1);
});

test("legacy fallback preserves resolved arrows, declaration actions and original source selection",async()=>{
 const view=resolvedView();
 await page.evaluate(view=>{
   const container=document.querySelector("#sequence-diagram");window.current=true;window.picks=[];window.targetPicks=[];
   TrellisSequence.render(container,view,null,new Set(),{isCurrent:()=>window.current,onSelect:step=>window.picks.push(step),
     onOpenTarget:(declaration,step)=>window.targetPicks.push({declaration,step,exact:declaration===view.participants.at(-1).declaration&&step===view.steps[0]})});
 },view);
 assert.equal(await page.locator(".sequence-arrow[data-trellis-call-id]").count(),1);
 assert.equal(await page.locator(".sequence-participant-label").count(),2);
 const action=page.locator(`.sequence-participant-label[data-trellis-target-id="${view.steps[0].target}"]`);
 await action.focus();await page.keyboard.press("Enter");await page.keyboard.press("Space");
 assert.equal(await page.evaluate(()=>window.targetPicks.length),2);
 assert.equal(await page.evaluate(()=>window.targetPicks.every(item=>item.exact)),true);
 const row=page.locator(`[data-source-step-id="${view.steps[0].id}"]`);await row.focus();await page.keyboard.press("Enter");
 assert.equal(await page.evaluate(()=>window.picks[0]?.callId),view.steps[0].callId);
 await page.evaluate(()=>window.current=false);await action.click({force:true});
 assert.equal(await page.evaluate(()=>window.targetPicks.length),2);
});

test("changed resolved message or participant mapping refuses all target and source actions",async()=>{
 const view=resolvedView();
 const results=await page.evaluate(async view=>{
   const container=document.querySelector("#sequence-diagram"),original=window.pele,results=[];
   for(const mutation of ["message-id","message-text","participant-id","participant-text","message-order"]) {
     const changed=structuredClone(view);
     if(mutation==="message-order") changed.steps.push({...structuredClone(changed.steps[0]),id:"resolved:other",callId:"resolved:other-call",label:"different_call"});
     container.replaceChildren();let selections=0,sources=0,targets=0;
     window.pele={render:(...args)=>{
       const output=original.render(...args);
       // Change trusted renderer output before production validation, not the installed DOM.
       let svg=output.svg;
       if(mutation==="message-id") svg=svg.replace(/(class="pele-edge pele-message" data-id=")[^"]+/,"$1i999");
       if(mutation==="message-text") svg=svg.replace("[001]","[002]");
       if(mutation==="participant-id") svg=svg.replace(/(class="pele-node pele-actor[^"]*" data-id=")P1/,"$1P999");
       if(mutation==="participant-text") svg=svg.replace("actual &lt;T&gt; &amp; café","altered &lt;T&gt; &amp; café");
       if(mutation==="message-order") svg=svg.replace(/(class="pele-edge-label" data-id=")i1/,"$1i2");
       if(svg===output.svg) throw Error(`Mutation ${mutation} did not match renderer output`);
       return {...output,svg};
     }};
     try {
       const outcome=await TrellisPeleSequence.render(container,changed,()=>sources++,new Set(),{onSelect:()=>selections++,onOpenTarget:()=>targets++});
       results.push({mutation,status:outcome.status,reason:outcome.reason,nodes:container.childElementCount,selections,sources,targets});
     }finally{window.pele=original;}
   }
   return results;
 },view);
 for(const result of results) {
   assert.equal(result.status,"unsupported",result.mutation);
   assert.match(result.reason,/Changed Pele message|Changed Pele participant/,result.mutation);
   assert.equal(result.nodes,0,result.mutation);
   assert.equal(result.selections+result.sources+result.targets,0,result.mutation);
 }
});

test("detached resolved lane and message actions cannot act on a newer pinned view",async()=>{
 const view=resolvedView();
 const result=await page.evaluate(async view=>{
   const container=document.querySelector("#sequence-diagram");let active="old",targets=0,selections=0,sources=0;
   const first=await TrellisPeleSequence.render(container,view,()=>sources++,new Set(),{isCurrent:()=>active==="old",onSelect:()=>selections++,onOpenTarget:()=>targets++});
   if(first.status!=="rendered") throw Error(first.reason);
   const lane=container.querySelector(".pele-actor[data-trellis-target-id]"),message=container.querySelector(".pele-edge-label[data-trellis-step-id]");
   const next=structuredClone(view);next.revision.indexRevision++;next.steps[0].id="new:resolved:occurrence";
   active="new";
   const second=await TrellisPeleSequence.render(container,next,()=>sources++,new Set(),{isCurrent:()=>active==="new",onSelect:()=>selections++,onOpenTarget:()=>targets++});
   for(const node of [lane,message]) for(const event of [new MouseEvent("click",{bubbles:true}),new KeyboardEvent("keydown",{key:"Enter",bubbles:true}),new KeyboardEvent("keydown",{key:" ",bubbles:true})]) node.dispatchEvent(event);
   return {status:second.status,targets,selections,sources,newMessage:!!container.querySelector('.pele-edge-label[data-trellis-step-id="new:resolved:occurrence"]')};
 },view);
 assert.equal(result.status,"rendered");assert.equal(result.newMessage,true);
 assert.equal(result.targets+result.selections+result.sources,0);
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
     return {...output,svg:output.svg.replace("Static possible paths - not a runtime trace","Static possible paths - altered runtime trace")};
   }};
   try{return await TrellisPeleSequence.render(container,view,step=>window.sourceActions.push(step),new Set(),{onSelect:step=>window.picks.push(step)});}
   finally{window.pele=original;}
 },view);
 assert.equal(result.status,"unsupported");assert.match(result.reason,/Changed Pele note text/);
 assert.equal(await page.locator("#sequence-diagram .pele-note").count(),0);
 assert.equal(await page.evaluate(()=>window.picks.length),0);
 assert.equal(await page.evaluate(()=>window.sourceActions.length),0);
});
test("failed group expansion visibly installs complete legacy DTO",async()=>{
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

// AC4: obsolete renderer outcomes and detached notes cannot affect a newer pin.
test("stale synchronous Pele success and failure leave the newer pinned diagram untouched", async () => {
 const oldView=fixture("rust"),newView=fixture("rust");
 newView.revision.indexRevision+=1;
 newView.steps=[{...newView.steps[0],id:"new-pin-evidence",label:"new pinned evidence"}];
 const observations=await page.evaluate(async ({oldView,newView})=>{
   const container=document.querySelector("#sequence-diagram"), state=document.querySelector("#state");
   const original=window.pele, results=[];
   try {
     for(const scenario of ["success","failure","source-success","source-failure"]) {
       const sourceOnly=scenario.startsWith("source-");
       const completion=scenario.replace(/^source-/,"");
       let activePin="old",selections=0,sources=0,oldSelections=0,oldSources=0;
       container.replaceChildren();state.textContent="old pinned revision";
       const oldCurrent=()=>activePin==="old",newCurrent=()=>activePin==="new";
       const initial=await TrellisPeleSequence.render(container,oldView,()=>oldSources++,new Set(),
         {isCurrent:oldCurrent,...(sourceOnly?{}:{onSelect:()=>oldSelections++})});
       if(initial.status!=="rendered") throw Error(initial.reason);
       const staleNote=container.querySelector('.pele-note[data-trellis-step-id]');
       let newerRequest,currentDiagram;
       window.pele={render:(...args)=>{
         // A synchronous renderer can change the current snapshot before its
         // output or exception returns. Publish the new pin at that boundary.
         activePin="new";window.pele=original;
         newerRequest=TrellisPeleSequence.render(container,newView,()=>sources++,new Set(),
           {isCurrent:newCurrent,...(sourceOnly?{}:{onSelect:()=>selections++})});
         currentDiagram=container.firstElementChild;
         state.textContent="new pinned revision";
         if(completion==="failure") throw new Error("old renderer failed");
         return original.render(...args);
       }};
       const outcome=await TrellisPeleSequence.render(container,oldView,()=>oldSources++,new Set(),{
         isCurrent:oldCurrent,...(sourceOnly?{}:{onSelect:()=>oldSelections++}),
         onUnsupported:()=>{state.textContent="old fallback";}
       });
       const newer=await newerRequest;
       if(newer?.status!=="rendered") throw Error(newer?.reason||"new pin did not render");
       staleNote.dispatchEvent(new MouseEvent("click",{bubbles:true}));
       staleNote.dispatchEvent(new KeyboardEvent("keydown",{key:"Enter",bubbles:true}));
       staleNote.dispatchEvent(new KeyboardEvent("keydown",{key:" ",bubbles:true}));
       results.push({scenario,status:outcome.status,same:container.firstElementChild===currentDiagram,
         state:state.textContent,newNote:!!container.querySelector('.pele-note[data-trellis-step-id="new-pin-evidence"]'),
         oldNote:!!container.querySelector('.pele-note[data-trellis-step-id="'+oldView.steps[0].id+'"]'),
         selections,sources,oldSelections,oldSources});
     }
   }finally{window.pele=original;}
   return results;
 },{oldView,newView});
 for(const observation of observations) {
   assert.equal(observation.status,"stale",observation.scenario);
   assert.equal(observation.same,true,observation.scenario);
   assert.equal(observation.newNote,true,observation.scenario);
   assert.equal(observation.oldNote,false,observation.scenario);
   assert.equal(observation.state,"new pinned revision",observation.scenario);
   for(const field of ["selections","sources","oldSelections","oldSources"])
     assert.equal(observation[field],0,`${observation.scenario}: ${field}`);
 }
});

// The app's unsupported branch receives the same complete pinned DTO. Use its
// actual legacy renderer and visible status rather than treating retained Pele
// markup from a prior pin as a fallback.
async function renderWithVisibleFallback(view, options={}) {
 return page.evaluate(async ({view,options})=>{
   const container=document.querySelector("#sequence-diagram"),state=document.querySelector("#state");
   container.replaceChildren();state.textContent="Pele pending";
   let selections=0,sources=0;
   const expanded=new Set(),callbacks={...options,isCurrent:()=>true,onSelect:()=>selections++};
   const outcome=await TrellisPeleSequence.render(container,view,()=>sources++,expanded,callbacks);
   if(outcome.status==="unsupported") {
     TrellisSequence.render(container,view,()=>sources++,expanded,callbacks);
     state.textContent=`Pele fell back: ${outcome.reason}`;
   }
   return {status:outcome.status,reason:outcome.reason,state:state.textContent,
     pele:container.querySelectorAll("svg.pele").length,legacy:container.querySelectorAll("svg:not(.pele)").length,
     rows:[...container.querySelectorAll("[data-source-step-id]")].map(row=>row.dataset.sourceStepId),
     text:container.textContent,selections,sources};
 },{view,options});
}
function assertVisibleFallback(result,ids,marker) {
 assert.equal(result.status,"unsupported");assert.match(result.state,/Pele fell back:/);
 assert.equal(result.pele,0);assert.equal(result.legacy,1);
 for(const id of ids) assert.ok(result.rows.includes(id),`missing original step ${id}`);
 assert.ok(result.text.includes(marker),`missing evidence ${marker}`);
 assert.equal(result.selections,0);assert.equal(result.sources,0);
}

test("24 levels render; rejected 25th level and over-512 KiB source visibly fall back whole", async()=>{
 const view=fixture("rust"),leaf=structuredClone(view.steps[0]);
 const depth=levels=>{let node={...leaf,id:`depth-${levels-1}`,children:[]};
   for(let n=levels-2;n>=0;n--) node={...leaf,id:`depth-${n}`,kind:"note",children:[node]};
   return node;};
 view.steps=[depth(24)];
 const allowable=await render(view);
 assert.equal(allowable.status,"rendered",allowable.reason);
 view.steps=[depth(25)];
 const tooDeep=await renderWithVisibleFallback(view);
 assert.match(tooDeep.reason,/depth/);
 assertVisibleFallback(tooDeep,Array.from({length:25},(_,i)=>`depth-${i}`),leaf.label);
 const sourceView=fixture("rust");sourceView.steps=[{...leaf,label:"L".repeat(512*1024),id:"large-source"}];
 const oversized=await renderWithVisibleFallback(sourceView,{showDetails:true});
 assert.match(oversized.reason,/Mermaid source too large/);
 assertVisibleFallback(oversized,["large-source"],"LLLLLLLL");
});

test("12,000 SVG nodes allowed and 12,001 reject with no partial SVG or selection",async()=>{
 const view=fixture("rust"),result=await page.evaluate(async view=>{
   const container=document.querySelector("#sequence-diagram"),original=window.pele;
   const first=await TrellisPeleSequence.render(container,view,null,new Set(),{});
   if(first.status!=="rendered") throw Error(first.reason);
   const prior=container.firstElementChild;
   const projection=TrellisPeleSequence.project(view);
   const raw=original.render(projection.source,{}).svg;
   const parser=new DOMParser().parseFromString(raw,"image/svg+xml");
   const baseNodes=parser.querySelectorAll("*").length;
   const filler='<g class="pele-test-padding"></g>';
   const make=(count)=>raw.replace("</svg>",filler.repeat(count-baseNodes)+"</svg>");
   let selections=0,sources=0;
   try {
     window.pele={render:()=>({svg:make(12000)})};
     const boundary=await TrellisPeleSequence.render(container,view,null,new Set(),{onSelect:()=>selections++});
     const installed=container.firstElementChild;
     const noteIds=[...container.querySelectorAll('.pele-note[data-trellis-step-id]')].map(n=>n.dataset.trellisStepId);
     window.pele={render:()=>({svg:make(12001)})};
     const overflow=await TrellisPeleSequence.render(container,view,null,new Set(),{onSelect:()=>selections++});
     const kept=installed===container.firstElementChild;
     TrellisSequence.render(container,view,()=>sources++,new Set(),{onSelect:()=>selections++});
     document.querySelector("#state").textContent=`Pele fell back: ${overflow.reason}`;
     return {boundary:boundary.status,overflow:overflow.status,reason:overflow.reason,
       kept,notOld:installed!==prior,nodes:installed.querySelectorAll('*').length+1,
       noteIds,selections,sources,legacy:container.querySelectorAll("svg:not(.pele)").length,
       pele:container.querySelectorAll("svg.pele").length,
       rows:[...container.querySelectorAll("[data-source-step-id]")].map(n=>n.dataset.sourceStepId),
       text:container.textContent,state:document.querySelector("#state").textContent};
   }finally{window.pele=original;}
 },view);
 assert.equal(result.boundary,"rendered");assert.equal(result.nodes,12000);
 assert.equal(result.overflow,"unsupported");assert.match(result.reason,/SVG element/);
 assert.equal(result.kept,true);assert.equal(result.notOld,true);
 assert.deepEqual(result.noteIds,steps(view.steps).map(step=>step.id));
 assert.equal(result.legacy,1);assert.equal(result.pele,0);
 assert.match(result.state,/Pele fell back:/);
 for(const step of steps(view.steps)) assert.ok(result.rows.includes(step.id),step.id);
 assert.ok(result.text.includes(view.steps[0].label));
 assert.equal(result.selections,0);assert.equal(result.sources,0);
});

test("currently pinned synchronous Pele failure visibly falls back to complete legacy evidence",async()=>{
 const view=fixture("rust"),result=await page.evaluate(async view=>{
   const container=document.querySelector("#sequence-diagram"),state=document.querySelector("#state"),original=window.pele;
   container.replaceChildren();state.textContent="Pele pending";
   let selections=0,sources=0,started=false;
   window.pele={render:()=>{started=true;throw new Error("Pele renderer failed");}};
   try {
     const outcome=await TrellisPeleSequence.render(container,view,()=>sources++,new Set(),
       {isCurrent:()=>true,onSelect:()=>selections++});
     if(outcome.status==="unsupported") {
       TrellisSequence.render(container,view,()=>sources++,new Set(),{onSelect:()=>selections++});
       state.textContent=`Pele fell back: ${outcome.reason}`;
     }
     return {started,status:outcome.status,reason:outcome.reason,state:state.textContent,
       pele:container.querySelectorAll("svg.pele").length,legacy:container.querySelectorAll("svg:not(.pele)").length,
       rows:[...container.querySelectorAll("[data-source-step-id]")].map(n=>n.dataset.sourceStepId),
       text:container.textContent,selections,sources};
   }finally{window.pele=original;}
 },view);
 assert.equal(result.started,true);assert.match(result.reason,/Pele renderer failed/);
 assertVisibleFallback(result,steps(view.steps).map(step=>step.id),view.steps[0].label);
});
