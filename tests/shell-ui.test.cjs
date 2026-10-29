"use strict";
const test = require("node:test");
const ROOT = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SCOPED = `/api/checkouts/${ROOT}`;
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
function harness(withApp = false) {
  const elements = new Map(), requests = [], timers = new Map(); let timer = 0;
  function node(tagName) {
    const classes = new Set();
    return {tagName, children:[], attrs:{}, listeners:{}, textContent:"", value:"", hidden:false, disabled:false,
      classList:{add(c){classes.add(c);},remove(c){classes.delete(c);},contains(c){return classes.has(c);},toggle(c, force){const on = force ?? !classes.has(c); if(on)classes.add(c);else classes.delete(c);return on;}},
      setAttribute(k,v){this.attrs[k]=String(v);}, getAttribute(k){return this.attrs[k];},
      append(...items){this.children.push(...items);}, replaceChildren(...items){this.children=items;this.textContent="";},
      addEventListener(type,fn){this.listeners[type]=fn;}, querySelector(){return null;}, focus(){this.focused=true;}, scrollIntoView(){}};
  }
  const get = id => { if(!elements.has(id))elements.set(id,node("div"));return elements.get(id); };
  const document = {getElementById:get,createElement:node,createElementNS:(_,tag)=>node(tag),createTextNode:text=>({...node("#text"),textContent:text}),createDocumentFragment:()=>node("fragment"),body:node("body"),listeners:{},addEventListener(k,v){this.listeners[k]=v;}};
  const context = vm.createContext({document,window:{confirm:()=>true},console,AbortController,DOMException,TextEncoder,URLSearchParams,
    setTimeout(fn){timers.set(++timer,fn);return timer;},clearTimeout(id){timers.delete(id);},
    fetch(path){requests.push(path);throw new Error("Unexpected request "+path);}});
  const run = code => vm.runInContext(code,context);
  if(withApp)run(fs.readFileSync("web/sequence.js","utf8"));
  run(fs.readFileSync("web/shell.js","utf8"));
  if(withApp){run(fs.readFileSync("web/files.js","utf8"));run(fs.readFileSync("web/app.js","utf8"));run(`token="synthetic";selectedRootKey="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";checkouts=[{rootKey:"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",workspaceRoot:"/repo",state:"available"}];status={revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},workspaceRoot:"/repo"};`);}
  if (withApp) {
    let handler = context.fetch;
    Object.defineProperty(context,"fetch",{configurable:true,get:()=>request,set(fn){handler=fn;}});
    const request=(url,...args)=>{
      assert.ok(url.startsWith(`${SCOPED}/`),`Unexpected unscoped/wrong-root route: ${url}`);
      return handler("/api" + url.slice(SCOPED.length),...args);
    };
  }
  return {get,document,context,run,requests,shell:context.window.TrellisShell};
}
const all = node => [node,...node.children.flatMap(all)];
const text = node => all(node).map(n=>n.textContent).join(" ");
const range={startLine:3,startColumn:2,endLine:9,endColumn:7};
const call=(id="entry",target="candidate")=>({id,callId:id,kind:"call",label:`Call full::${id} <script>`,path:"main.rs",range,target,resolution:"unresolved",children:[],alternate:[]});
const view=(steps=[])=>({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},seed:{id:"root",name:"root",path:"main.rs",range},participants:[{id:"root",kind:"method",label:"root"},{id:"candidate",kind:"externalCandidate",label:"Builder",identification:"Lexical type candidate, not resolved dispatch"},{id:"other",kind:"unresolvedReceiver",label:"Chain result"}],steps,warnings:[],hiddenSteps:0});
const response=data=>({ok:true,status:200,json:async()=>data});
const key=(element,value)=>{let prevented=false;element.listeners.keydown({key:value,preventDefault(){prevented=true;}});return prevented;};

test("Architecture owns its metric controls across view and Files transitions", () => {
  const h = harness();
  h.shell.showView("architecture");
  assert.equal(h.get("diagram-metrics").hidden, true);
  h.shell.setSection("files");
  h.shell.setSection("diagrams");
  assert.equal(h.get("diagram-metrics").hidden, true);
  h.shell.showView("sequence");
  assert.equal(h.get("diagram-metrics").hidden, false);
  h.shell.showView("classes");
  assert.equal(h.get("diagram-metrics").hidden, false);
  assert.deepEqual(h.requests, []);
});

