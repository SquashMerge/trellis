"use strict";
// Synthetic data only. Run: node --test tests/dependency-ui.test.cjs
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const source = ["sequence.js", "shell.js", "app.js"].map(file => fs.readFileSync(path.join(__dirname, "../web", file), "utf8")).join("\n");

function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return {promise, resolve, reject};
}
const ROOT = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SCOPED = `/api/checkouts/${ROOT}`;
function harness() {
  const elements = new Map(), blobs = [], downloads = [], timers = new Map();
  let nextTimer = 0;
  function node(tagName) {
    const classes = new Set();
    return {tagName, classList: {add(value) { classes.add(value); }, remove(value) { classes.delete(value); }, contains(value) { return classes.has(value); }, toggle(value, force) { const on = force ?? !classes.has(value); if (on) classes.add(value); else classes.delete(value); return on; }}, focus() {}, hidden: true, disabled: false, value: "", textContent: "", children: [], listeners: {},
      addEventListener(type, handler) { this.listeners[type] = handler; },
      replaceChildren(...items) { this.children = items; },
      append(...items) { this.children.push(...items); },
      querySelector() { return null; },
      attrs: {}, setAttribute(key, value) { this.attrs[key] = String(value); }, remove() {},
      click() { downloads.push(this.download); }};
  }
  function get(id) { if (!elements.has(id)) elements.set(id, node()); return elements.get(id); }
  const context = vm.createContext({console, TextEncoder, DOMException, Blob, AbortController,
    setTimeout(callback, delay) { const id = ++nextTimer; timers.set(id, {callback, delay}); return id; }, clearTimeout(id) { timers.delete(id); },
    window: {confirm: () => true},
    URL: {createObjectURL(blob) { blobs.push(blob); return "blob:synthetic"; }, revokeObjectURL() {}},
    document: {addEventListener() {}, getElementById: get, createElement: node, createElementNS: (ns, tag) => node(tag), createDocumentFragment: node, createTextNode: text => ({textContent:text}), body: node()},
    fetch() { throw new Error("Unexpected request"); }});
  const run = code => vm.runInContext(code, context);
  run(source);
  run(`token = 'synthetic'; selectedRootKey = '${ROOT}'; checkouts = [{rootKey:'${ROOT}',workspaceRoot:'/synthetic',state:'available',active:false}]; status = {revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1}};`);
  // The app receives the production scoped URL; legacy suffixes below remain the
  // fixture handlers' semantic route names, never an unscoped network fallback.
  let handler = context.fetch;
  const networkUrls = [];
  Object.defineProperty(context, "fetch", {configurable:true, get:() => request, set(fn) {handler = fn;}});
  const request = (url, ...args) => {
    networkUrls.push(url);
    if (url === "/api/checkouts" || url === "/api/daemon/status") {
      assert.equal(args[0]?.method, "GET");
      return handler(url, ...args);
    }
    assert.ok(url.startsWith(`${SCOPED}/`), `Unexpected unscoped or wrong-root request: ${url}`);
    const result = handler("/api" + url.slice(SCOPED.length), ...args);
    if (!url.startsWith(`${SCOPED}/status`)) return result;
    return Promise.resolve(result).then(reply => reply.ok ? {...reply, json:async () => ({workspaceRoot:"/synthetic", ...await reply.json()})} : reply);
  };
  return {context, run, get, blobs, downloads, timers, networkUrls};
}



const response = data => ({ok:true,status:200,json:async()=>data});
function descendants(node) { return [node, ...(node.children || []).flatMap(descendants)]; }
function text(node) { return descendants(node).map(n=>n.textContent).join(" "); }

