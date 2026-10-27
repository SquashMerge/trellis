"use strict";
// Synthetic tokens and in-memory files only. Run: node --test tests/token-ui.test.cjs
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const source = ["sequence.js", "shell.js", "app.js"].map(file => fs.readFileSync(path.join(__dirname, "../web", file), "utf8")).join("\n");
const KEY = "trellis.daemonToken.v1";
const ROOT = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SCOPED = `/api/checkouts/${ROOT}`;
const response = data => ({ok:true, status:200, json:async () => data});
const flush = () => new Promise(resolve => setImmediate(resolve));
function harness({saved, blocked = false, routes} = {}) {
  const elements = new Map(), requests = [], writes = [], stored = new Map(saved === undefined ? [] : [[KEY, saved]]);
  function node(tagName) {
    const classes = new Set();
    return {tagName, hidden:true, disabled:false, checked:false, value:"", textContent:"", children:[], listeners:{}, attrs:{},
      classList:{add(value) { classes.add(value); }, remove(value) { classes.delete(value); }, contains(value) { return classes.has(value); }, toggle(value, force) { const on = force ?? !classes.has(value); if (on) classes.add(value); else classes.delete(value); return on; }},
      focus() {}, addEventListener(type, handler) { this.listeners[type] = handler; },
      replaceChildren(...items) { this.children = items; }, append(...items) { this.children.push(...items); },
      querySelector() { return null; }, setAttribute(key, value) { this.attrs[key] = String(value); }, remove() {}, click() {}};
  }
  function get(id) { if (!elements.has(id)) elements.set(id, node()); return elements.get(id); }
  get("connect-form").hidden = false;
  const storage = {
    getItem(key) { if (blocked) throw new Error("Storage blocked"); return stored.get(key) ?? null; },
    setItem(key, value) { if (blocked) throw new Error("Storage blocked"); writes.push([key,value]); stored.set(key,value); },
    removeItem(key) { if (blocked) throw new Error("Storage blocked"); stored.delete(key); }
  };
  const context = vm.createContext({console, TextEncoder, DOMException, Blob, AbortController, URLSearchParams,
    setTimeout() { return 1; }, clearTimeout() {}, localStorage:storage, window:{localStorage:storage, location:{host:"127.0.0.1:43127"}, confirm:() => true},
    document:{addEventListener() {}, getElementById:get, createElement:node, createElementNS:(_,tag) => node(tag), createDocumentFragment:node, createTextNode:text => ({textContent:text}), body:node()},
    fetch:async (url, options) => {
      requests.push({url,options});
      if (routes) return routes(url, options);
      if (url === "/api/checkouts") return response({checkouts:[{rootKey:ROOT,workspaceRoot:"/synthetic",state:"available",active:false}]});
      if (url === "/api/daemon/status") return response({activeCheckouts:0});
      assert.ok(url.startsWith(`${SCOPED}/`), `Unexpected unscoped route: ${url}`);
      url = "/api" + url.slice(SCOPED.length);
      if (url === "/api/status") return response({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},workspaceRoot:"/synthetic",stats:{}});
      if (url.startsWith("/api/tree?")) return response({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},path:"",root:"/synthetic",items:[],nextOffset:null});
      if (url === "/api/jobs/current") return response(null);
      if (url === "/api/dependencies") return response({state:"disabled",workspaceRevision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},catalogId:null,packages:[],warnings:[]});
      throw new Error(`Unexpected request: ${url}`);
    }});
  vm.runInContext(source, context);
  async function connect(value = "synthetic-new", remember = false, select = true) {
    get("token").value = value; get("remember-token").checked = remember;
    get("connect-form").listeners.submit({preventDefault() {}});
    await flush();
    if (select && !get("checkout-select").disabled) {
      get("checkout-select").value = ROOT;
      await get("checkout-select").listeners.change({target:get("checkout-select")});
      await flush();
    }
  }
  async function file(text, size = Buffer.byteLength(text)) {
    get("token-file").value = "synthetic-selection";
    get("token-file").files = [{size, text:async () => text}];
    await get("token-file").listeners.change();
  }
  return {get, context, run:code=>vm.runInContext(code,context), requests, writes, stored, connect, file};
}

test("checkout selection and refresh load only retained services and keep current jobs",async()=>{
  const h=harness();
  await h.connect("synthetic",false);
  assert.equal(h.get("checkout-state").textContent,"Checkout ready.");
  assert.deepEqual(h.requests.map(({url})=>url),[
    "/api/checkouts","/api/daemon/status",`${SCOPED}/status`,
    `${SCOPED}/tree?path=&offset=0&limit=200`,`${SCOPED}/dependencies`,`${SCOPED}/jobs/current`
  ]);
  h.get("refresh").listeners.click();await flush();
  assert.equal(h.get("error").hidden,true);
  assert.ok(h.requests.every(({url})=>! /\/(?:symbols|symbol|query|views|annotations|questions|jev|acp)(?:[/?]|$)/.test(url)));
  h.get("logout").listeners.click();
  await h.connect("synthetic",false);
  assert.equal(h.get("checkout-state").textContent,"Checkout ready.");
});
test("retired checkout routes are rejected before a browser request",async()=>{
  const h=harness();await h.connect("synthetic",false);
  const before=h.requests.length;
  for(const route of ["symbol?id=x","query","views","views/item","annotations","annotations/item",
    "questions/preview","questions/packet/jev-request","questions/packet/jev-response","questions/packet/selection",
    "questions/packet/jev-run","questions/packet/acp-answer","jev/status","acp/status"]) {
    h.context.retiredPath=`/api/${route}`;
    await assert.rejects(h.run("api(retiredPath)"),/Unknown checkout API route/);
  }
  assert.equal(h.requests.length,before);
});

test("Files reads use the selected checkout adapter without reviving retired symbol detail",async()=>{
  const h=harness();await h.connect("synthetic",false);
  const routes=[
    ["symbols?path=a.js&indexGeneration=12345678-1234-4123-8123-123456789abc&indexRevision=1","GET",undefined],
    ["syntax-matches","POST",{expectedRevision:{indexGeneration:"12345678-1234-4123-8123-123456789abc",indexRevision:1},symbolId:"measured"}],
    ["git/diff?path=a.js","GET",undefined]
  ];
  h.context.fetch=async(url,options)=>{h.requests.push({url,options});return response({});};
  for(const [suffix,method,body] of routes){
    h.context.filesPath=`/api/${suffix}`;h.context.filesMethod=method;h.context.filesBody=body;
    await h.run("api(filesPath,filesMethod,filesBody)");
    const request=h.requests.at(-1);
    assert.equal(request.url,`${SCOPED}/${suffix}`);assert.equal(request.options.method,method);
    if(body)assert.deepEqual(JSON.parse(request.options.body),body);
  }
});