test("five workspace tabs have roving keyboard focus and never request source",()=>{
  const h=harness();
  assert.equal(h.get("sequence-panel").hidden,false);assert.equal(h.get("dependency-library").hidden,true);assert.equal(h.get("architecture-panel").hidden,true);
  assert.equal(key(h.get("view-sequence"),"ArrowRight"),true);
  assert.equal(h.get("view-classes").attrs["aria-selected"],"true");
  assert.equal(h.get("classes-panel").hidden,false);
  assert.ok(h.get("view-classes").focused);
  assert.equal(key(h.get("view-classes"),"ArrowRight"),true);
  assert.equal(h.get("view-libraries").attrs["aria-selected"],"true");assert.equal(h.get("view-libraries").tabIndex,0);assert.equal(h.get("view-sequence").tabIndex,-1);
  assert.equal(h.get("dependency-library").hidden,false);assert.ok(h.get("view-libraries").focused);
  key(h.get("view-classes"),"End");assert.equal(h.get("dependency-library").hidden,false);
  key(h.get("view-libraries"),"ArrowRight");assert.equal(h.get("architecture-panel").hidden,false);assert.ok(h.get("view-architecture").focused);
  key(h.get("view-architecture"),"ArrowRight");assert.equal(h.get("treemap-panel").hidden,false);
  key(h.get("view-treemap"),"ArrowRight");assert.equal(h.get("sequence-panel").hidden,false);
  key(h.get("view-sequence"),"ArrowLeft");assert.equal(h.get("treemap-panel").hidden,false);
  key(h.get("view-libraries"),"Home");assert.equal(h.get("architecture-panel").hidden,false);
  for (const name of ["tools", "terminal"]) h.shell.showView(name);
  assert.equal(h.get("architecture-panel").hidden,false);
  for (const name of ["architecture", "treemap", "sequence", "classes", "libraries"]) {
    assert.equal(h.get(`view-${name}`).attrs["aria-selected"],String(name === "architecture"));
    assert.equal(h.get(`view-${name}`).tabIndex,name === "architecture" ? 0 : -1);
  }
  assert.deepEqual(h.requests,[]);
});
test("workspace tabs have exactly five valid ARIA panel relationships",()=>{
  const html=fs.readFileSync(path.join(__dirname,"../web/index.html"),"utf8");
  const tabs=[...html.matchAll(/<button id="view-([^"]+)" role="tab"[^>]*aria-controls="([^"]+)"/g)];
  assert.deepEqual(tabs.map(match=>match[1]),["architecture","treemap","sequence","classes","libraries"]);
  for(const [,name,panel] of tabs) {
    assert.match(html,new RegExp(`<section id="${panel}"[^>]*role="tabpanel"[^>]*aria-labelledby="view-${name}"`));
  }
});
test("source dock only changes presentation and can switch/close by keyboard",()=>{
  const h=harness();assert.equal(h.get("source-dock").hidden,true);
  h.shell.showSource("workspace");assert.equal(h.get("workspace-source-panel").hidden,false);
  key(h.get("dock-workspace"),"ArrowRight");assert.equal(h.get("library-source-panel").hidden,false);
  assert.equal(h.get("workspace-source-panel").hidden,true);
  h.document.listeners.keydown({key:"Escape"});assert.equal(h.get("source-dock").hidden,true);
  assert.deepEqual(h.requests,[]);
});
test("inspector uses safe original evidence, never claims candidate confidence",()=>{
  const h=harness(),step=call();let opened=0;const original=JSON.stringify(step);
  h.shell.selectStep(step,view([step]),()=>opened++);
  assert.equal(opened,0);assert.equal(h.get("inspector-title").textContent,step.label);assert.equal(h.get("inspector-clear").disabled,false);
  assert.match(h.get("inspector-target").textContent,/Terminal syntax only · no verified target/);
  assert.match(h.get("inspector-location").textContent,/main.rs:3:2–9:7 · revision 12345678:1/);
  assert.match(h.get("inspector-evidence").textContent,/not a runtime trace or verified dispatch/);
  assert.doesNotMatch(text(h.get("inspector-content"))+text(h.get("inspector-detail")),/confidence.*1\.0/);
  assert.equal(all(h.get("inspector-detail")).some(n=>n.tagName==="script"),false);
  h.get("inspector-open-source").listeners.click();assert.equal(opened,1);assert.equal(JSON.stringify(step),original);
});
test("declaration Inspect uses measured symbol text and pinned revision without reading source",()=>{
  const h=harness(),symbol={id:"declaration",name:"<script>declaration</script>",kind:"method",path:"src/<tag>.rs",range,provenance:{kind:"syntax"}},revision=view().revision;
  let opened=0;const original=JSON.stringify(symbol);
  h.context.window.matchMedia=()=>({matches:true});
  h.get("explorer-toggle").listeners.click();
  h.shell.inspectSymbol(symbol,revision,()=>++opened);
  assert.equal(opened,0);
  assert.equal(h.get("inspector-title").textContent,symbol.name);
  assert.equal(h.get("inspector-title").title,symbol.name);
  assert.equal(h.get("inspector-kind").textContent,"method");
  assert.equal(h.get("inspector-location").textContent,"src/<tag>.rs:3:2–9:7 · revision 12345678:1");
  assert.equal(h.get("inspector-target").textContent,"Declaration only · no call target or verified dispatch");
  assert.match(h.get("inspector-evidence").textContent,/Measured declaration syntax/);
  assert.equal(h.get("inspector-content").hidden,false);
  assert.equal(h.get("inspector-clear").disabled,false);
  assert.equal(h.document.body.classList.contains("explorer-open"),false);
  assert.equal(h.document.body.classList.contains("inspector-open"),true);
  assert.equal(h.get("inspector-open-source").textContent,"Read declaration source");
  assert.equal(h.get("inspector-open-source").disabled,false);
  assert.equal(h.get("inspector-detail").children[0].tagName,"details");
  assert.notEqual(h.get("inspector-detail").children[0].open,true);
  assert.match(text(h.get("inspector-detail")),/<script>declaration<\/script>/);
  assert.equal(all(h.get("inspector-detail")).some(n=>n.tagName==="script"),false);
  assert.equal(h.get("inspector-open-source").listeners.click(),1);
  assert.equal(JSON.stringify(symbol),original);
  assert.deepEqual(h.requests,[]);
});
test("declaration source callbacks are replaced and erased by existing inspector resets",()=>{
  const h=harness(),symbol={id:"declaration",name:"declared",kind:"function",path:"main.rs",range};let first=0,second=0,step=0;
  h.shell.inspectSymbol(symbol,view().revision,()=>first++);
  h.shell.inspectSymbol({...symbol,name:"next"},view().revision,()=>second++);
  h.get("inspector-open-source").listeners.click();
  assert.equal(first,0);assert.equal(second,1);
  h.shell.selectStep(call(),view(),()=>step++);
  assert.equal(h.get("inspector-open-source").textContent,"Open call site");
  h.get("inspector-open-source").listeners.click();assert.equal(step,1);assert.equal(second,1);
  for(const reset of [()=>h.shell.resetInspector(),()=>h.shell.setConnected(false),()=>h.get("inspector-clear").listeners.click()]){
    h.shell.inspectSymbol(symbol,view().revision,()=>second++);reset();
    h.get("inspector-open-source").listeners.click();
    assert.equal(second,1);
    assert.equal(h.get("inspector-content").hidden,true);
    assert.equal(h.get("inspector-open-source").disabled,true);
    assert.equal(h.get("inspector-detail").children.length,0);
  }
  assert.deepEqual(h.requests,[]);
});
test("declaration Inspect requires source witness, paired revision and explicit callback",()=>{
  const h=harness(),symbol={id:"declaration",name:"declared",kind:"class",path:"main.rs",range};let opened=0;
  for(const [item,revision,callback] of [
    [{...symbol,path:""},view().revision,()=>opened++],
    [{...symbol,range:{startLine:4,endLine:2}},view().revision,()=>opened++],
    [symbol,null,()=>opened++],
    [symbol,{indexGeneration:"",indexRevision:1},()=>opened++],
    [symbol,{indexGeneration:view().revision.indexGeneration,indexRevision:1.5},()=>opened++],
    [symbol,view().revision,null]
  ]){
    h.shell.inspectSymbol(item,revision,callback);
    assert.equal(h.get("inspector-open-source").disabled,true);
    h.get("inspector-open-source").listeners.click();
  }
  h.shell.inspectSymbol(null,view().revision,()=>opened++);
  assert.equal(h.get("inspector-content").hidden,true);
  assert.equal(opened,0);
  assert.deepEqual(h.requests,[]);
});
test("group keeps first-entry target limitation, full source callback and all guard/alternate children",()=>{
  const h=harness(),group={id:"chain",kind:"group",label:"Original full chain",path:"main.rs",range,children:[call(),call("finish","other")],alternate:[]};let selected;
  h.shell.selectStep(group,view([group]),()=>{selected=group;});
  assert.match(h.get("inspector-title").textContent,/\+1 chain calls/);assert.match(text(h.get("inspector-detail")),/Original full chain/);
  assert.match(h.get("inspector-target").textContent,/First measured entry call only/);
  assert.match(h.get("inspector-target").textContent,/different receivers and return types/);
  assert.match(text(h.get("inspector-detail")),/full::finish/);
  h.get("inspector-open-source").listeners.click();assert.equal(selected,group);assert.equal(selected.range,range);
  const guard={...group,kind:"branch",label:"if !ready && original_guard",children:[{...call(),guard:"original guard text",hidden:true}],alternate:[{...call("exit"),kind:"return",label:"return original alternate"}]};
  h.shell.selectStep(guard,view([guard]),()=>{});
  assert.match(text(h.get("inspector-detail")),/original guard text/);assert.match(text(h.get("inspector-detail")),/return original alternate/);
  assert.doesNotMatch(h.get("inspector-target").textContent,/First measured entry/);
});
test("inspector reset and disconnect erase source callback and workspace metadata",()=>{
  const h=harness();let opened=0;h.shell.updateWorkspace({workspaceRoot:"/a/repo",revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:7},stats:{files:10}});
  assert.equal(h.get("workspace-name").textContent,"repo");
  assert.match(h.get("workspace-meta").textContent,/Revision 12345678:7/);
  h.shell.selectStep(call(),view(),()=>opened++);h.shell.resetInspector();h.get("inspector-open-source").listeners.click();
  assert.equal(opened,0);assert.equal(h.get("inspector-open-source").disabled,true);assert.equal(h.get("inspector-clear").disabled,true);assert.equal(h.get("inspector-content").hidden,true);
  h.shell.selectStep(call(),view(),()=>opened++);h.shell.showSource("workspace");h.shell.setConnected(false);h.get("inspector-open-source").listeners.click();
  assert.equal(opened,0);assert.equal(h.get("source-dock").hidden,true);assert.equal(h.get("workspace-name").textContent,"No workspace");
});
test("responsive drawers use classes rather than hiding desktop panels",()=>{
  const h=harness();h.context.window.matchMedia=()=>({matches:true});h.get("explorer-toggle").listeners.click();assert.ok(h.document.body.classList.contains("explorer-open"));
  h.get("inspector-toggle").listeners.click();assert.equal(h.get("inspector-toggle").attrs["aria-expanded"],"true");
  h.document.listeners.keydown({key:"Escape"});assert.equal(h.get("inspector-toggle").attrs["aria-expanded"],"false");
  assert.equal(h.get("call-inspector").hidden,false);
});
async function selectedHarness() {
  const h=harness(true),step=call(),data=view([step]);
  h.context.fetch=async path=>{h.requests.push(path);if(path==="/api/sequence")return response(data);if(path==="/api/status")return response({revision:data.revision,workspaceRoot:"/repo"});if(path.startsWith("/api/source?"))return response({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},file:{path:"main.rs",text:"line1\nline2\nline3"}});throw new Error("Unexpected "+path);};
  await h.run(`selectMethod(${JSON.stringify(data.seed)})`);
  const pick=all(h.get("sequence-diagram")).find(n=>n.attrs.class?.split(" ").includes("sequence-source"));assert.ok(pick);
  pick.listeners.click();
  return h;
}
test("app row selection inspects without fetch; explicit Open call site reads guarded snapshot",async()=>{
  const h=await selectedHarness();assert.deepEqual(h.requests,["/api/sequence","/api/status"]);
  assert.equal(h.get("inspector-content").hidden,false);
  await h.get("inspector-open-source").listeners.click();assert.equal(h.requests.length,4);assert.match(h.requests[2],/^\/api\/source\?/);assert.equal(h.requests[3],"/api/status");
  assert.equal(h.get("source-dock").hidden,false);assert.equal(h.get("workspace-source-panel").hidden,false);
});
for(const change of ["method","revision","disconnect","index"] ) test(`app invalidates inspector callbacks after ${change}`,async()=>{
  const h=await selectedHarness(),open=h.get("inspector-open-source").listeners.click;
  if(change==="method")await h.run(`selectMethod({id:"new",name:"new",path:"main.rs",range:{startLine:1,endLine:1}})`);
  if(change==="revision")h.run(`status.revision={...status.revision,indexRevision:2};clearBrowse();`);
  if(change==="disconnect")h.get("logout").listeners.click();
  if(change==="index") {h.context.fetch=async()=>response({id:"job",state:"running"});await h.get("index").listeners.click();}
  await open();assert.equal(h.requests.some(p=>p.startsWith("/api/source?")),false);
  assert.equal(h.get("inspector-content").hidden,true);
});
test("captured app source callback rejects revision/session changes even without a shell reset",async()=>{
  const h=harness(true);let callback;
  h.context.window.TrellisShell.selectStep=(_,__,source)=>{callback=source;};
  h.context.fetch=async path=>{if(path==="/api/status")return response({revision:view().revision,workspaceRoot:"/repo"});assert.equal(path,"/api/sequence");return response(view([call()]));};await h.run(`selectMethod(${JSON.stringify(view().seed)})`);
  all(h.get("sequence-diagram")).find(n=>n.attrs.class?.split(" ").includes("sequence-source")).listeners.click();
  h.context.fetch=()=>{throw new Error("Stale source fetch");};h.run("status.revision=2;epoch++;");
  await callback();assert.equal(h.get("source-dock").hidden,true);
});