const pkg = {id:"cargo:thing@1", ecosystem:"cargo", name:"thing", version:"1.2.3", source:"registry", aliases:["thing_alias"], sourceState:"present", indexState:"partial", warnings:["cfg unknown <script>"]};
const symbol = {id:"decl",packageId:pkg.id,name:"Client",qualifiedName:"thing::Client",kind:"struct",signature:"pub struct Client",sourceRef:"immutable-ref",path:"src/lib.rs",range:{startLine:1,endLine:1}};
const catalog = (id="catalog-1") => ({state:"ready",workspaceRevision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},catalogId:id,packages:[pkg],symbolCount:201,warnings:["Syntax candidates only <b>not semantic</b>"]});
const symbols = (items=[symbol], nextOffset=null, id="catalog-1") => ({catalogId:id,workspaceRevision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},items,nextOffset});
const snapshot = () => ({id:symbol.sourceRef,rootId:pkg.id,rootLabel:"thing 1.2.3",path:symbol.path,hash:"hash123",file:{text:"pub struct Client;\n<script>unsafe</script>\n"+"line\n".repeat(1298)},definitions:[symbol],warnings:["Candidate only"]});
const click = (container,label) => {
 const item=descendants(container).find(n=>n.tagName==="button" && n.textContent===label);
 assert.ok(item, `Missing button ${label}`); return item.listeners.click();
};
async function ready(h) {h.context.fetch=async()=>response(catalog());await h.run("refreshDependencies()");}
async function select(h) {await ready(h);h.context.fetch=async()=>response(symbols());await h.run(`selectDependencyPackage(${JSON.stringify(pkg)})`);}