test("Connect enters the shell directly and reports the measured daemon address", async () => {
  const h = harness();
  await h.connect("synthetic",false,false);
  assert.equal(h.run('document.body.classList.contains("daemon-connected")'),true);
  assert.equal(h.get("connect-form").hidden,true);
  assert.equal(h.get("logout").hidden,false);
  assert.equal(h.get("token").value,"");
  assert.match(h.get("notice").textContent,/Connected to the daemon on 127\.0\.0\.1:43127/);
  assert.equal(h.run("selectedRootKey"),null);
  assert.deepEqual(h.requests.map(r=>r.url),["/api/checkouts","/api/daemon/status"]);
  h.get("logout").listeners.click();
  assert.equal(h.run('document.body.classList.contains("daemon-connected")'),false);
  assert.equal(h.get("connect-form").hidden,false);
});
test("authentication rejection shows its type as text without echoing the credential", async () => {
  const h = harness({saved:"synthetic-old",routes:async()=>({ok:false,status:401,json:async()=>({error:{code:"unauthorized",message:"Synthetic rejection"}})})});
  await h.connect("synthetic-rejected",true,false);
  assert.match(h.get("error").textContent,/Authentication failed \(unauthorized\)/);
  assert.doesNotMatch(h.get("error").textContent,/synthetic-rejected/);
  assert.equal(h.run('document.body.classList.contains("daemon-connected")'),false);
  assert.equal(h.get("token").value,"");
  assert.equal(h.stored.has(KEY),false);
});

test("saved token is loaded with opt-in checked but makes no startup request", () => {
  const h = harness({saved:"synthetic-saved"});
  assert.equal(h.get("token").value, "synthetic-saved");
  assert.equal(h.get("remember-token").checked, true);
  assert.equal(h.get("connect-form").hidden, false);
  assert.equal(h.requests.length, 0);
  assert.equal(h.writes.length, 0);
});
test("fresh browser starts with empty token and persistence off", () => {
  const h = harness();
  assert.equal(h.get("token").value, "");
  assert.equal(h.get("remember-token").checked, false);
  assert.equal(h.requests.length, 0);
});
test("successful explicit connect persists trimmed token only with opt-in", async () => {
  const h = harness(); await h.connect("  synthetic-new\n", true);
  assert.equal(h.get("workspace").hidden, false);
  assert.deepEqual(h.writes, [[KEY,"synthetic-new"]]);
  assert.equal(h.requests[0].options.headers.Authorization, "Bearer synthetic-new");
});
test("successful connect without opt-in removes existing saved token", async () => {
  const h = harness({saved:"synthetic-old"}); await h.connect("synthetic-new", false);
  assert.equal(h.get("workspace").hidden, false);
  assert.equal(h.stored.has(KEY), false); assert.equal(h.writes.length, 0);
});
test("unchecking persistence immediately forgets the saved token", () => {
  const h = harness({saved:"synthetic-old"});
  h.get("remember-token").checked = false; h.get("remember-token").listeners.change();
  assert.equal(h.stored.has(KEY), false); assert.equal(h.requests.length, 0);
});
for (const status of [401,403,500]) test(`failed ${status} connect ${status === 500 ? "retains" : "removes"} saved token`, async () => {
  const h = harness({saved:"synthetic-old"});
  h.context.fetch = async () => ({ok:false,status,json:async () => ({error:{message:"Synthetic rejection"}})});
  await h.connect("synthetic-new", true);
  assert.equal(h.stored.get(KEY), status === 500 ? "synthetic-old" : undefined);
  assert.equal(h.writes.length, 0); assert.equal(h.get("workspace").hidden, true);
  if (status === 500) assert.match(h.get("checkout-state").textContent, /unavailable/i);
  else assert.equal(h.get("error").hidden, false);
});
test("network failure retains existing saved token and never saves the rejected replacement", async () => {
  const h = harness({saved:"synthetic-old"}); h.context.fetch = async () => { throw new Error("Synthetic offline"); };
  await h.connect("synthetic-new", true);
  assert.equal(h.stored.get(KEY), "synthetic-old"); assert.equal(h.writes.length, 0);
});
test("Disconnect removes saved token and resets token, checkbox and file selection", async () => {
  const h = harness(); await h.connect("synthetic-new", true);
  h.get("token").value = "synthetic-pending"; h.get("token-file").value = "synthetic-selection";
  h.get("logout").listeners.click();
  assert.equal(h.stored.has(KEY), false); assert.equal(h.get("remember-token").checked, false);
  assert.equal(h.get("token").value, ""); assert.equal(h.get("token-file").value, "");
  assert.equal(h.get("workspace").hidden, true); assert.equal(h.get("connect-form").hidden, false);
});
test("blocked storage does not prevent startup or normal connect and reports save failure", async () => {
  const h = harness({blocked:true}); assert.equal(h.requests.length, 0);
  await h.connect("synthetic-new", true);
  assert.equal(h.get("workspace").hidden, false);
  assert.match(h.get("notice").textContent, /storage.*unavailable|not saved/i);
  await h.run(`selectCheckout('${ROOT}')`);
  assert.match(h.get("notice").textContent, /storage.*unavailable|not saved/i,"checkout guidance must retain a failed token-save warning");
  assert.doesNotThrow(() => h.get("logout").listeners.click());
  await h.connect("synthetic-memory-only",false);
  assert.equal(h.get("workspace").hidden,false);
  assert.doesNotMatch(h.get("notice").textContent,/not saved/i,"a new in-memory session must not inherit the prior save failure");
});
test("local token file trims and fills input only, with no request or persistence", async () => {
  const h = harness(); await h.file(" \nsynthetic-file\r\n");
  assert.equal(h.get("token").value, "synthetic-file"); assert.equal(h.get("token-file").value, "");
  assert.equal(h.get("remember-token").checked, false);
  assert.equal(h.requests.length, 0); assert.equal(h.writes.length, 0);
});
test("file accepts 512 characters and exactly 1024 bytes before trimming", async () => {
  const h = harness(); await h.file(" ".repeat(512) + "s".repeat(512));
  assert.equal(h.get("token").value, "s".repeat(512)); assert.equal(h.get("token-file").value, "");
});
for (const [label,text,size] of [["blank"," \n",2],["overlong","s".repeat(513),513],["oversize","synthetic-file",1025]])
  test(`invalid ${label} file leaves token unchanged and clears file selection`, async () => {
    const h = harness(); h.get("token").value = "synthetic-existing";
    await h.file(text,size);
    assert.equal(h.get("token").value, "synthetic-existing"); assert.equal(h.get("token-file").value, "");
    assert.equal(h.get("error").hidden, false); assert.equal(h.requests.length, 0); assert.equal(h.writes.length, 0);
  });
