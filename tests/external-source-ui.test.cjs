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
// Candidate source is reached in context through the catalog (#450); the manual-root
// browser was Libraries-tab-only UI and is gone.
const PACKAGE = {id:"pkg",name:"std",version:"1.0.0",ecosystem:"cargo",aliases:[],source:"rust-src",sourceState:"present",indexState:"complete",warnings:[]};
function prime(h) {
  h.run(`dependencyCatalog = ${JSON.stringify({catalogId:"catalog",state:"ready",packages:[PACKAGE],warnings:[]})}; dependencyPackage = dependencyCatalog.packages[0];`);
}
const snapshot = (path="std/src/fs.rs") => ({id:`ref:${path}`,rootId:"pkg",rootLabel:"std",path,hash:"abc123",file:{text:"struct OpenOptions;\n<script>alert(1)</script>\nfn open() {}"},
  definitions:[{id:"options",name:"OpenOptions",kind:"class",range:{startLine:1,endLine:1}},{id:"open",name:"open",kind:"method",range:{startLine:3,endLine:3}}],warnings:["Definitional candidate only <b>not resolved</b>"]});
const symbolOf = (data, id = data.definitions[0].id, range) => ({id, packageId:"pkg", name:id, sourceRef:data.id, path:data.path, ...(range ? {range} : {})});
const load = (h, data, id, range) => h.run(`loadDependencySource(${JSON.stringify(symbolOf(data, id, range))})`);
const click = (container, label) => descendants(container).find(n=>n.tagName==="button" && n.textContent===label).listeners.click();