test("automatic workspace status refresh requests only catalog metadata; optional failures do not block",async()=>{
 const h=harness(), requests=[];
 h.context.fetch=async url=>{requests.push(url);if(url==="/api/status")return response({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},stats:{}});if(url==="/api/dependencies")return response(catalog());if(url.startsWith("/api/tree?"))return response({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},path:"",root:"/synthetic",items:[],nextOffset:null});throw Error(url);};
 await h.run("refreshStatus()"); await new Promise(setImmediate);
 assert.deepEqual(requests,["/api/status","/api/tree?path=&offset=0&limit=200","/api/dependencies"]);
 assert.match(text(h.get("dependency-packages")),/thing 1.2.3.*source: present.*index: partial.*thing_alias/s);
 assert.match(text(h.get("dependency-warnings")),/<b>not semantic<\/b>/);
 assert.equal(descendants(h.get("dependency-warnings")).some(n=>n.tagName==="b"),false);
 h.context.fetch=async url=>{if(url==="/api/status")return response({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},stats:{}});if(url.startsWith("/api/tree?"))return response({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},path:"",root:"/synthetic",items:[],nextOffset:null});throw Error("old daemon");};
 await h.run("refreshStatus()"); await new Promise(setImmediate);
 assert.match(h.get("dependency-state").textContent,/unavailable/); assert.equal(h.get("error").hidden,true);
});
test("package, paged declaration filter, and explicit source browsing preserve the primary workspace",async()=>{
 const h=harness(), requests=[];
 h.run("selectedMethod={id:'keep'}; $('sequence-diagram').textContent='diagram'; $('source').textContent='workspace source'");
 h.context.fetch=async url=>{requests.push(url);if(url==="/api/dependencies")return response(catalog());
 if(url.startsWith("/api/dependencies/source?"))return response(snapshot());
 const params=new URL(url,"http://local").searchParams;
 if(params.get("offset")==="100")return response(symbols([{...symbol,id:"second",name:"Second",qualifiedName:"thing::Second"}]));
 return response(symbols([symbol],100));};
 await h.get("dependency-refresh").listeners.click();
 await click(h.get("dependency-packages"),"thing 1.2.3");
 assert.equal(requests.length,2); assert.match(requests[1],/packageId=cargo%3Athing%401.*offset=0&limit=100/);
 assert.equal(h.get("dependency-next").disabled,false);
 await h.get("dependency-next").listeners.click(); assert.match(requests.at(-1),/offset=100/);
 assert.match(text(h.get("dependency-symbols")),/thing::Second/); assert.doesNotMatch(text(h.get("dependency-symbols")),/thing::Client/);
 await h.get("dependency-previous").listeners.click(); assert.match(requests.at(-1),/offset=0/);
 h.get("dependency-filter").value="Client & <b>";
 await h.get("dependency-search-form").listeners.submit({preventDefault(){}});
 assert.match(requests.at(-1),/q=Client%20%26%20%3Cb%3E/);
 assert.ok(requests.every(url=>!url.includes("/source?")));
 await click(h.get("dependency-symbols"),"thing::Client · struct");
 assert.equal(requests.filter(url=>url.includes("/source?")).length,1);
 assert.match(requests.at(-1),/catalogId=catalog-1&sourceRef=immutable-ref/);
 assert.match(h.get("external-source-path").textContent,/hash123.*immutable candidate.*terminal/);
 assert.match(text(h.get("external-source")),/<script>unsafe<\/script>/);
 assert.ok(descendants(h.get("external-source")).length<1900);
 const count=requests.length;h.get("external-next").listeners.click();assert.equal(requests.length,count);
 assert.match(text(h.get("external-source")),/Showing lines 601–1200/);
 assert.equal(h.run("selectedMethod.id"),"keep");
 assert.equal(h.get("sequence-diagram").textContent,"diagram");assert.equal(h.get("source").textContent,"workspace source");
 assert.ok(requests.every(url=>url.startsWith("/api/dependencies")));
});
for(const state of ["loading","failed","disabled"]) test(`${state} status remains honest with manual refresh and no background polling`,async()=>{
 const h=harness(); let count=0;
 h.context.fetch=async()=>{count++;return response({...catalog(),state,catalogId:null,packages:[],symbolCount:0});};
 await h.run("refreshDependencies()"); assert.match(h.get("dependency-state").textContent,new RegExp(state));
 assert.equal(h.timers.size,0);assert.equal(count,1);assert.equal(h.get("dependency-packages").children.length,0);
 await h.get("dependency-refresh").listeners.click();assert.equal(count,2);
});
test("empty catalog and missing package declarations show source and index limitations",async()=>{
 const h=harness();h.context.fetch=async()=>response({...catalog(),packages:[]});await h.run("refreshDependencies()");
 assert.match(text(h.get("dependency-packages")),/No supported library packages/);
 await ready(h);h.context.fetch=async()=>response(symbols([]));await h.run(`selectDependencyPackage(${JSON.stringify({...pkg,sourceState:"missing",indexState:"skipped"})})`);
 assert.match(h.get("dependency-symbol-state").textContent,/No indexed definitions.*missing.*skipped/);
});
for(const failure of ["success","http","network","json"]) test(`obsolete ${failure} source cannot overwrite newer library source or workspace`,async()=>{
 const h=harness();await select(h);const old=deferred();h.context.fetch=()=>old.promise;
 const pending=h.run(`loadDependencySource(${JSON.stringify(symbol)})`);
 h.context.fetch=async()=>response({...snapshot(),hash:"newhash"});await h.run(`loadDependencySource(${JSON.stringify(symbol)})`);
 if(failure==="network")old.reject(Error("old"));
 else if(failure==="http")old.resolve({ok:false,status:409,json:async()=>({error:{message:"old"}})});
 else if(failure==="json")old.resolve({ok:true,status:200,json:async()=>{throw Error("old");}});
 else old.resolve(response(snapshot()));
 await pending;assert.match(h.get("external-source-path").textContent,/newhash/);assert.equal(h.get("error").hidden,true);assert.equal(h.run("selectedMethod"),null);
});
for(const change of ["clearDependencyCatalog()","status={revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:2}}","status={revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},workspaceRoot:'new'}","$('logout').listeners.click()"])
 test(`late catalog and source ignored after ${change}`,async()=>{
 const h=harness();await select(h);const old=deferred();h.context.fetch=()=>old.promise;
 const pending=h.run(`loadDependencySource(${JSON.stringify(symbol)})`);h.run(change);old.resolve(response(snapshot()));await pending;
 assert.equal(h.run("externalSnapshot"),null);
 const h2=harness(),late=deferred();h2.context.fetch=()=>late.promise;const request=h2.run("refreshDependencies()");h2.run(change);late.resolve(response(catalog()));await request;
 assert.equal(h2.run("dependencyCatalog"),null);
});
test("changed catalog clears source and rejects obsolete symbol responses",async()=>{
 const h=harness();await select(h);h.context.fetch=async()=>response(snapshot());await h.run(`loadDependencySource(${JSON.stringify(symbol)})`);
 const old=deferred();h.context.fetch=()=>old.promise;const pending=h.run("loadDependencySymbols(100)");
 h.context.fetch=async()=>response(catalog("catalog-2"));await h.run("refreshDependencies()");old.resolve(response(symbols()));await pending;
 assert.equal(h.run("externalSnapshot"),null);assert.equal(h.get("external-source").children.length,0);assert.equal(h.get("dependency-symbols").children.length,0);
});
test("newer catalog status wins over obsolete failures",async()=>{
 const h=harness(),old=deferred();h.context.fetch=()=>old.promise;const pending=h.run("refreshDependencies()");
 await ready(h);old.reject(Error("old failure"));await pending;assert.equal(h.run("dependencyCatalog.catalogId"),"catalog-1");assert.doesNotMatch(h.get("dependency-state").textContent,/old failure/);
});
test("source conflicts remain local and require status refresh; no implicit request",async()=>{
 const h=harness();await select(h);let requests=0;h.context.fetch=async()=>{requests++;return {ok:false,status:409,json:async()=>({error:{message:"Source changed"}})};};
 await h.run(`loadDependencySource(${JSON.stringify(symbol)})`);assert.equal(requests,1);assert.equal(h.run("externalSnapshot"),null);
 assert.match(h.get("external-source-path").textContent,/Source changed.*Refresh library status/);assert.equal(h.get("error").hidden,true);assert.equal(h.run("selectedMethod"),null);
});
test("mismatched declaration/source provenance never paints",async()=>{
 const h=harness();await ready(h);h.context.fetch=async()=>response(symbols([symbol],null,"other"));await h.run(`selectDependencyPackage(${JSON.stringify(pkg)})`);
 assert.equal(h.get("dependency-symbols").children.length,0);assert.match(h.get("dependency-symbol-state").textContent,/provenance mismatch/);
 await select(h);h.context.fetch=async()=>response({...snapshot(),definitions:[]});await h.run(`loadDependencySource(${JSON.stringify(symbol)})`);
 assert.equal(h.run("externalSnapshot"),null);assert.match(h.get("external-source-path").textContent,/provenance mismatch/);
});
test("package changes and manual file clicks supersede pending catalog source",async()=>{
 const h=harness();await select(h);const old=deferred();h.context.fetch=()=>old.promise;const pending=h.run(`loadDependencySource(${JSON.stringify(symbol)})`);
 h.context.fetch=async()=>response(symbols());await h.run(`selectDependencyPackage(${JSON.stringify(pkg)})`);old.resolve(response(snapshot()));await pending;
 assert.equal(h.run("externalSnapshot"),null);
 const late=deferred();h.context.fetch=()=>late.promise;const p=h.run(`loadDependencySource(${JSON.stringify(symbol)})`);
 h.context.fetch=async()=>response({...snapshot(),rootId:"manual",path:"manual.rs",hash:"manual"});await h.run("loadExternalFile({id:'manual',label:'Manual'},'manual.rs')");
 late.resolve(response(snapshot()));await p;assert.equal(h.run("externalSnapshot.hash"),"manual");
});
test("UI keeps catalog visible and manual roots collapsed; flags do not claim semantic resolution",()=>{
 const html=fs.readFileSync("web/index.html","utf8");
 assert.match(html,/<section id="dependency-library"/);assert.doesNotMatch(html,/<details id="external-sources"[^>]*\bopen/);
 assert.match(html,/Syntax candidates/);assert.match(html,/Terminal library boundary/);assert.match(html,/do not prove resolved types/);
 assert.ok(html.indexOf('id="sequence-diagram"')<html.indexOf('id="dependency-library"'));
});