test("oversize file is rejected before its contents are read", async () => {
  const h = harness(); let reads = 0;
  h.get("token-file").files = [{size:1025,text:async () => { reads++; return "synthetic"; }}];
  await h.get("token-file").listeners.change(); assert.equal(reads, 0);
});
test("file read error is caught and file selection resets", async () => {
  const h = harness(); h.get("token").value = "synthetic-existing"; h.get("token-file").value = "synthetic-selection";
  h.get("token-file").files = [{size:10,text:async () => { throw new Error("Synthetic read failure"); }}];
  await h.get("token-file").listeners.change();
  assert.equal(h.get("token").value, "synthetic-existing"); assert.equal(h.get("token-file").value, "");
  assert.equal(h.get("error").hidden, false); assert.equal(h.requests.length, 0);
});
test("cancelled file selection is a no-op", async () => {
  const h = harness(); h.get("token").value = "synthetic-existing"; h.get("token-file").files = [];
  await h.get("token-file").listeners.change(); assert.equal(h.get("token").value, "synthetic-existing");
  assert.equal(h.requests.length, 0);
});

for (const text of ["synthetic\nsecret", "<script>synthetic</script>"]) test("file rejects unsafe token text without echoing it", async () => {
  const h = harness(); h.get("token").value = "synthetic-existing";
  await h.file(text);
  assert.equal(h.get("token").value, "synthetic-existing");
  assert.equal(h.get("error").hidden, false);
  assert.ok(!h.get("error").textContent.includes(text));
});
test("file read exceptions do not expose raw error details", async () => {
  const h = harness();
  h.get("token-file").files = [{size:10,text:async () => { throw new Error("synthetic-sensitive-detail"); }}];
  await h.get("token-file").listeners.change();
  assert.equal(h.get("error").hidden, false);
  assert.doesNotMatch(h.get("error").textContent, /synthetic-sensitive-detail/);
});
test("older file read cannot overwrite newer selection", async () => {
  const h = harness(); let finish;
  h.get("token-file").files = [{size:10,text:() => new Promise(resolve => { finish = resolve; })}];
  const pending = h.get("token-file").listeners.change();
  await h.file("synthetic-newer"); finish("synthetic-older"); await pending;
  assert.equal(h.get("token").value, "synthetic-newer"); assert.equal(h.requests.length, 0);
});
test("Connect clears a pending native file selection and ignores its late contents", async () => {
  const h = harness(); let finish;
  h.get("token-file").value = "synthetic-selection";
  h.get("token-file").files = [{size:10,text:() => new Promise(resolve => { finish = resolve; })}];
  const pending = h.get("token-file").listeners.change();
  await h.connect("synthetic-pasted",false,false);
  assert.equal(h.get("token-file").value,"");
  assert.equal(h.get("token").value,"");
  finish("synthetic-file"); await pending;
  assert.equal(h.get("token").value,"");
  assert.equal(h.run("token"),"synthetic-pasted");
  assert.equal(h.get("connect-form").hidden,true);
});

test("pending file read cannot restore credentials after Disconnect", async () => {
  const h = harness(); let finish;
  h.get("token-file").files = [{size:10,text:() => new Promise(resolve => { finish = resolve; })}];
  const pending = h.get("token-file").listeners.change();
  h.get("logout").listeners.click(); finish("synthetic-old"); await pending;
  assert.equal(h.get("token").value, ""); assert.equal(h.stored.has(KEY), false);
});

// The same revision and relative path cannot prove checkout identity. Only the
// exact selected root URL and generation fence can do that.
test("explicit A to B switch keeps exact B URLs and rejects late A 401 with equal revision and path", async () => {
  const A = ROOT, B = "b".repeat(64), pair = {indexGeneration:"12345678-1234-4123-8123-123456789abc",indexRevision:1};
  const rows = [A,B].map((rootKey,i) => ({rootKey,workspaceRoot:i ? "/checkout/B" : "/checkout/A",state:"available",active:false}));
  let holdA = false, releaseA;
  const h = harness({routes:async (url, options) => {
    if (url === "/api/checkouts") return response({checkouts:rows});
    if (url === "/api/daemon/status") return response({activeCheckouts:0});
    const match = /^\/api\/checkouts\/([ab]{64})\/([^?]+)(?:\?.*)?$/.exec(url);
    assert.ok(match, `A request leaked to a global/default route: ${url}`);
    const [,root,suffix] = match, label = root === A ? "A" : "B";
    if (suffix === "status") return response({revision:pair,workspaceRoot:`/checkout/${label}`,stats:{files:1},diagnostics:[`diagnostic-${label}`]});
    if (suffix === "tree") {
      if (root === A && holdA) return new Promise(resolve => {releaseA = resolve;});
      return response({revision:pair,path:"",root:`/checkout/${label}`,items:[{kind:"file",name:"same.rs",path:"same.rs",methodCount:1}],nextOffset:null});
    }
    if (suffix === "source") return response({revision:pair,file:{path:"same.rs",text:`source-${label}`}});
    if (suffix === "jobs/current") return response(null);
    if (suffix === "dependencies") return response({state:"disabled",workspaceRevision:pair,catalogId:null,packages:[],warnings:[]});
    if (["index","navigation"].includes(suffix)) return response({});
    throw new Error(`Unexpected ${url} ${options.method}`);
  }});
  await h.connect("synthetic",false,false);
  assert.equal(h.run("selectedRootKey"),null);
  assert.deepEqual(h.requests.map(r => r.url),["/api/checkouts","/api/daemon/status"]);
  h.get("checkout-select").value=A;
  await h.get("checkout-select").listeners.change({target:h.get("checkout-select")});
  await flush(); await flush();
  assert.equal(h.get("checkout-root").textContent,"/checkout/A");
  holdA=true;const old=h.run("loadTreeRoot()");
  assert.ok(releaseA,"A tree request must remain deferred");
  const switchAt=h.requests.length;
  h.get("checkout-select").value=B;
  await h.get("checkout-select").listeners.change({target:h.get("checkout-select")});
  await flush(); await flush();
  await h.run("showSource({path:'same.rs',range:{startLine:1,endLine:1}},status.revision)");
  for (const [route,method] of [["/api/navigation","POST"],["/api/index","POST"]]) {
    await h.run(`api('${route}','${method}',{})`);
  }
  const afterB=h.requests.slice(switchAt);
  assert.ok(afterB.length>=4);
  assert.ok(afterB.every(({url}) => url.startsWith(`/api/checkouts/${B}/`)), JSON.stringify(afterB.map(r=>r.url)));
  assert.ok(afterB.some(({url,options}) => url === `/api/checkouts/${B}/source?path=same.rs&indexGeneration=${pair.indexGeneration}&indexRevision=1` && options.method === "GET"));
  assert.ok(afterB.some(({url,options}) => url === `/api/checkouts/${B}/navigation` && options.method === "POST"));
  assert.ok(afterB.some(({url,options}) => url === `/api/checkouts/${B}/index` && options.method === "POST"));
  releaseA({ok:false,status:401,json:async()=>({error:{message:"late-A-auth"}})});
  await old;
  assert.equal(h.run("selectedRootKey"),B);
  assert.equal(h.get("checkout-root").textContent,"/checkout/B");
  assert.match(h.get("diagnostics").textContent,/diagnostic-B/);
  assert.doesNotMatch(h.get("diagnostics").textContent,/diagnostic-A/);
  const sourceText = function walk(node) { return [node.textContent,...(node.children || []).flatMap(walk)].join(" "); };
  assert.match(sourceText(h.get("source")),/source-B/);
  assert.equal(h.get("connect-form").hidden,true);
  assert.equal(h.get("error").hidden,true);
});