test("in-flight call source cannot reopen the dock after a new method clears it",async()=>{
  const h=await selectedHarness();let resolve;
  h.context.fetch=()=>new Promise(done=>{resolve=done;});
  const pending=h.get("inspector-open-source").listeners.click();
  h.run("clearBrowse();clearSource();");
  resolve(response({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},file:{path:"main.rs",text:"obsolete"}}));await pending;
  assert.equal(h.get("source-dock").hidden,true);assert.doesNotMatch(text(h.get("source")),/obsolete/);
});
test("successful candidate source opens the library dock without changing method or active view",async()=>{
  const h=harness(true),root={id:"rust",label:"Rust"};
  h.run(`selectedMethod={id:"kept"};`);h.shell.showView("libraries");
  h.context.fetch=async()=>response({rootId:"rust",rootLabel:"Rust",path:"lib.rs",hash:"hash",file:{text:"candidate"},definitions:[],warnings:[]});
  await h.run(`loadExternalFile(${JSON.stringify(root)},"lib.rs")`);
  assert.equal(h.get("library-source-panel").hidden,false);assert.equal(h.get("source-dock").hidden,false);
  assert.equal(h.get("dependency-library").hidden,false);assert.equal(h.run("selectedMethod.id"),"kept");
});

for (const mode of ["manual", "catalog"]) test(`${mode} source loading/failure stays visible when source dock is closed`,async()=>{
  const h=harness(true);h.shell.showView("libraries");let resolve;
  h.context.fetch=()=>new Promise(done=>{resolve=done;});
  if(mode==="catalog")h.run(`dependencyCatalog={catalogId:"catalog",state:"ready"};dependencyPackage={id:"pkg"};`);
  const pending=h.run(mode==="manual" ? `loadExternalFile({id:"rust",label:"Rust"},"lib.rs")` : `loadDependencySource({id:"symbol",packageId:"pkg",name:"open",sourceRef:"ref",path:"lib.rs"})`);
  const state=h.get(mode==="manual"?"external-state":"dependency-symbol-state");
  assert.match(state.textContent,/Loading candidate/);assert.equal(h.get("source-dock").hidden,true);
  resolve({ok:false,status:409,json:async()=>({error:{message:"Candidate expired"}})});await pending;
  assert.match(state.textContent,/Candidate expired/);assert.equal(h.get("source-dock").hidden,true);assert.equal(h.get("dependency-library").hidden,false);
});