test("catalog candidate source is text, separate from workspace evidence, and filters its definitions", async()=>{
  const h=harness(), requests=[], data=snapshot(); prime(h);
  h.context.fetch=async url=>{requests.push(url); return response(data);};
  h.run("selectedMethod={id:'keep'}; $('sequence-diagram').textContent='keep diagram'; $('source').textContent='workspace source'");
  await load(h, data, "open", {startLine:3,endLine:3});
  assert.equal(requests.length,1); assert.match(requests[0],/\/api\/dependencies\/source\?catalogId=catalog&sourceRef=/);
  assert.match(h.get("external-source-path").textContent,/std.*std\/src\/fs.rs.*abc123.*immutable/);
  assert.match(text(h.get("external-warnings")),/not resolved/);
  assert.equal(descendants(h.get("external-source")).filter(n=>n.classList?.contains("highlight")).length,1);
  assert.match(text(h.get("external-source")),/<script>alert\(1\)<\/script>/);
  assert.equal(descendants(h.get("external-source")).some(n=>n.tagName==="script"),false);
  assert.doesNotMatch(text(h.get("external-source")), /Workspace diagrams are unavailable/);
  assert.match(h.get("external-source-hint").textContent, /Workspace diagrams are unavailable/);
  assert.equal(h.get("external-source-hint").hidden, false);
  assert.equal(descendants(h.get("external-source")).filter(node => node.className === "source-line").length, data.file.text.split("\n").length, "only source lines are copied");
  h.get("external-filter").value="OpenOptions"; h.get("external-filter").listeners.input();
  assert.equal(descendants(h.get("external-definitions")).filter(n=>n.tagName==="button").length,1);
  assert.equal(h.run("selectedMethod.id"),"keep");
  assert.equal(h.get("sequence-diagram").textContent,"keep diagram"); assert.equal(h.get("source").textContent,"workspace source");
  assert.equal(requests.length,1, "definition filtering reuses the cached snapshot");
});
for(const failure of ["success","http","network","json"]) test(`obsolete candidate ${failure} cannot replace newer candidate or invalidate workspace`,async()=>{
 const h=harness(), old=deferred(); prime(h); h.context.fetch=()=>old.promise;
 const pending=load(h, snapshot("old.rs"));
 h.context.fetch=async()=>response(snapshot("new.rs")); await load(h, snapshot("new.rs"));
 if(failure==="network")old.reject(new Error("obsolete"));
 else if(failure==="http")old.resolve({ok:false,status:409,json:async()=>({error:{message:"obsolete"}})});
 else if(failure==="json")old.resolve({ok:true,status:200,json:async()=>{throw new Error("obsolete");}});
 else old.resolve(response(snapshot("old.rs")));
 await pending; assert.match(h.get("external-source-path").textContent,/new.rs/); assert.equal(h.run("selectedMethod"),null);
});
test("disconnect clears candidate source and ignores a late catalog source response",async()=>{
 const h=harness(), old=deferred(); prime(h); h.context.fetch=()=>old.promise;
 const pending=load(h, snapshot()); h.get("logout").listeners.click(); old.resolve(response(snapshot()));
 await pending; assert.equal(h.get("external-source").children.length,0);
 assert.equal(h.run("externalSnapshot"),null);
});
test("parent-qualified definitions distinguish File::open from OpenOptions::open and filter by container",async()=>{
 const h=harness(), data=snapshot(); prime(h); data.definitions=[
 {id:"file",name:"File",kind:"class",range:{startLine:1,endLine:1}},
 {id:"options",name:"OpenOptions",kind:"class",range:{startLine:1,endLine:1}},
 {id:"file-open",parent:"file",name:"open",kind:"method",range:{startLine:2,endLine:2}},
 {id:"options-open",parent:"options",name:"open",kind:"method",range:{startLine:3,endLine:3}}];
 h.context.fetch=async()=>response(data); await load(h, data, "file");
 assert.match(text(h.get("external-definitions")),/File::open/); assert.match(text(h.get("external-definitions")),/OpenOptions::open/);
 h.get("external-filter").value="OpenOptions"; h.get("external-filter").listeners.input();
 assert.doesNotMatch(text(h.get("external-definitions")),/File::open/); assert.match(text(h.get("external-definitions")),/OpenOptions::open/);
 await click(h.get("external-definitions"),"OpenOptions::open · method · line 3");
 assert.equal(descendants(h.get("external-source")).filter(n=>n.classList?.contains("highlight")).length,1);
});
test("large candidate text bounds DOM and definition selection keeps global line numbers",async()=>{
 const h=harness(), data=snapshot(); prime(h); data.file.text="\n".repeat(2*1024*1024-1)+"fn tail() {}";
 data.definitions=[{id:"tail",name:"tail",kind:"function",range:{startLine:2*1024*1024,endLine:2*1024*1024}}];
 h.context.fetch=async()=>response(data); await load(h, data, "tail");
 assert.ok(descendants(h.get("external-source")).length<2000);
 await click(h.get("external-definitions"),`tail · function · line ${2*1024*1024}`);
 assert.ok(descendants(h.get("external-source")).length<2000);
 assert.match(text(h.get("external-source")),/2097152.*fn tail/);
 assert.equal(descendants(h.get("external-source")).filter(n=>n.classList?.contains("highlight")).length,1);
 assert.equal(h.run("externalSnapshot.file.text.length"),data.file.text.length);
});
test("candidate source paging uses cached text, preserves selected range, and resets on new source or disconnect",async()=>{
 const h=harness(), data=snapshot(); let requests=0; prime(h);
 data.file.text=Array.from({length:1300},(_,i)=>`line ${i+1}`).join("\n");
 data.definitions=[{id:"long",name:"long",kind:"function",range:{startLine:1,endLine:900}}];
 h.context.fetch=async()=>{requests++;return response(data);};
 await load(h, data, "long");
 await click(h.get("external-definitions"),"long · function · line 1");
 const provenance=h.get("external-source-path").textContent;
 assert.equal(h.get("external-previous").disabled,true); assert.equal(h.get("external-next").disabled,false);
 h.get("external-next").listeners.click(); assert.match(text(h.get("external-source")),/Showing lines 601–1200 of 1300/);
 assert.equal(descendants(h.get("external-source")).filter(n=>n.classList?.contains("highlight")).length,300);
 h.get("external-next").listeners.click(); assert.match(text(h.get("external-source")),/Showing lines 1201–1300 of 1300/);
 assert.equal(h.get("external-next").disabled,true); assert.equal(descendants(h.get("external-source")).filter(n=>n.classList?.contains("highlight")).length,0);
 h.get("external-previous").listeners.click(); assert.match(text(h.get("external-source")),/Showing lines 601–1200/);
 assert.equal(h.get("external-source-path").textContent,provenance); assert.equal(requests,1);
 await load(h, data, "long");
 assert.equal(h.run("externalWindowStart"),0); assert.equal(h.run("externalSelectedRange"),null);
 h.get("logout").listeners.click(); assert.equal(h.get("external-next").disabled,true); assert.equal(h.get("external-previous").disabled,true);
});