test("one available checkout never auto-selects; list retry recovers without scoped traffic", async () => {
  let fail=true;
  const h=harness({routes:(url,opts)=>{
    if(url==="/api/checkouts") return fail ? {ok:false,status:503,json:async()=>({error:{message:"list temporarily unavailable"}})}
      : response({checkouts:[{rootKey:ROOT,workspaceRoot:"/synthetic",state:"available",active:false}]});
    if(url==="/api/daemon/status")return response({activeCheckouts:0});
    throw new Error(`Scoped call before explicit selection: ${url}`);
  }});
  await h.connect("synthetic",false,false);
  assert.equal(h.run("selectedRootKey"),null);
  assert.equal(h.get("checkout-select").disabled,true);
  assert.equal(h.get("checkout-retry").hidden,false);
  fail=false;h.get("checkout-retry").listeners.click();await flush();
  assert.equal(h.get("checkout-select").disabled,false);
  assert.equal(h.get("workspace").hidden,true);
  assert.equal(h.run("selectedRootKey"),null);
  assert.ok(h.requests.every(({url,options})=>["/api/checkouts","/api/daemon/status"].includes(url) && options.method==="GET"));
});

test("unavailable or corrupt checkout rows cannot be selected", async () => {
  const bad="c".repeat(64), h=harness({routes:(url)=>{
    if(url==="/api/checkouts")return response({checkouts:[{rootKey:ROOT,workspaceRoot:"/synthetic",state:"unavailable",active:false},{rootKey:bad,state:"corrupt",active:false}]});
    if(url==="/api/daemon/status")return response({activeCheckouts:0});
    throw new Error(`Unavailable selection made a scoped call: ${url}`);
  }});
  await h.connect("synthetic",false,false);
  assert.equal(h.get("checkout-select").disabled,true);
  h.get("checkout-select").value=ROOT;h.get("checkout-select").listeners.change({target:h.get("checkout-select")});
  await flush();
  assert.equal(h.run("selectedRootKey"),null);
  assert.equal(h.get("workspace").hidden,true);
  assert.match(h.get("checkout-state").textContent,/available checkout/i);
});

test("cold checkout keeps its explicit selection and never invents evidence", async () => {
  const h=harness({routes:url=>{
    if(url==="/api/checkouts")return response({checkouts:[{rootKey:ROOT,workspaceRoot:"/synthetic",state:"available",active:false}]});
    if(url==="/api/daemon/status")return response({activeCheckouts:0});
    if(url===`${SCOPED}/status`)return {ok:false,status:503,json:async()=>({error:{code:"index_not_ready",message:"Cold checkout"}})};
    throw new Error(`Cold checkout leaked request: ${url}`);
  }});
  await h.connect("synthetic",false,false);
  h.get("checkout-select").value=ROOT;h.get("checkout-select").listeners.change({target:h.get("checkout-select")});await flush();
  assert.equal(h.run("selectedRootKey"),ROOT);
  assert.equal(h.run("status"),null);
  assert.match(h.get("checkout-state").textContent,/Index not ready/i);
  assert.equal(h.get("checkout-root").textContent.includes("/synthetic"),false,"unverified display path is not evidence");
  assert.deepEqual(h.requests.map(r=>r.url),["/api/checkouts","/api/daemon/status",`${SCOPED}/status`]);
});

test("catching-up prior head remains paired and clears on a refreshed status", async () => {
  const pair={indexGeneration:"12345678-1234-4123-8123-123456789abc",indexRevision:1};
  let catchingUp=true;
  const h=harness({routes:url=>{
    if(url==="/api/checkouts")return response({checkouts:[{rootKey:ROOT,workspaceRoot:"/synthetic",state:"available",active:false}]});
    if(url==="/api/daemon/status")return response({activeCheckouts:0});
    assert.ok(url.startsWith(`${SCOPED}/`),`Wrong root: ${url}`);
    if(url===`${SCOPED}/status`)return response({workspaceRoot:"/synthetic",revision:pair,catchingUp,stats:{}});
    if(url.startsWith(`${SCOPED}/tree?`))return response({revision:pair,path:"",root:"/synthetic",items:[],nextOffset:null});
    if(url===`${SCOPED}/jobs/current`)return response(null);
    if(url===`${SCOPED}/dependencies`)return response({state:"disabled",workspaceRevision:pair,catalogId:null,packages:[],warnings:[]});
    throw new Error(`Unexpected: ${url}`);
  }});
  await h.connect("synthetic",false,false);
  h.get("checkout-select").value=ROOT;h.get("checkout-select").listeners.change({target:h.get("checkout-select")});await flush();
  assert.equal(h.get("catching-up").hidden,false);
  assert.match(fs.readFileSync(path.join(__dirname,"../web/index.html"),"utf8"),/Catching up — evidence may be stale/);
  assert.equal(h.run("status.revision.indexRevision"),1);
  catchingUp=false;await h.run("refreshStatus()");
  assert.equal(h.get("catching-up").hidden,true);
  assert.equal(h.run("status.revision.indexRevision"),1);
  assert.equal(h.get("checkout-root").textContent,"/synthetic");
});

test("state change in refreshed checkout list clears a selected root", async () => {
  let state="available";
  const h=harness({routes:url=>{
    if(url==="/api/checkouts")return response({checkouts:[{rootKey:ROOT,workspaceRoot:"/synthetic",state,active:false}]});
    if(url==="/api/daemon/status")return response({activeCheckouts:0});
    if(url===`${SCOPED}/status`)return response({workspaceRoot:"/synthetic",revision:null,stats:{}});
    throw new Error(`Unexpected: ${url}`);
  }});
  await h.connect("synthetic",false,false);
  h.get("checkout-select").value=ROOT;h.get("checkout-select").listeners.change({target:h.get("checkout-select")});await flush();
  assert.equal(h.run("selectedRootKey"),ROOT);
  state="unavailable";await h.run("loadCheckouts()");await flush();
  assert.equal(h.run("selectedRootKey"),null);
  assert.equal(h.get("workspace").hidden,true);
  assert.match(h.get("checkout-state").textContent,/available checkouts|available checkout|choose/i);
});

