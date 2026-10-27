"use strict";
// Synthetic data only. Run: node --test tests/browse-ui.test.cjs
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const source = ["sequence.js", "files.js", "app.js"].map(file => fs.readFileSync(path.join(__dirname, "../web", file), "utf8")).join("\n");

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
    return {tagName, classList: {add(value) { classes.add(value); }, contains(value) { return classes.has(value); }, toggle(value, force) { const on = force ?? !classes.has(value); if (on) classes.add(value); else classes.delete(value); return on; }}, focus() {}, hidden: true, disabled: false, value: "", textContent: "", children: [], listeners: {},
      addEventListener(type, handler) { this.listeners[type] = handler; },
      replaceChildren(...items) { this.children = items; },
      append(...items) { this.children.push(...items); },
      querySelector() { return null; },
      attrs: {}, setAttribute(key, value) { this.attrs[key] = String(value); }, remove() {},
      click() { downloads.push(this.download); }};
  }
  function get(id) { if (!elements.has(id)) elements.set(id, node()); return elements.get(id); }
  const context = vm.createContext({console, TextEncoder, DOMException, Blob, AbortController, URLSearchParams,
    setTimeout(callback, delay) { const id = ++nextTimer; timers.set(id, {callback, delay}); return id; }, clearTimeout(id) { timers.delete(id); },
    window: {confirm: () => true},
    URL: {createObjectURL(blob) { blobs.push(blob); return "blob:synthetic"; }, revokeObjectURL() {}},
    document: {getElementById: get, createElement: node, createElementNS: (ns, tag) => node(tag), createDocumentFragment: node, createTextNode: text => ({textContent:text}), body: node()},
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
const root = {id:"rust",label:"rust",path:"/configured/rust/library"};
const snapshot = (path="std/src/fs.rs") => ({id:"snapshot",rootId:"rust",rootLabel:"rust",path,hash:"abc123",file:{text:"struct OpenOptions;\n<script>alert(1)</script>\nfn open() {}"},definitions:[{name:"OpenOptions",kind:"class",range:{startLine:1,endLine:1}},{name:"open",kind:"method",range:{startLine:3,endLine:3}}],warnings:["Definitional candidate only <b>not resolved</b>"]});
const click = (container, label) => descendants(container).find(n=>n.tagName==="button" && n.textContent===label).listeners.click();
test("collapsed panel loads nothing until explicit action; metadata browsing is lazy and independent", async()=>{
  const h=harness(), requests=[];
  assert.doesNotMatch(fs.readFileSync("web/index.html","utf8"), /<details id="external-sources"[^>]*\bopen/);
  h.context.fetch=async url=>{requests.push(url);
    if(url==="/api/rust-sources")return response({roots:[root]});
    if(url.includes("/file?"))return response(snapshot());
    const path=new URL(url,"http://local").searchParams.get("path");
    const items=path===""?[{name:"std",path:"std",kind:"directory"}]:path==="std"?[{name:"src",path:"std/src",kind:"directory"}]:[{name:"fs.rs",path:"std/src/fs.rs",kind:"file"}];
    return response({items,nextOffset:null,truncated:false});
  };
  h.get("external-sources").open=true; assert.equal(requests.length,0);
  h.run("selectedMethod={id:'keep'}; $('sequence-diagram').textContent='keep diagram'; $('source').textContent='workspace source'");
  await h.get("external-load").listeners.click(); assert.deepEqual(requests,["/api/rust-sources"]);
  assert.match(text(h.get("external-roots")),/\/configured\/rust\/library/);
  await click(h.get("external-roots"),"Browse rust");
  await click(h.get("external-roots"),"std/"); await click(h.get("external-roots"),"src/");
  assert.equal(requests.filter(p=>p.includes("/file?")).length,0);
  await click(h.get("external-roots"),"fs.rs"); assert.equal(requests.length,5);
  assert.match(h.get("external-source-path").textContent,/rust.*std\/src\/fs.rs.*abc123.*immutable/);
  assert.match(text(h.get("external-warnings")),/not resolved/);
  await click(h.get("external-definitions"),"open · method · line 3");
  assert.equal(requests.length,5);
  assert.equal(descendants(h.get("external-source")).filter(n=>n.classList?.contains("highlight")).length,1);
  assert.match(text(h.get("external-source")),/<script>alert\(1\)<\/script>/);
  assert.equal(descendants(h.get("external-source")).some(n=>n.tagName==="script"),false);
  h.get("external-filter").value="OpenOptions"; h.get("external-filter").listeners.input();
  assert.equal(descendants(h.get("external-definitions")).filter(n=>n.tagName==="button").length,1);
   assert.equal(h.run("selectedMethod.id"),"keep");
  assert.equal(h.get("sequence-diagram").textContent,"keep diagram"); assert.equal(h.get("source").textContent,"workspace source");
  assert.ok(requests.every(p=>p.startsWith("/api/rust-sources")));
});
test("empty configuration is honest and creates no browsing controls",async()=>{
 const h=harness(); h.context.fetch=async()=>response({roots:[]}); await h.run("loadExternalRoots()");
 assert.match(h.get("external-state").textContent,/No external Rust source roots configured/);
 assert.equal(h.get("external-roots").children.length,0); assert.equal(h.get("external-filter").disabled,true);
});
for(const failure of ["success","http","network","json"]) test(`obsolete file ${failure} cannot replace newer candidate or invalidate workspace`,async()=>{
 const h=harness(), old=deferred(); h.context.fetch=()=>old.promise;
 const pending=h.run(`loadExternalFile(${JSON.stringify(root)},'old.rs')`);
 h.context.fetch=async()=>response(snapshot("new.rs")); await h.run(`loadExternalFile(${JSON.stringify(root)},'new.rs')`);
 if(failure==="network")old.reject(new Error("obsolete"));
 else if(failure==="http")old.resolve({ok:false,status:409,json:async()=>({error:{message:"obsolete"}})});
 else if(failure==="json")old.resolve({ok:true,status:200,json:async()=>{throw new Error("obsolete");}});
 else old.resolve(response(snapshot("old.rs")));
 await pending; assert.match(h.get("external-source-path").textContent,/new.rs/); assert.equal(h.run("selectedMethod"),null);
});
for(const request of ["loadExternalRoots()",`loadExternalFile(${JSON.stringify(root)},'std/src/fs.rs')`]) test(`disconnect clears external data and ignores late ${request}`,async()=>{
 const h=harness(), old=deferred(); h.context.fetch=()=>old.promise;
 const pending=h.run(request); h.get("logout").listeners.click(); old.resolve(response(request.includes("Roots")?{roots:[root]}:snapshot()));
 await pending; assert.equal(h.get("external-roots").children.length,0); assert.equal(h.get("external-source").children.length,0);
 assert.equal(h.run("externalSnapshot"),null); assert.equal(h.get("external-sources").open,false);
});
test("directory paging, truncation and stale directory failures",async()=>{
 const h=harness(); h.context.fetch=async()=>response({roots:[root]}); await h.run("loadExternalRoots()");
 h.context.fetch=async()=>response({items:[{name:"first.rs",path:"first.rs",kind:"file"}],nextOffset:1,truncated:true});
 await click(h.get("external-roots"),"Browse rust"); assert.match(text(h.get("external-roots")),/truncated/);
 const requests=[]; h.context.fetch=async url=>{requests.push(url);return response({items:[{name:"second.rs",path:"second.rs",kind:"file"}],nextOffset:null});};
 await click(h.get("external-roots"),"Load more entries"); assert.match(requests[0],/offset=1/); assert.match(text(h.get("external-roots")),/first.rs.*second.rs/);
 h.context.fetch=async()=>response({roots:[root]}); await h.run("loadExternalRoots()");
 const old=deferred(); h.context.fetch=()=>old.promise; const pending=click(h.get("external-roots"),"Browse rust");
 h.run("clearExternalSources()"); old.reject(new Error("obsolete directory")); await pending;
 assert.equal(h.get("external-roots").children.length,0); assert.doesNotMatch(h.get("external-state").textContent,/obsolete/);
});

test("parent-qualified definitions distinguish File::open from OpenOptions::open and filter by container",async()=>{
 const h=harness(), data=snapshot(); data.definitions=[
 {id:"file",name:"File",kind:"class",range:{startLine:1,endLine:1}},
 {id:"options",name:"OpenOptions",kind:"class",range:{startLine:1,endLine:1}},
 {id:"file-open",parent:"file",name:"open",kind:"method",range:{startLine:2,endLine:2}},
 {id:"options-open",parent:"options",name:"open",kind:"method",range:{startLine:3,endLine:3}}];
 h.context.fetch=async()=>response(data); await h.run(`loadExternalFile(${JSON.stringify(root)},'std/src/fs.rs')`);
 assert.match(text(h.get("external-definitions")),/File::open/); assert.match(text(h.get("external-definitions")),/OpenOptions::open/);
 h.get("external-filter").value="OpenOptions"; h.get("external-filter").listeners.input();
 assert.doesNotMatch(text(h.get("external-definitions")),/File::open/); assert.match(text(h.get("external-definitions")),/OpenOptions::open/);
 await click(h.get("external-definitions"),"OpenOptions::open · method · line 3");
 assert.equal(descendants(h.get("external-source")).filter(n=>n.classList?.contains("highlight")).length,1);
});

test("large candidate text bounds DOM and definition selection keeps global line numbers",async()=>{
 const h=harness(), data=snapshot(); data.file.text="\n".repeat(2*1024*1024-1)+"fn tail() {}";
 data.definitions=[{id:"tail",name:"tail",kind:"function",range:{startLine:2*1024*1024,endLine:2*1024*1024}}];
 h.context.fetch=async()=>response(data); await h.run(`loadExternalFile(${JSON.stringify(root)},'std/src/fs.rs')`);
 assert.ok(descendants(h.get("external-source")).length<2000);
 await click(h.get("external-definitions"),`tail · function · line ${2*1024*1024}`);
 assert.ok(descendants(h.get("external-source")).length<2000);
 assert.match(text(h.get("external-source")),/2097152.*fn tail/);
 assert.equal(descendants(h.get("external-source")).filter(n=>n.classList?.contains("highlight")).length,1);
 assert.equal(h.run("externalSnapshot.file.text.length"),data.file.text.length);
});
test("collapse during directory pagination can reopen and reload instead of leaving loading forever",async()=>{
 const h=harness(); h.context.fetch=async()=>response({roots:[root]}); await h.run("loadExternalRoots()");
 h.context.fetch=async()=>response({items:[{name:"first.rs",path:"first.rs",kind:"file"}],nextOffset:1});
 await click(h.get("external-roots"),"Browse rust"); const old=deferred(); h.context.fetch=()=>old.promise;
 const pending=click(h.get("external-roots"),"Load more entries"); await click(h.get("external-roots"),"Browse rust");
 h.context.fetch=async()=>response({items:[{name:"fresh.rs",path:"fresh.rs",kind:"file"}],nextOffset:null});
 await click(h.get("external-roots"),"Browse rust"); old.resolve(response({items:[],nextOffset:null})); await pending;
 assert.match(text(h.get("external-roots")),/fresh.rs/); assert.doesNotMatch(text(h.get("external-roots")),/Loading directory/);
});

test("candidate source paging uses cached text, preserves selected range, and resets on new file or disconnect",async()=>{
 const h=harness(), data=snapshot(); let requests=0;
 data.file.text=Array.from({length:1300},(_,i)=>`line ${i+1}`).join("\n");
 data.definitions=[{id:"long",name:"long",kind:"function",range:{startLine:1,endLine:900}}];
 h.context.fetch=async()=>{requests++;return response(data);};
 await h.run(`loadExternalFile(${JSON.stringify(root)},'std/src/fs.rs')`);
 await click(h.get("external-definitions"),"long · function · line 1");
 const provenance=h.get("external-source-path").textContent;
 assert.equal(h.get("external-previous").disabled,true); assert.equal(h.get("external-next").disabled,false);
 h.get("external-next").listeners.click(); assert.match(text(h.get("external-source")),/Showing lines 601–1200 of 1300/);
 assert.equal(descendants(h.get("external-source")).filter(n=>n.classList?.contains("highlight")).length,300);
 h.get("external-next").listeners.click(); assert.match(text(h.get("external-source")),/Showing lines 1201–1300 of 1300/);
 assert.equal(h.get("external-next").disabled,true); assert.equal(descendants(h.get("external-source")).filter(n=>n.classList?.contains("highlight")).length,0);
 h.get("external-previous").listeners.click(); assert.match(text(h.get("external-source")),/Showing lines 601–1200/);
 assert.equal(h.get("external-source-path").textContent,provenance); assert.equal(requests,1);
 await h.run(`loadExternalFile(${JSON.stringify(root)},'std/src/fs.rs')`);
 assert.equal(h.run("externalWindowStart"),0); assert.equal(h.run("externalSelectedRange"),null);
 h.get("logout").listeners.click(); assert.equal(h.get("external-next").disabled,true); assert.equal(h.get("external-previous").disabled,true);
});