test("connect and completed workspace index automatically refresh metadata, never source",async()=>{
 const h=harness(),requests=[]; let revision={indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1};
 h.context.fetch=async(url,opts)=>{requests.push([url,opts.method]);
 if(url==="/api/checkouts")return response({checkouts:[{rootKey:ROOT,workspaceRoot:"/synthetic",state:"available",active:false}]});
 if(url==="/api/daemon/status")return response({activeCheckouts:0});
 if(url==="/api/status")return response({revision,stats:{}});
 if(url==="/api/dependencies")return response({...catalog(),workspaceRevision:revision});
 if(url.startsWith("/api/tree?"))return response({revision,path:"",root:"/workspace",items:[],nextOffset:null});
 if(url==="/api/jobs/current")return response(null);
 if(url==="/api/index")return response({id:"job",state:"running"});
 if(url==="/api/jobs/job"){revision={indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:2};return response({id:"job",state:"done"});}
 throw Error(url);};
 h.get("token").value="synthetic";h.get("connect-form").listeners.submit({preventDefault(){}});await new Promise(setImmediate);
 assert.equal(h.get("workspace").hidden,true,"authentication must not auto-select a checkout");
 h.get("checkout-select").value=ROOT;h.get("checkout-select").listeners.change({target:h.get("checkout-select")});await new Promise(setImmediate);
 assert.equal(h.get("workspace").hidden,false);assert.equal(requests.filter(([url])=>url==="/api/dependencies").length,1);
 await h.get("index").listeners.click();const poll=[...h.timers.values()].find(t=>t.delay===700);assert.ok(poll);await poll.callback();await new Promise(setImmediate);
 assert.equal(requests.filter(([url])=>url==="/api/dependencies").length,2);assert.equal(h.run("dependencyCatalog.workspaceRevision.indexRevision"),2);
 assert.ok(requests.every(([url])=>!url.includes("/source?")&&!url.startsWith("/api/questions")));
});
test("old package results cannot replace a new filter page",async()=>{
 const h=harness();await select(h);const old=deferred();h.context.fetch=()=>old.promise;const pending=h.run("loadDependencySymbols(100)");
 h.context.fetch=async()=>response(symbols([{...symbol,name:"Fresh",qualifiedName:"thing::Fresh",signature:"pub struct Fresh"}]));await h.run("loadDependencySymbols(0,'Fresh')");
 old.resolve(response(symbols()));await pending;assert.match(text(h.get("dependency-symbols")),/Fresh/);assert.doesNotMatch(text(h.get("dependency-symbols")),/Client/);
});