test("a warm head is erased on typed cold status and a later ready head loads cleanly", async () => {
  const P={indexGeneration:"12345678-1234-4123-8123-123456789abc",indexRevision:1};
  const Q={...P,indexRevision:2};
  let phase="warm";
  const h=harness({routes:url=>{
    if(url==="/api/checkouts")return response({checkouts:[{rootKey:ROOT,workspaceRoot:"/synthetic",state:"available",active:false}]});
    if(url==="/api/daemon/status")return response({activeCheckouts:0});
    assert.ok(url.startsWith(`${SCOPED}/`),`Unselected request: ${url}`);
    if(url===`${SCOPED}/status`)return phase==="cold"
      ? {ok:false,status:503,json:async()=>({error:{code:"index_not_ready",message:"No admitted head"}})}
      : phase==="unavailable" ? {ok:false,status:503,json:async()=>({error:{code:"checkout_capacity",message:"Retry later"}})}
      : response({workspaceRoot:"/synthetic",revision:phase==="warm"?P:Q,stats:{},diagnostics:[`diagnostic-${phase}`]});
    if(url.startsWith(`${SCOPED}/tree?`))return response({revision:phase==="warm"?P:Q,path:"",root:"/synthetic",items:[],nextOffset:null});
    if(url===`${SCOPED}/jobs/current`)return response(null);
    if(url===`${SCOPED}/dependencies`)return response({state:"disabled",workspaceRevision:phase==="warm"?P:Q,catalogId:null,packages:[],warnings:[]});
    if(url.startsWith(`${SCOPED}/source?`))return response({revision:P,file:{path:"same.rs",text:"old-P-source"}});
    throw new Error(`Unexpected request: ${url}`);
  }});
  await h.connect("synthetic",true);
  await h.run("showSource({path:'same.rs',range:{startLine:1,endLine:1}},status.revision)");
  h.run("selectedMethod={id:'old-P'}; job={id:'old-P',state:'queued'};");
  assert.equal(h.run("status.revision.indexRevision"),1);
  assert.equal(h.run("sourceCache.size"),1);
  assert.match(h.get("source-path").textContent,/same.rs/);
  phase="unavailable";
  await assert.rejects(h.run("refreshStatus()"),error=>error.code==="checkout_capacity");
  assert.equal(h.run("status.revision.indexRevision"),1,"an unrelated 503 must not claim a cold head");
  assert.equal(h.run("sourceCache.size"),1);

  phase="cold";
  await assert.rejects(h.run("refreshStatus()"),error=>error.code==="index_not_ready");
  assert.equal(h.run("selectedRootKey"),ROOT);
  assert.equal(h.run("token"),"synthetic");
  assert.equal(h.get("checkout-select").value,ROOT);
  assert.equal(h.run("status"),null);
  for(const value of ["selectedMethod","job"])
    assert.equal(h.run(value),null,`${value} retained P`);
  assert.equal(h.run("sourceCache.size"),0);
  assert.equal(h.get("source").children.length,0);
  assert.equal(h.get("catching-up").hidden,true);
  assert.match(h.get("checkout-state").textContent,/Index not ready.*Retry/i);
  assert.doesNotMatch(h.get("diagnostics").textContent,/diagnostic-warm/);
  assert.doesNotMatch(h.get("source-path").textContent,/same.rs/);

  phase="ready";
  await h.run("refreshStatus()");
  assert.equal(h.run("status.revision.indexRevision"),2);
  assert.match(h.get("diagnostics").textContent,/diagnostic-ready/);
  assert.equal(h.run("sourceCache.size"),0);
  assert.equal(h.get("checkout-root").textContent,"/synthetic");
});

for(const states of [["available","available"],["available","unavailable"]])
  test(`duplicate checkout root key ${states.join("/")} rejects list before selection`,async()=>{
    let duplicate=true;
    const h=harness({routes:url=>{
      if(url==="/api/checkouts")return response({checkouts:duplicate
        ? states.map((state,i)=>({rootKey:ROOT,workspaceRoot:`/synthetic-${i}`,state,active:false}))
        : [{rootKey:ROOT,workspaceRoot:"/synthetic",state:"available",active:false}]});
      if(url==="/api/daemon/status")return response({activeCheckouts:0});
      throw new Error(`Duplicate checkout sent a scoped request: ${url}`);
    }});
    await h.connect("synthetic",false,false);
    assert.equal(h.run("selectedRootKey"),null);
    assert.equal(h.run("checkouts.length"),0);
    assert.equal(h.get("checkout-select").disabled,true);
    assert.equal(h.get("checkout-select").children.length,1);
    assert.equal(h.get("checkout-retry").hidden,false);
    assert.match(h.get("checkout-state").textContent,/Duplicate checkout root key/i);
    h.get("checkout-select").value=ROOT;
    h.get("checkout-select").listeners.change({target:h.get("checkout-select")});
    await assert.rejects(h.run("api('/api/status')"),/Choose an available checkout/);
    assert.equal(h.get("workspace").hidden,true);
    assert.ok(h.requests.every(({url})=>["/api/checkouts","/api/daemon/status"].includes(url)));
    duplicate=false;h.get("checkout-retry").listeners.click();await flush();
    assert.equal(h.get("checkout-select").disabled,false);
    assert.equal(h.run("selectedRootKey"),null,"fresh valid list must not auto-select");
    assert.ok(h.requests.every(({url})=>["/api/checkouts","/api/daemon/status"].includes(url)));
  });

test("a refreshed ambiguous selected key clears the old root before another scoped call",async()=>{
  let duplicate=false;
  const h=harness({routes:url=>{
    if(url==="/api/checkouts")return response({checkouts:duplicate
      ? [{rootKey:ROOT,workspaceRoot:"/synthetic",state:"available"},{rootKey:ROOT,workspaceRoot:"/other",state:"unavailable"}]
      : [{rootKey:ROOT,workspaceRoot:"/synthetic",state:"available"}]});
    if(url==="/api/daemon/status")return response({activeCheckouts:0});
    if(!duplicate && url===`${SCOPED}/status`)return response({workspaceRoot:"/synthetic",revision:null,stats:{}});
    throw new Error(`Ambiguous checkout sent a scoped request: ${url}`);
  }});
  await h.connect("synthetic",false);
  assert.equal(h.run("selectedRootKey"),ROOT);
  duplicate=true;const before=h.requests.length;
  assert.equal(await h.run("loadCheckouts()"),false);
  assert.equal(h.run("selectedRootKey"),null);
  assert.equal(h.run("status"),null);
  assert.equal(h.get("workspace").hidden,true);
  assert.equal(h.get("checkout-select").disabled,true);
  assert.deepEqual(h.requests.slice(before).map(request=>request.url),["/api/checkouts"]);
  await assert.rejects(h.run("api('/api/status')"),/Choose an available checkout/);
  assert.deepEqual(h.requests.slice(before).map(request=>request.url),["/api/checkouts"]);
});

