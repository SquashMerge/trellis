"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const {readFileSync} = require("node:fs");
const {createServer} = require("node:http");
const {resolve,join} = require("node:path");
const {chromium} = require("playwright");
const ROOT = resolve(__dirname,"../..");
const files = {"/vendor/pele-1.10.0-trellis.2.js":"web/vendor/pele-1.10.0-trellis.2.js","/sequence-pele.js":"web/sequence-pele.js","/source-witness.js":"web/source-witness.js", "/sequence-model.js":"web/sequence-model.js", "/sequence.js":"web/sequence.js", "/pele-svg.js":"web/pele-svg.js","/style.css":"web/style.css"};
const fixture = language => JSON.parse(readFileSync(join(ROOT,`tests/fixtures/sequence-pele/${language}.json`)));
const html = '<!doctype html><html><head><link rel="stylesheet" href="/style.css"><script src="/vendor/pele-1.10.0-trellis.2.js" defer></script><script src="/source-witness.js" defer></script><script src="/sequence-model.js" defer></script><script src="/sequence.js" defer></script><script src="/pele-svg.js" defer></script><script src="/sequence-pele.js" defer></script></head><body><p id="state"></p><div id="sequence-diagram" class="sequence-scroll"></div></body></html>';
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

test("malformed, contradictory and dangling target evidence refuses Pele and shows the error state, with no fabricated target",async()=>{
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
   const outcome=await renderWithErrorState(view);
   assertErrorState(outcome,/target|evidence|declaration/i);
   assert.equal(await page.locator("[data-trellis-target-id],[data-trellis-step-id]").count(),0);
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

// #1007: there is no second renderer. The error state is the empty state's plain line.
test("the error state replaces a drawn diagram with one plain line and leaves the old controls inert",async()=>{
 const view=resolvedView();
 assert.equal((await render(view)).status,"rendered");
 const result=await page.evaluate(()=>{
   const container=document.querySelector("#sequence-diagram");
   const old=[container.querySelector(".pele-edge-label[data-trellis-step-id]"),container.querySelector(".pele-actor[data-trellis-target-id]")];
   TrellisSequence.renderError(container,'Unsupported <b>sequence</b> step');
   for(const node of old) for(const event of [new MouseEvent("click",{bubbles:true}),new KeyboardEvent("keydown",{key:"Enter",bubbles:true}),new KeyboardEvent("keydown",{key:" ",bubbles:true})]) node.dispatchEvent(event);
   const note=container.firstElementChild,empty=document.createElement("p");empty.className="sequence-empty";empty.textContent="reference";container.append(empty);
   const look=node=>{const style=getComputedStyle(node);return [style.color,style.fontSize,style.marginTop,style.marginLeft].join("|");};
   const seen={children:container.childElementCount-1,tag:note.tagName,cls:note.className,text:note.textContent,elements:note.childElementCount,
     visible:note.getBoundingClientRect().height>0,sameLook:look(note)===look(empty),svg:container.querySelectorAll("svg").length,
     controls:container.querySelectorAll("button,[role=button],[tabindex]").length,picks:window.picks.length,targets:window.targetPicks.length};
   empty.remove();return seen;
 });
 assert.deepEqual(result,{children:1,tag:"P",cls:"sequence-empty",text:"This Sequence could not be drawn: Unsupported <b>sequence</b> step",elements:0,
   visible:true,sameLook:true,svg:0,controls:0,picks:0,targets:0});
 assert.deepEqual(violations.filter(error=>/Content Security Policy|CSP/.test(error)),[]);
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

test("malformed SVG and duplicate IDs are refused without partial diagram",async()=>{
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
test("failed group expansion shows the error state with the adapter's reason",async()=>{
 const view=fixture("rust"),group=structuredClone(view.steps[0]),entry=structuredClone(view.steps[0]);
 group.id="unsupported-group";group.kind="group";group.callId=null;group.label="original complete group";
 entry.id="unsupported-child";delete entry.hidden;group.children=[entry];view.steps=[group];
 const initial=await page.evaluate(async view=>{
   const container=document.querySelector("#sequence-diagram");container.replaceChildren();
   const expanded=new Set();document.querySelector("#state").textContent="Pele rendered";
   // The app's own redraw handler (web/app.js): the error state, and the reason in the status line.
   const options={isCurrent:()=>true,onUnsupported:reason=>{
     TrellisSequence.renderError(container,reason);
     document.querySelector("#state").textContent=`This Sequence could not be drawn: ${reason}`;
   }};
   return (await TrellisPeleSequence.render(container,view,null,expanded,options)).status;
 },view);
 assert.equal(initial,"rendered");
 await page.locator('.trellis-pele button.sequence-group-control').click();
 await page.waitForFunction(()=>document.querySelector("#state")?.textContent?.includes("could not be drawn"));
 assert.match(await page.locator("#state").textContent(),/Unsupported sequence step/);
 assert.equal(await page.locator("#sequence-diagram svg").count(),0,"neither the folded diagram nor a second drawing stays");
 assert.equal(await page.locator("#sequence-diagram > *").count(),1);
 assert.equal(await page.locator("#sequence-diagram > p.sequence-empty").textContent(),"This Sequence could not be drawn: Unsupported sequence step");
 assert.equal(await page.locator("#sequence-diagram button, #sequence-diagram [role=button]").count(),0);
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

// The app's unsupported branch (web/app.js): the error state in the diagram area and the
// adapter's reason in the status line. Nothing of the refused view is drawn.
async function renderWithErrorState(view, options={}) {
 return page.evaluate(async ({view,options})=>{
   const container=document.querySelector("#sequence-diagram"),state=document.querySelector("#state");
   container.replaceChildren();state.textContent="Pele pending";
   let selections=0,sources=0;
   const expanded=new Set(),callbacks={...options,isCurrent:()=>true,onSelect:()=>selections++};
   const outcome=await TrellisPeleSequence.render(container,view,()=>sources++,expanded,callbacks);
   if(outcome.status==="unsupported") {
     TrellisSequence.renderError(container,outcome.reason);
     state.textContent=`This Sequence could not be drawn: ${outcome.reason}`;
   }
   for(const node of container.querySelectorAll("*")) for(const event of [new MouseEvent("click",{bubbles:true}),new KeyboardEvent("keydown",{key:"Enter",bubbles:true})]) node.dispatchEvent(event);
   return {status:outcome.status,reason:outcome.reason,state:state.textContent,
     svg:container.querySelectorAll("svg").length,children:container.childElementCount,
     note:container.querySelector(":scope > p.sequence-empty")?.textContent,
     controls:container.querySelectorAll("button,[role=button],[tabindex]").length,selections,sources};
 },{view,options});
}
function assertErrorState(result,reason) {
 assert.equal(result.status,"unsupported");assert.match(result.reason,reason);
 assert.equal(result.note,`This Sequence could not be drawn: ${result.reason}`);
 assert.equal(result.state,result.note);
 assert.equal(result.svg,0);assert.equal(result.children,1);assert.equal(result.controls,0);
 assert.equal(result.selections,0);assert.equal(result.sources,0);
}

test("24 levels render; a rejected 25th level and an over-512 KiB source show the error state", async()=>{
 const view=fixture("rust"),leaf=structuredClone(view.steps[0]);
 const depth=levels=>{let node={...leaf,id:`depth-${levels-1}`,children:[]};
   for(let n=levels-2;n>=0;n--) node={...leaf,id:`depth-${n}`,kind:"note",children:[node]};
   return node;};
 view.steps=[depth(24)];
 const allowable=await render(view);
 assert.equal(allowable.status,"rendered",allowable.reason);
 view.steps=[depth(25)];
 assertErrorState(await renderWithErrorState(view),/depth/);
 const sourceView=fixture("rust");sourceView.steps=[{...leaf,label:"L".repeat(512*1024),id:"large-source"}];
 assertErrorState(await renderWithErrorState(sourceView,{showDetails:true}),/Mermaid source too large/);
});

// #1007: with no second renderer, a view at the server's structural caps (20 participant
// columns, step depth 24, 200 shown steps: docs/sequence-slice-contract.md) must draw.
function structuralCapsView({columns=20,depth=24,shown=200}={}) {
 const view=resolvedView(),call=view.steps[0],target=view.participants.at(-1);
 view.participants.length=1;view.semantic={tier:"semantic"};
 for(let n=0;n<columns-1;n++) {
   const declaration={...structuredClone(target.declaration),id:`cap:target:${n}`,name:`target_${n}`,path:`target_${n}.rs`};
   view.participants.push({...structuredClone(target),id:declaration.id,label:declaration.name,declaration});
 }
 const measured=n=>({...structuredClone(call),id:`cap:call:${n}`,callId:`cap:occurrence:${n}`,label:`call_${n}`,target:`cap:target:${n%(columns-1)}`,
   callEvidence:{...call.callEvidence,provenanceId:`proof:${n}`}});
 // depth-1 nested branches, each a shown step, around the deepest call; the rest follow at the top.
 let nested=[measured(0)];
 for(let level=depth-2;level>=0;level--) nested=[{...structuredClone(call),id:`cap:branch:${level}`,kind:"branch",label:`guard_${level}`,callId:null,target:null,callEvidence:null,children:nested,alternate:[]}];
 view.steps=[...nested,...Array.from({length:shown-depth},(_,n)=>measured(n+1))];
 return view;
}
test("a view at the structural caps (20 columns, depth 24, 200 shown steps) renders in Pele, in every display",async()=>{
 const view=structuralCapsView(),count=roots=>roots.reduce((sum,step)=>sum+1+count(step.children),0);
 const levels=roots=>roots.length?1+Math.max(...roots.map(step=>levels(step.children))):0;
 assert.equal(count(view.steps),200);assert.equal(levels(view.steps),24);assert.equal(view.participants.length,20);
 for(const [name,options,arrows] of [["Show all steps",{showDetails:true},177],["calls first",{},177],["Interactions",{interactions:true},177]]) {
   const result=await render(view,options);
   assert.equal(result.status,"rendered",`${name}: ${result.reason}`);
   assert.equal(await page.locator("#sequence-diagram svg.pele.trellis-pele").count(),1,name);
   assert.equal(await page.locator(".pele-actor:not(.pele-actor-footer)").count(),20,`${name}: every column is drawn`);
   assert.equal(await page.locator(".pele-edge-label[data-trellis-step-id]").count(),arrows,`${name}: every call is an arrow`);
   assert.equal(await page.locator("#sequence-diagram > p.sequence-empty").count(),0,name);
   const projected=await page.evaluate(({view,options})=>{const p=TrellisPeleSequence.project(view,new Set(),options);return {rows:p.rows.length,lanes:p.lanes.length,frames:p.closeFrames.length};},{view,options});
   // Interactions draw arrows and frames only; the other displays give every shown step a row.
   assert.deepEqual(projected,{rows:options.interactions?177:200,lanes:20,frames:23},name);
 }
 // One more column, level or shown step is past a cap, so the view above is at each of them.
 for(const [over,reason] of [[{columns:21},/Too many sequence participants/],[{depth:25},/Unsupported sequence depth/],[{shown:201},/Too many source notes/]])
   assert.match((await render(structuralCapsView(over),{showDetails:true})).reason||"",reason);
 assert.deepEqual(violations.filter(error=>/Content Security Policy|CSP/.test(error)),[]);
});

test("12,000 SVG nodes allowed; 12,001 reject with no partial SVG or selection, then the error state",async()=>{
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
     const boundary=await TrellisPeleSequence.render(container,view,()=>sources++,new Set(),{onSelect:()=>selections++});
     const installed=container.firstElementChild;
     const noteIds=[...container.querySelectorAll('.pele-note[data-trellis-step-id]')].map(n=>n.dataset.trellisStepId);
     window.pele={render:()=>({svg:make(12001)})};
     const overflow=await TrellisPeleSequence.render(container,view,()=>sources++,new Set(),{onSelect:()=>selections++});
     const kept=installed===container.firstElementChild;
     TrellisSequence.renderError(container,overflow.reason);
     return {boundary:boundary.status,overflow:overflow.status,reason:overflow.reason,
       kept,notOld:installed!==prior,nodes:installed.querySelectorAll('*').length+1,
       noteIds,selections,sources,svg:container.querySelectorAll("svg").length,
       children:container.childElementCount,text:container.textContent};
   }finally{window.pele=original;}
 },view);
 assert.equal(result.boundary,"rendered");assert.equal(result.nodes,12000);
 assert.equal(result.overflow,"unsupported");assert.match(result.reason,/SVG element/);
 assert.equal(result.kept,true);assert.equal(result.notOld,true);
 assert.deepEqual(result.noteIds,steps(view.steps).map(step=>step.id));
 assert.equal(result.svg,0);assert.equal(result.children,1);
 assert.equal(result.text,`This Sequence could not be drawn: ${result.reason}`);
 assert.equal(result.selections,0);assert.equal(result.sources,0);
});

test("a currently pinned synchronous Pele failure shows the error state with its reason",async()=>{
 const view=fixture("rust"),original=await page.evaluateHandle(()=>window.pele);
 await page.evaluate(()=>{window.peleStarted=false;window.pele={render:()=>{window.peleStarted=true;throw new Error("Pele renderer failed");}};});
 try {
   const result=await renderWithErrorState(view);
   assert.equal(await page.evaluate(()=>window.peleStarted),true);
   assertErrorState(result,/^Pele renderer failed$/);
 } finally { await page.evaluate(original=>{window.pele=original;},original); }
});

test("an unavailable Pele asset shows the error state, and the next render with it draws again",async()=>{
 const view=fixture("rust"),original=await page.evaluateHandle(()=>window.pele);
 await page.evaluate(()=>{window.pele=undefined;});
 try { assertErrorState(await renderWithErrorState(view),/^Pele asset unavailable$/); }
 finally { await page.evaluate(original=>{window.pele=original;},original); }
 const again=await render(view);assert.equal(again.status,"rendered",again.reason);
 assert.equal(await page.locator("#sequence-diagram > p.sequence-empty").count(),0);
});

test("calls-first keeps conditional and unknown context, exact selection and admitted detail", async () => {
 const view=fixture("rust"),template=view.steps[0];
 const leaf=(id,kind,label=id,extra={})=>({...structuredClone(template),id,kind,label,callId:null,target:undefined,callEvidence:undefined,children:[],alternate:[],hidden:false,...extra});
 const call=id=>leaf(id,"call",id,{callId:id});
 view.steps=[call("prepare"),leaf("write","effect","write: state = ready"),leaf("definition","definition","define callback · body deferred"),
  leaf("nested","boundary","Nested callable boundary: body does not execute at definition; captures are not expanded."),
  leaf("condition","branch","if ready",{children:[call("ready"),leaf("return","return","return result")],alternate:[call("fallback"),leaf("write-alt","effect","write: fallback")]}),
  leaf("loop","loop","loop_expression: possible iterations, not unrolled; exit/termination unknown",{children:[call("repeat"),leaf("write-loop","effect","write: iteration")]}),
  leaf("unknown","boundary","Unsupported complex control: possible behavior remains unknown; body not expanded."),
  leaf("continuation","branch","only if the preceding path continues normally",{children:[call("finish")]}),leaf("last-return","return","return final_value")];
 const result=await page.evaluate(async view=>{
  const walk=roots=>roots.flatMap(step=>[step,...walk(step.children),...walk(step.alternate)]),original=walk(view.steps),before=JSON.stringify(view);
  const freeze=value=>{if(value&&typeof value==="object"){Object.freeze(value);Object.values(value).forEach(freeze);}return value;};freeze(view);
  const container=document.querySelector("#sequence-diagram"),results=[];
  {
   const renderer=TrellisPeleSequence,selected=[],options={onSelect:step=>selected.push(step)},expanded=new Set();
   const outcome=await renderer.render(container,view,null,expanded,options);
   if(outcome.status!=="rendered")throw Error(outcome.reason);
   const selector=".pele-note[data-trellis-step-id],.pele-edge-label[data-trellis-step-id]";
   const nodes=[...container.querySelectorAll(selector)],ids=nodes.map(node=>node.dataset.trellisStepId);
   for(const node of nodes)node.dispatchEvent(new KeyboardEvent("keydown",{key:"Enter",bubbles:true}));
   const exact=selected.every((step,i)=>step===original.find(original=>original.id===ids[i]));
   const unknown=container.textContent.includes("behavior unknown"),exit=container.textContent.includes("may exit early");
   await renderer.render(container,view,null,expanded,{...options,showDetails:true});
   const restored=[...container.querySelectorAll(selector)].map(node=>node.dataset.trellisStepId);
   results.push({ids,exact,unknown,exit,restored,fullWrite:container.textContent.includes("write: state = ready"),unchanged:JSON.stringify(view)===before});
  }
  return {results,original:original.map(step=>step.id)};
 },view);
 if(process.env.TRELLIS_SEQUENCE_SHOTS) {
  await page.locator("#state").evaluate(node=>node.textContent="");
  await page.setViewportSize({width:1440,height:1000});
  await render(view);
  await page.screenshot({path:join(process.env.TRELLIS_SEQUENCE_SHOTS,"calls-first.png"),fullPage:true});
  await render(view,{showDetails:true});
  await page.screenshot({path:join(process.env.TRELLIS_SEQUENCE_SHOTS,"all-steps.png"),fullPage:true});
 }
 assert.equal(result.original.length,16);
 for(const rendered of result.results) {
  assert.deepEqual(rendered.ids,["prepare","condition","ready","fallback","loop","repeat","unknown","continuation","finish"]);
  assert.ok(rendered.exact);assert.ok(rendered.unknown);assert.ok(rendered.exit);assert.ok(rendered.fullWrite);assert.ok(rendered.unchanged);
  assert.deepEqual(rendered.restored,result.original);
 }
});