test("closing a library dock restores invoking control or active tab, not disabled method source",()=>{
  const h=harness(),invoker=h.get("definition");h.shell.showView("libraries");h.get("method-source").disabled=true;
  h.document.activeElement=invoker;h.shell.showSource("library");h.get("source-dock-close").listeners.click();assert.ok(invoker.focused);
  invoker.hidden=true;h.shell.showSource("library");h.get("source-dock-close").listeners.click();assert.ok(h.get("view-libraries").focused);
  h.shell.selectStep(call(),view(),()=>{});h.shell.showSource("library");h.document.listeners.keydown({key:"Escape"});assert.equal(h.get("source-dock").hidden,true);
});


test("inspector keeps raw evidence folded and source action before full details", () => {
  const html = fs.readFileSync(path.join(__dirname, "../web/index.html"), "utf8");
  assert.ok(html.indexOf('id="inspector-open-source"') < html.indexOf('id="inspector-detail"'));
  const source = fs.readFileSync(path.join(__dirname, "../web/shell.js"), "utf8");
  assert.doesNotMatch(source, /evidence\(step, detail[^;]+\.open = true/);
  assert.match(source, /"target", "resolution", "candidateSymbols"/);
});


test("mobile drawers are exclusive and explicit source exposes the dock", () => {
  const h = harness();
  h.context.window.matchMedia = () => ({matches:true});
  h.get("explorer-toggle").listeners.click();
  assert.equal(h.document.body.classList.contains("explorer-open"), true);
  h.shell.selectStep(call(), view(), () => {});
  assert.equal(h.document.body.classList.contains("explorer-open"), false);
  assert.equal(h.document.body.classList.contains("inspector-open"), true);
  h.shell.showSource("workspace");
  assert.equal(h.document.body.classList.contains("inspector-open"), false);
  assert.equal(h.get("source-dock").hidden, false);
  assert.equal(h.get("dock-workspace").focused, true);
  h.get("explorer-toggle").listeners.click();
  h.shell.showView("sequence");
  assert.equal(h.document.body.classList.contains("explorer-open"), false);
});


test("desktop source clears latent drawers before a mobile resize", () => {
  const h = harness();
  h.context.window.matchMedia = () => ({matches:false});
  h.shell.selectStep(call(), view(), () => {});
  assert.equal(h.document.body.classList.contains("inspector-open"), true);
  h.shell.showSource("workspace");
  h.context.window.matchMedia = () => ({matches:true});
  assert.equal(h.document.body.classList.contains("inspector-open"), false);
  assert.equal(h.document.body.classList.contains("explorer-open"), false);
  assert.equal(h.get("source-dock").hidden, false);
});

test("inspector refuses source callback for malformed ranges or missing paired snapshot",()=>{
 const h=harness(),malformed=[{...call("missing"),path:""},
  {...call("empty"),range:{}},{...call("inverted"),range:{startLine:4,endLine:2}},
  {...call("fraction"),range:{startLine:1.5,endLine:2}}];
 let opened=0;
 for(const step of malformed){h.shell.selectStep(step,view([step]),()=>opened++);
  assert.equal(h.get("inspector-open-source").disabled,true);
  h.get("inspector-open-source").listeners.click();}
 const good=call("good");h.shell.selectStep(good,{...view([good]),revision:null},()=>opened++);
 assert.equal(h.get("inspector-open-source").disabled,true);
 assert.equal(opened,0);
});

const branchGit = (upstream = null) => ({state:"available",head:{kind:"branch",name:"feature/<script>",commit:"abcdef123456",shortCommit:"abcdef1"},upstream});
test("Git header shows measured branch, last-fetch counts, detached HEAD and unavailable without inventing a branch",()=>{
  const h=harness();
  for (const [git,expected] of [
    [branchGit({name:"origin/main",ahead:2,behind:3,basis:"last_fetch"}),"feature/<script> · origin/main ↑2 ↓3 · As of last fetch"],
    [branchGit(),"feature/<script> · No upstream"],
    [{state:"available",head:{kind:"detached",name:null,commit:"abcdef123456",shortCommit:"abcdef1"},upstream:null},"Detached HEAD abcdef1 · No upstream"],
    [{state:"unavailable",reason:"not_repository"},"Git unavailable"],
    [undefined,"Git unavailable"]
  ]) {
    h.shell.updateCheckout("/a/repo",git);
    assert.equal(h.get("workspace-git").textContent,expected);
    assert.equal(h.get("workspace-git").title,expected);
    assert.equal(h.get("workspace-git").hidden,false);
    assert.equal(all(h.get("workspace-git")).some(n=>n.tagName==="script"),false);
    h.shell.updateWorkspace(null);
    assert.equal(h.get("workspace-git").textContent,expected,"cold index must not erase independent Git");
    assert.equal(h.get("workspace-name").textContent,"repo");
  }
  h.shell.setConnected(false);
  assert.equal(h.get("workspace-git").textContent,"");
  assert.equal(h.get("workspace-git").hidden,true);
  assert.deepEqual(h.requests,[]);
});

test("Files is a primary source section without automatic reads or drawers and preserves the diagram view", () => {
  const h = harness();
  h.context.window.matchMedia = () => ({matches:true});
  h.shell.showView("libraries");
  h.shell.showMetricInspector();
  h.get("section-files").listeners.click();
  assert.equal(h.get("file-filter").focused,true);
  assert.equal(h.document.body.classList.contains("files-section"),true);
  assert.equal(h.document.body.classList.contains("explorer-open"),false);
  assert.equal(h.document.body.classList.contains("inspector-open"),false);
  assert.equal(h.get("section-files").attrs["aria-current"],"page");
  for (const id of ["diagram-tabs", "diagram-metrics", "sequence-panel", "classes-panel", "dependency-library", "metric-inspector"]) {
    assert.equal(h.get(id).hidden,true,`${id} is not shown over Files`);
  }
  assert.equal(h.get("source-dock").hidden,false);
  assert.equal(h.get("workspace-source-panel").hidden,false);
  assert.equal(h.get("library-source-panel").hidden,true);
  assert.equal(h.get("files-inspector").hidden,false);
  h.document.listeners.keydown({key:"Escape"});
  assert.equal(h.get("source-dock").hidden,false,"Escape does not close primary Files source");
  h.get("section-diagrams").listeners.click();
  assert.equal(h.document.body.classList.contains("files-section"),false);
  assert.equal(h.get("section-diagrams").attrs["aria-current"],"page");
  assert.equal(h.get("dependency-library").hidden,false);
  assert.equal(h.get("view-libraries").focused,true);
  assert.equal(h.get("files-inspector").hidden,true);
  assert.deepEqual(h.requests,[]);
});

test("Clear removes metric exception and drawers, notifies the controller, and erases call source callbacks", () => {
  const h = harness(); let cleared = 0, opened = 0;
  h.shell.setInspectorClearCallback(() => cleared++);
  h.shell.selectStep(call(),view(),() => opened++);
  h.shell.showMetricInspector();
  h.get("metric-inspector-title").textContent="main.rs";
  h.get("metric-inspector-values").append(h.get("synthetic-value"));
  assert.equal(h.document.body.classList.contains("metric-inspector-active"),true);
  h.get("inspector-clear").listeners.click();
  assert.equal(cleared,1);
  assert.equal(h.document.body.classList.contains("metric-inspector-active"),false);
  assert.equal(h.document.body.classList.contains("inspector-open"),false);
  assert.equal(h.get("metric-inspector").hidden,true);
  assert.equal(h.get("metric-inspector-title").textContent,"");
  assert.equal(h.get("metric-inspector-values").children.length,0);
  assert.equal(h.get("inspector-clear").disabled,true);
  h.get("inspector-open-source").listeners.click();
  assert.equal(opened,0);
  h.shell.selectStep(call(),view(),() => opened++);
  h.get("inspector-clear").listeners.click();
  h.get("inspector-open-source").listeners.click();
  assert.equal(cleared,2);
  assert.equal(opened,0);
  assert.equal(h.get("inspector-open-source").disabled,true);
  assert.deepEqual(h.requests,[]);
});

test("Treemap owns its controls and inspector across view and Files transitions", () => {
  const h = harness();
  h.shell.showView("treemap");
  assert.equal(h.get("treemap-panel").hidden, false);
  assert.equal(h.get("diagram-metrics").hidden, true);
  h.shell.showTreemapInspector();
  assert.equal(h.get("treemap-inspector").hidden, false);
  h.shell.showArchitectureInspector();
  assert.equal(h.get("treemap-inspector").hidden, true);
  h.shell.showTreemapInspector();
  assert.equal(h.get("architecture-inspector").hidden, true);
  h.shell.setSection("files");
  assert.equal(h.get("treemap-panel").hidden, true);
  assert.equal(h.get("treemap-inspector").hidden, true);
  h.shell.setSection("diagrams");
  assert.equal(h.get("treemap-panel").hidden, false);
  assert.equal(h.get("diagram-metrics").hidden, true);
  h.shell.showView("sequence");
  assert.equal(h.get("diagram-metrics").hidden, false);
});