for (const otherState of ["available","unavailable"])
  test(`selected URL builder rejects an ambiguous ${otherState} root before fetch`,async()=>{
    const h=harness();
    h.run(`token='synthetic'; selectedRootKey='${ROOT}'; checkouts=[
      {rootKey:'${ROOT}',workspaceRoot:'/first',state:'available'},
      {rootKey:'${ROOT}',workspaceRoot:'/second',state:'${otherState}'}];`);
    await assert.rejects(h.run("api('/api/status')"),/Choose an available checkout/);
    assert.equal(h.requests.length,0);
  });

test("selected checkout survives typed storage_busy without scoped traffic and recovers at Q", async () => {
  const P={indexGeneration:"12345678-1234-4123-8123-123456789abc",indexRevision:1};
  const Q={...P,indexRevision:2};
  let phase="warm";
  const h=harness({routes:url=>{
    if(url==="/api/checkouts")return response({checkouts:[{rootKey:ROOT,workspaceRoot:"/synthetic",state:phase==="busy"?"storage_busy":"available",active:true}]});
    if(url==="/api/daemon/status")return response({activeCheckouts:1});
    if(phase==="busy")throw new Error(`Scoped request during typed storage_busy: ${url}`);
    assert.ok(url.startsWith(`${SCOPED}/`),`Unexpected route: ${url}`);
    const revision=phase==="warm"?P:Q;
    if(url===`${SCOPED}/status`)return response({workspaceRoot:"/synthetic",revision,stats:{},diagnostics:[`diagnostic-${phase}`]});
    if(url.startsWith(`${SCOPED}/tree?`))return response({revision,path:"",root:"/synthetic",items:[],nextOffset:null});
    if(url===`${SCOPED}/jobs/current`)return response(null);
    if(url===`${SCOPED}/dependencies`)return response({state:"disabled",workspaceRevision:revision,catalogId:null,packages:[],warnings:[]});
    if(url.startsWith(`${SCOPED}/source?`))return response({revision:P,file:{path:"same.rs",text:"source-P"}});
    throw new Error(`Unexpected: ${url}`);
  }});
  await h.connect("synthetic",true);
  await h.run("showSource({path:'same.rs',range:{startLine:1,endLine:1}},status.revision)");
  assert.equal(h.run("status.revision.indexRevision"),1);
  assert.equal(h.run("sourceCache.size"),1);
  phase="busy";
  const before=h.requests.length;
  await h.run("loadCheckouts()");
  assert.equal(h.run("selectedRootKey"),ROOT,"transient read-probe busy must retain explicit root");
  assert.equal(h.run("token"),"synthetic");
  assert.equal(h.get("checkout-select").value,ROOT);
  assert.equal(h.get("checkout-root").textContent,"/synthetic");
  assert.equal(h.get("workspace").hidden,true,"old evidence must be nonactionable");
  assert.equal(h.run("status"),null);
  assert.equal(h.run("sourceCache.size"),0);
  assert.equal(h.get("source").children.length,0);
  assert.equal(h.get("checkout-retry").hidden,false);
  assert.match(h.get("checkout-state").textContent,/busy.*retry/i);
  await assert.rejects(h.run("api('/api/status')"),/Choose an available checkout/);
  assert.deepEqual(h.requests.slice(before).map(request=>request.url),["/api/checkouts"]);
  await h.get("checkout-retry").listeners.click(); await flush();
  assert.equal(h.run("selectedRootKey"),ROOT);
  assert.ok(h.requests.slice(before).every(({url})=>["/api/checkouts","/api/daemon/status"].includes(url)),"busy retry must be global-only");
  phase="ready";
  await h.get("checkout-retry").listeners.click(); await flush();
  assert.equal(h.run("selectedRootKey"),ROOT);
  assert.equal(h.run("status.revision.indexRevision"),2);
  assert.equal(h.get("workspace").hidden,false);
  assert.match(h.get("diagnostics").textContent,/diagnostic-ready/);
  assert.equal(h.run("sourceCache.size"),0);
  assert.doesNotMatch(h.get("source-path").textContent,/same.rs/);
  assert.ok(h.requests.slice(before).filter(({url})=>url.startsWith(`${SCOPED}/`)).every(({url})=>phase==="ready" && !url.includes("source")));
});

for(const unavailable of ["unavailable","corrupt","missing","changed","duplicate"])
  test(`selected checkout ${unavailable} is not treated as transient busy`,async()=>{
    let phase="warm";
    const h=harness({routes:url=>{
      if(url==="/api/checkouts")return response({checkouts:phase==="warm"?[{rootKey:ROOT,workspaceRoot:"/synthetic",state:"available"}]
        : phase==="missing"?[]:phase==="changed"?[{rootKey:ROOT,workspaceRoot:"/other",state:"storage_busy"}]
        : phase==="duplicate"?[{rootKey:ROOT,workspaceRoot:"/synthetic",state:"storage_busy"},{rootKey:ROOT,workspaceRoot:"/synthetic",state:"available"}]
        :[{rootKey:ROOT,workspaceRoot:"/synthetic",state:phase}]});
      if(url==="/api/daemon/status")return response({activeCheckouts:0});
      if(phase==="warm"&&url===`${SCOPED}/status`)return response({workspaceRoot:"/synthetic",revision:null,stats:{}});
      throw new Error(`Unexpected scoped request after ${phase}: ${url}`);
    }});
    await h.connect("synthetic",false);
    assert.equal(h.run("selectedRootKey"),ROOT);
    phase=unavailable;const at=h.requests.length;await h.run("loadCheckouts()");await flush();
    assert.equal(h.run("selectedRootKey"),null);
    assert.equal(h.run("status"),null);
    assert.equal(h.get("workspace").hidden,true);
    assert.ok(h.requests.slice(at).every(({url})=>url==="/api/checkouts"));
  });

const gitBranch = (name, upstream = null) => ({state:"available",head:{kind:"branch",name,commit:"abcdef123456",shortCommit:"abcdef1"},upstream});
const coldStatus = () => ({ok:false,status:503,json:async()=>({error:{code:"index_not_ready",message:"No admitted head"}})});