test("catalog readiness tells an existing method selection to refresh without replacing its diagram",async()=>{
 const h=harness(),requests=[];h.run("selectedMethod={id:'keep'}; $('sequence-diagram').textContent='keep diagram'");
 h.context.fetch=async url=>{requests.push(url);return response({...catalog(),state:'loading',catalogId:null});};await h.run("refreshDependencies()");
 h.context.fetch=async url=>{requests.push(url);return response(catalog());};await h.run("refreshDependencies()");
 assert.match(h.get("dependency-state").textContent,/Reselect the workspace method.*syntax-candidate lanes/);
 assert.equal(h.get("sequence-diagram").textContent,"keep diagram");assert.equal(h.run("selectedMethod.id"),"keep");
 assert.deepEqual(requests,["/api/dependencies","/api/dependencies"]);
});

test("dependency catalog with reused revision and old generation is rejected",async()=>{
  const h=harness();
  h.run(`status={revision:{indexGeneration:'87654321-4321-4321-8321-abcdef123456',indexRevision:1},workspaceRoot:'/workspace'}`);
  h.context.fetch=async url=>{if(url==='/api/dependencies')return response(catalog());throw Error(url);};
  await h.run('refreshDependencies()');
  assert.equal(h.run('dependencyCatalog'),null);
  assert.match(h.get('dependency-state').textContent,/another workspace revision/);
});