test("checkout options and cold header share measured Git labels without extra requests on connect or selection",async()=>{
  const rows=[
    {rootKey:ROOT,workspaceRoot:"/repo/branch",state:"available",git:gitBranch("feature/<script>",{name:"origin/main",ahead:2,behind:1,basis:"last_fetch"})},
    {rootKey:"b".repeat(64),workspaceRoot:"/repo/no-upstream",state:"available",git:gitBranch("local")},
    {rootKey:"c".repeat(64),workspaceRoot:"/repo/detached",state:"available",git:{state:"available",head:{kind:"detached",name:null,commit:"abcdef123456",shortCommit:"abcdef1"},upstream:null}},
    {rootKey:"d".repeat(64),workspaceRoot:"/repo/non-git",state:"available",git:{state:"unavailable",reason:"not_repository"}},
    {rootKey:"e".repeat(64),workspaceRoot:"/repo/missing-git",state:"available"}
  ];
  const labels=["feature/<script> · origin/main ↑2 ↓1 · As of last fetch","local · No upstream","Detached HEAD abcdef1 · No upstream","Git unavailable","Git unavailable"];
  const h=harness({routes:url=>{
    if(url==="/api/checkouts")return response({checkouts:rows});
    if(url==="/api/daemon/status")return response({activeCheckouts:0});
    if(rows.some(row=>url===`/api/checkouts/${row.rootKey}/status`))return coldStatus();
    throw new Error(`Unexpected Git or provider request: ${url}`);
  }});
  assert.equal(h.requests.length,0);
  await h.connect("synthetic",false,false);
  assert.deepEqual(h.requests.map(r=>r.url),["/api/checkouts","/api/daemon/status"]);
  for(const [i,row] of rows.entries()) {
    const option=h.get("checkout-select").children[i+1];
    assert.equal(option.textContent,`${row.workspaceRoot} · available · ${labels[i]}`);
    assert.equal(option.children.length,0,"repository labels stay text, not markup");
    await h.run(`selectCheckout(${JSON.stringify(row.rootKey)})`);
    assert.equal(h.get("workspace-git").textContent,labels[i]);
    assert.equal(h.get("workspace-git").hidden,false);
    assert.equal(h.run("status"),null);
    assert.equal(h.run("selectedRootKey"),row.rootKey);
    h.context.window.TrellisShell.updateWorkspace(null);
    assert.equal(h.get("workspace-git").textContent,labels[i]);
  }
  assert.equal(h.requests.length,2+rows.length);
  h.get("logout").listeners.click();
  assert.equal(h.get("workspace-git").textContent,"");
  assert.equal(h.get("workspace-git").hidden,true);
});

test("explicit status refresh reads Git even with a cold index; list refresh and read failure update only Git",async()=>{
  let git=gitBranch("before"),fail=false;
  const h=harness({routes:(url,options)=>{
    assert.equal(options.method,"GET");
    if(url==="/api/checkouts")return response({checkouts:[{rootKey:ROOT,workspaceRoot:"/repo",state:"available",git}]});
    if(url==="/api/daemon/status")return response({activeCheckouts:0});
    if(url===`${SCOPED}/status`)return coldStatus();
    if(url===`${SCOPED}/git`)return fail ? {ok:false,status:503,json:async()=>({error:{message:"Read unavailable"}})} : response({git,changes:[]});
    throw new Error(`Unexpected request: ${url}`);
  }});
  await h.connect("synthetic",false,false);await h.run(`selectCheckout('${ROOT}')`);
  git=gitBranch("after",{name:"origin/after",ahead:0,behind:4,basis:"last_fetch"});
  const before=h.requests.length;
  await h.get("refresh").listeners.click();
  assert.deepEqual(h.requests.slice(before).map(r=>r.url),[`${SCOPED}/git`,`${SCOPED}/status`]);
  assert.equal(h.get("workspace-git").textContent,"after · origin/after ↑0 ↓4 · As of last fetch");
  assert.equal(h.run("status"),null);
  git=gitBranch("from-list");await h.run("loadCheckouts()");
  assert.equal(h.get("workspace-git").textContent,"from-list · No upstream");
  fail=true;await h.run("refreshGit()");
  assert.equal(h.get("workspace-git").textContent,"Git unavailable");
  assert.equal(h.run("selectedRootKey"),ROOT);
  const requests=h.requests.length;
  await assert.rejects(h.run("api('/api/git','POST',{})"),/must be GET/);
  assert.equal(h.requests.length,requests);
});

test("late Git success or authentication failure from A cannot overwrite B or disconnect it",async()=>{
  for(const lateAuth of [false,true]) {
    const B="b".repeat(64);let finish;
    const rows=[ROOT,B].map((rootKey,i)=>({rootKey,workspaceRoot:i?"/B":"/A",state:"available",git:gitBranch(i?"branch-B":"branch-A")}));
    const h=harness({routes:url=>{
      if(url==="/api/checkouts")return response({checkouts:rows});
      if(url==="/api/daemon/status")return response({activeCheckouts:0});
      if(url.endsWith("/status"))return coldStatus();
      if(url===`${SCOPED}/git`)return new Promise(resolve=>{finish=resolve;});
      throw new Error(`Wrong-root Git request: ${url}`);
    }});
    await h.connect("synthetic",false,false);await h.run(`selectCheckout('${ROOT}')`);
    const pending=h.run("refreshGit()");
    assert.ok(finish);
    await h.run(`selectCheckout('${B}')`);
    finish(lateAuth?{ok:false,status:401,json:async()=>({error:{message:"obsolete authentication"}})}:response({git:gitBranch("obsolete-A"),changes:[]}));
    await pending;
    assert.equal(h.run("selectedRootKey"),B);
    assert.equal(h.get("workspace-git").textContent,"branch-B · No upstream");
    assert.equal(h.get("connect-form").hidden,true);
    assert.equal(h.get("error").hidden,true);
  }
});

test("newer selected Git read wins over an older read and an in-flight list summary",async()=>{
  let holdList=false,finishList;const reads=[];
  const row=git=>({rootKey:ROOT,workspaceRoot:"/repo",state:"available",git});
  const h=harness({routes:url=>{
    if(url==="/api/checkouts")return holdList?new Promise(resolve=>{finishList=resolve;}):response({checkouts:[row(gitBranch("initial"))]});
    if(url==="/api/daemon/status")return response({activeCheckouts:0});
    if(url===`${SCOPED}/status`)return coldStatus();
    if(url===`${SCOPED}/git`)return new Promise(resolve=>reads.push(resolve));
    throw new Error(`Unexpected request: ${url}`);
  }});
  await h.connect("synthetic",false,false);await h.run(`selectCheckout('${ROOT}')`);
  holdList=true;const listed=h.run("loadCheckouts()"),older=h.run("refreshGit()"),newer=h.run("refreshGit()");
  reads[1](response({git:gitBranch("newest"),changes:[]}));await newer;
  reads[0](response({git:gitBranch("older"),changes:[]}));await older;
  finishList(response({checkouts:[row(gitBranch("older-list"))]}));await listed;
  assert.equal(h.get("workspace-git").textContent,"newest · No upstream");
});

test("Disconnect clears selected Git and rejects an in-flight Git read",async()=>{
  let finish;
  const h=harness({routes:url=>{
    if(url==="/api/checkouts")return response({checkouts:[{rootKey:ROOT,workspaceRoot:"/repo",state:"available",git:gitBranch("selected")}]});
    if(url==="/api/daemon/status")return response({activeCheckouts:0});
    if(url===`${SCOPED}/status`)return coldStatus();
    if(url===`${SCOPED}/git`)return new Promise(resolve=>{finish=resolve;});
    throw new Error(`Unexpected request: ${url}`);
  }});
  await h.connect("synthetic",false,false);await h.run(`selectCheckout('${ROOT}')`);
  const pending=h.run("refreshGit()");
  h.get("logout").listeners.click();
  finish(response({git:gitBranch("obsolete"),changes:[]}));await pending;
  assert.equal(h.get("workspace-git").textContent,"");
  assert.equal(h.get("workspace-git").hidden,true);
  assert.equal(h.run("selectedGit"),null);
  assert.equal(h.run("selectedRootKey"),null);
});

const checkoutNodes = node => [node,...node.children.flatMap(checkoutNodes)];
test("checkout list shows measured freshness and shares explicit selection with the picker", async () => {
  const pin = {indexGeneration:"12345678-1234-4123-8123-123456789abc",indexRevision:1};
  const rows = [
    {rootKey:ROOT,workspaceRoot:"/synthetic",state:"available",revision:pin,catchingUp:false},
    {rootKey:"b".repeat(64),workspaceRoot:"/catching-up",state:"available",revision:pin,catchingUp:true},
    {rootKey:"c".repeat(64),workspaceRoot:"/idle",state:"available",revision:pin,catchingUp:null},
    {rootKey:"d".repeat(64),workspaceRoot:"/cold",state:"available",revision:null,catchingUp:null,indexError:"index_not_ready"},
    {rootKey:"e".repeat(64),workspaceRoot:"/failed",state:"unavailable",revision:null}
  ];
  const h = harness({routes:url => {
    if (url === "/api/checkouts") return response({checkouts:rows});
    if (url === "/api/daemon/status") return response({activeCheckouts:0});
    if (url === `${SCOPED}/status`) return response({workspaceRoot:"/synthetic",revision:pin,catchingUp:false,stats:{}});
    if (url.startsWith(`${SCOPED}/tree?`)) return response({revision:pin,path:"",root:"/synthetic",indexedWorkspace:"/synthetic",items:[],nextOffset:null});
    if (url === `${SCOPED}/dependencies`) return response({state:"disabled",workspaceRevision:pin,catalogId:null,packages:[],warnings:[]});
    if (url === `${SCOPED}/jobs/current`) return response(null);
    throw new Error("Unexpected " + url);
  }});
  await h.connect("synthetic",false,false);
  const nodes = checkoutNodes(h.get("indexed-checkouts"));
  const labels = nodes.filter(node => node.className === "checkout-freshness");
  assert.deepEqual(labels.map(node => node.attrs["data-state"]),["fresh","catching-up","unknown","unknown","failed"]);
  assert.match(labels[0].textContent,/Fresh.*12345678:1/);
  assert.match(labels[1].textContent,/Catching up.*12345678:1/);
  assert.match(labels[2].textContent,/Indexed · idle/);
  assert.match(labels[3].textContent,/Not indexed/);
  assert.match(labels[4].textContent,/Failed/);
  const picks = nodes.filter(node => node.className === "checkout-pick");
  assert.equal(picks[4].disabled,true);
  assert.ok(picks.slice(0,4).every(node => !node.disabled));
  assert.equal(h.run("selectedRootKey"),null);
  assert.deepEqual(h.requests.map(request => request.url),["/api/checkouts","/api/daemon/status"]);
  const before = h.requests.length;
  await picks[0].listeners.click(); await flush();
  const listRequests = h.requests.slice(before).map(request => request.url);
  assert.equal(h.run("selectedRootKey"),ROOT);
  assert.equal(h.get("checkout-select").value,ROOT);
  assert.equal(h.get("checkout-state").textContent,"Checkout ready.");
  h.run("resetCheckout()");
  const pickerStart = h.requests.length;
  h.get("checkout-select").value=ROOT;
  h.get("checkout-select").listeners.change({target:h.get("checkout-select")}); await flush();
  assert.deepEqual(h.requests.slice(pickerStart).map(request => request.url),listRequests);
  assert.equal(h.run("selectedRootKey"),ROOT);
  assert.equal(h.get("checkout-state").textContent,"Checkout ready.");
});

test("newer checkout list freshness updates the selected card and header at the same snapshot pin", async () => {
  const pin = {indexGeneration:"12345678-1234-4123-8123-123456789abc",indexRevision:1};
  let listedCatchingUp = false;
  const h = harness({routes:url => {
    if (url === "/api/checkouts") return response({checkouts:[{rootKey:ROOT,workspaceRoot:"/synthetic",state:"available",revision:{...pin},catchingUp:listedCatchingUp}]});
    if (url === "/api/daemon/status") return response({activeCheckouts:0});
    if (url === `${SCOPED}/status`) return response({workspaceRoot:"/synthetic",revision:pin,catchingUp:false,stats:{}});
    if (url.startsWith(`${SCOPED}/tree?`)) return response({revision:pin,path:"",root:"/synthetic",indexedWorkspace:"/synthetic",items:[],nextOffset:null});
    if (url === `${SCOPED}/dependencies`) return response({state:"disabled",workspaceRevision:pin,catalogId:null,packages:[],warnings:[]});
    if (url === `${SCOPED}/jobs/current`) return response(null);
    throw new Error("Unexpected " + url);
  }});
  await h.connect("synthetic",false);
  assert.equal(h.run("status.catchingUp"),false);
  assert.equal(h.get("workspace-index-state").attrs["data-state"],"fresh");
  listedCatchingUp = true;
  const before = h.requests.length;
  await h.run("loadCheckouts()");
  const label = checkoutNodes(h.get("indexed-checkouts")).find(node => node.className === "checkout-freshness");
  assert.equal(label.attrs["data-state"],"catching-up");
  assert.match(label.textContent,/Catching up/);
  assert.equal(h.get("workspace-index-state").attrs["data-state"],"catching-up");
  assert.match(h.get("workspace-index-label").textContent,/Catching up/);
  assert.equal(h.run("status.catchingUp"),false,"cached status must not override the newer list observation");
  assert.deepEqual(h.requests.slice(before).map(request => request.url),["/api/checkouts"]);
  assert.deepEqual(JSON.parse(JSON.stringify(h.run("status.revision"))),pin);
});