test("library status and definition pages reject reused numeric revisions, clear stale panels, and refresh status",async()=>{
 const next={indexGeneration:'87654321-4321-4321-8321-abcdef123456',indexRevision:1};
 for(const boundary of ['status','symbols']) {
   const h=harness(),requests=[];
   if(boundary==='symbols') await select(h);
   h.run(`status={revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},workspaceRoot:'/same'}`);
   h.context.fetch=async url=>{
     requests.push(url);
     if(url==='/api/status') return response({revision:next,workspaceRoot:'/same',stats:{}});
     if(url.startsWith('/api/tree?')) return response({path:'',root:'/same',indexedWorkspace:'/same',revision:next,items:[],nextOffset:null});
     if(url.startsWith('/api/dependencies/symbols?')) return response({...symbols(),workspaceRevision:next});
     if(url==='/api/dependencies') return response(requests.includes('/api/status') ? {...catalog(),workspaceRevision:next} : {...catalog(),workspaceRevision:next});
     throw Error(url);
   };
   if(boundary==='status') await h.run('refreshDependencies()');
   else await h.run('loadDependencySymbols(0)');
   await new Promise(setImmediate);
   assert.ok(requests.includes('/api/status'),boundary);
   assert.equal(h.run('status.revision.indexGeneration'),next.indexGeneration,boundary);
   assert.equal(h.run('dependencyPackage'),null,boundary);
   assert.equal(h.get('dependency-symbols').children.length,0,boundary);
 }
});


test("late external-source catalog response cannot paint after same-number new-generation refresh", async()=>{
 const h=harness(), pendingSource=deferred(), requests=[];
 await select(h);
 h.run("status.workspaceRoot='/same'");
 const old=h.run('status.revision');
 const next={indexGeneration:'87654321-4321-4321-8321-abcdef123456',indexRevision:1};
 h.context.fetch=async url=>{
   requests.push(url);
   if(url.startsWith('/api/dependencies/source?')) return pendingSource.promise;
   if(url==='/api/status') return response({workspaceRoot:'/same',revision:next,stats:{}});
   if(url.startsWith('/api/tree?')) return response({path:'',root:'/same',indexedWorkspace:'/same',revision:next,items:[],nextOffset:null});
   if(url==='/api/dependencies') return response({...catalog(),workspaceRevision:next});
   throw Error(url);
 };
 const loading=h.run(`loadDependencySource(${JSON.stringify(symbol)})`);
 assert.match(requests[0],/^\/api\/dependencies\/source\?catalogId=catalog-1&sourceRef=immutable-ref$/);
 assert.doesNotMatch(requests[0],/indexGeneration|indexRevision/);
 await h.run('refreshStatus()');
 pendingSource.resolve(response(snapshot()));await loading;
 assert.equal(h.run('status.revision.indexGeneration'),next.indexGeneration);
 assert.equal(h.run('externalSnapshot'),null);
 assert.equal(h.get('external-source').children.length,0);
});

test("queued indexing can stop local waiting without cancelling the durable request",async()=>{
 const h=harness(),requests=[];
 h.context.fetch=async(url,options)=>{requests.push([url,options.method]);
  if(url==="/api/jobs/current")return response(null);
  if(url==="/api/index")return response({id:"accepted",state:"queued",submittedAt:"1",startedAt:null,finishedAt:null});
  throw Error(`unexpected ${url}`);};
 await h.get("index").listeners.click();
 assert.equal(h.get("cancel").hidden,false);assert.equal(h.get("index").disabled,true);
 assert.match(h.get("job").textContent,/queued · accepted/);
 h.get("cancel").listeners.click();
 assert.equal(h.get("cancel").hidden,true);assert.equal(h.get("index").disabled,false);
 assert.match(h.get("job").textContent,/no longer waiting here/);
 assert.deepEqual(requests,[["/api/index","POST"]]);
 assert.equal([...h.timers.values()].filter(t=>t.delay===700).length,0);
});
