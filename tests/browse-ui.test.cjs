"use strict";
// Synthetic data only. Run: node --test tests/browse-ui.test.cjs
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
function harness(windowOptions = {}) {
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
    window: {confirm: () => true, ...windowOptions},
    URL: {createObjectURL(blob) { blobs.push(blob); return "blob:synthetic"; }, revokeObjectURL() {}},
    document: {addEventListener() {}, getElementById: get, createElement: node, createElementNS: (ns, tag) => node(tag), createDocumentFragment: node, createTextNode: text => ({textContent:text}), body: node()},
    fetch() { throw new Error("Unexpected request"); }});
  const run = code => vm.runInContext(code, context);
  run(source);
  run(`token = 'synthetic'; selectedRootKey = '${ROOT}'; checkouts = [{rootKey:'${ROOT}',workspaceRoot:'/synthetic',state:'available',active:false}]; seed = 'root'; status = {revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1}};`);
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
  const preserveNewFocus = () => {
    run(`querySerial++; questionSerial++; status = {revision:{...status.revision,indexRevision:status.revision.indexRevision+1}};
      packet = {packetId:'new'}; focused = {marker:'new'};
      $('focus-state').textContent = 'new valid focus'; $('error').hidden = true;`);
    return run("packet");
  };
  const assertPreserved = newer => {
    assert.equal(run("packet"), newer);
    assert.equal(get("focus-state").textContent, "new valid focus");
    assert.equal(get("error").hidden, true);
  };
  return {context, run, get, blobs, downloads, timers, networkUrls, preserveNewFocus, assertPreserved};
}


const symbol = id => ({id, name:id, path:"src/a.js", range:{startLine:1,endLine:5}});
const view = id => ({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},seed:symbol(id),participants:[{id,label:id,kind:"method"},{id:"unknown",label:"external?",kind:"boundary"}],steps:[],warnings:[],hiddenSteps:0,truncated:false});
const response = data => ({ok:true,status:200,json:async()=>data});
function descendants(node) { return [node, ...node.children.flatMap(descendants)]; }
function text(node) { return descendants(node).map(n=>n.textContent).join(" "); }

test("file tree expands methods inline, backend flags filter conservatively, pagination and local path filter", async () => {
  const h = harness(), paths = [];
  h.context.fetch = async (path) => {
    paths.push(path);
    if (path.startsWith("/api/files")) return response({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},items:[{path:paths.length===1?"src/a.js":"lib/b.js",methodCount:2}],nextOffset:paths.length===1?1:null});
    return response({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},items:[{symbol:symbol("check"),consequential:true,reason:"validation"},{symbol:symbol("getter"),consequential:false,reason:"trivial"}],truncated:false});
  };
  await h.run("loadFiles(true)");
  await h.run("toggleFile(files[0])");
  assert.match(text(h.get("file-tree")), /check/); assert.doesNotMatch(text(h.get("file-tree")), /getter/);
  assert.equal(descendants(h.get("file-tree")).filter(n=>n.tagName==="summary")[0].textContent,"src");
  h.get("all-methods").checked=true; h.run("renderFiles()"); assert.match(text(h.get("file-tree")), /getter/);
  await h.run("loadFiles()"); assert.match(paths.at(-1), /offset=1/);
  h.get("file-filter").value="lib/"; h.run("renderFiles()"); assert.doesNotMatch(text(h.get("file-tree")), /a.js/); assert.match(text(h.get("file-tree")), /b.js/);
  assert.ok(paths.every(p=>p.startsWith("/api/files")||p.startsWith("/api/methods")));
});

for (const failure of ["success","http","network","json"]) test(`obsolete diagram ${failure} cannot overwrite newer selection`, async () => {
  const h=harness(), old=deferred();
  h.context.fetch=()=>old.promise;
  const pending=h.run(`selectMethod(${JSON.stringify(symbol("old"))})`);
  h.context.fetch=async()=>response(view("new"));
  await h.run(`selectMethod(${JSON.stringify(symbol("new"))})`);
  if(failure==="network") old.reject(new Error("old network"));
  else if(failure==="http") old.resolve({ok:false,status:409,json:async()=>({error:{message:"obsolete"}})});
  else if(failure==="json") old.resolve({ok:true,status:200,json:async()=>{throw new Error("old json");}});
  else old.resolve(response(view("old")));
  await pending;
  assert.match(h.get("sequence-state").textContent,/^new/); assert.equal(h.run("selectedMethod.id"),"new");
  assert.equal(h.get("error").hidden,true);
});

test("file close and reopen invalidates old method failures", async()=>{
  const h=harness(), old=deferred(); h.run(`files=[{path:'src/a.js',methodCount:1}];`);
  h.context.fetch=()=>old.promise; const pending=h.run("toggleFile(files[0])");
  await h.run("toggleFile(files[0])");
  h.context.fetch=async()=>response({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},items:[{symbol:symbol("fresh"),consequential:true}],truncated:false});
  await h.run("toggleFile(files[0])");
  old.resolve({ok:false,status:409,json:async()=>({error:{message:"obsolete"}})}); await pending;
  assert.match(text(h.get("file-tree")),/fresh/); assert.equal(h.run("files.length"),1);
});

for(const invalidate of ["clearBrowse()", "status={revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:2}}", "$('logout').listeners.click()"])
 test(`obsolete catalog success after ${invalidate} is ignored`,async()=>{
  const h=harness(), old=deferred(); h.context.fetch=()=>old.promise;
  const pending=h.run("loadFiles(true)"); h.run(invalidate);
  old.resolve(response({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},items:[{path:"old.js",methodCount:1}],nextOffset:null})); await pending;
  assert.equal(h.run("files.length"),0);
});

test("sequence selection sends no query or provider calls; Show all steps is explicit",async()=>{
  const h=harness(), requests=[];
  h.context.fetch=async(path,options)=>{requests.push([path,JSON.parse(options.body)]); return response(view("root"));};
  await h.run(`selectMethod(${JSON.stringify(symbol("root"))})`);
  h.get("all-steps").checked=true; await h.get("all-steps").listeners.change();
  assert.deepEqual(requests.map(r=>r[0]),["/api/sequence","/api/sequence"]);
  assert.equal(requests[0][1].showAll,false); assert.equal(requests[1][1].showAll,true);
  assert.deepEqual(requests[0][1].expectedRevision,JSON.parse(JSON.stringify(h.run("status.revision"))));
});

test("native SVG preserves nested branch loop try, safe labels, unknown lifelines and keyboard source",()=>{
  const h=harness(), dto=view("<img src=x onerror=boom>"), selected=[];
  const step=(id,kind,children=[],alternate=[])=>({id,kind,label:id,path:"src/a.js",range:{startLine:2,endLine:2},target:"unknown",children,alternate});
  dto.steps=[step("condition","branch",[step("repeat","loop",[step("<script>bad</script>","call")])],[step("attempt","try",[step("write","effect")],[step("finally","boundary",[step("cleanup","call")])])])];
  dto.steps.push({...step("hidden-log", "call"), hidden:true});
  h.context.dto=dto; h.context.readSource=s=>selected.push(s);
  h.run("renderSequence($('sequence-diagram'), dto, readSource)");
  const all=descendants(h.get("sequence-diagram")), branch=all.find(n=>n.attrs?.["data-kind"]==="branch");
  assert.ok(descendants(branch).some(n=>n.attrs?.["data-kind"]==="loop"));
  const attempt=all.find(n=>n.attrs?.["data-kind"]==="try");
  assert.ok(descendants(attempt).some(n=>n.attrs?.["data-step-id"]==="cleanup"));
  assert.ok(all.some(n=>n.tagName==="svg")); assert.equal(all.filter(n=>n.attrs?.class==="sequence-arrow").length,0);
  assert.ok(all.some(n=>n.attrs?.["data-presentation"]==="quiet-note"));
  assert.ok(all.every(n=>!["script","img"].includes(n.tagName)));
  assert.doesNotMatch(text(h.get("sequence-diagram")), /hidden-log/);
  assert.doesNotMatch(text(h.get("sequence-diagram")), /Unknown target/);
  assert.match(text(h.get("sequence-diagram")), /<script>bad<\/script>/);
  const control=all.find(n=>n.attrs?.role==="button"); let prevented=false;
  control.listeners.keydown({key:"Enter",preventDefault(){prevented=true;}});
  assert.ok(prevented); assert.equal(selected[0].id,"condition");
});

test("current sequence errors replace loading state and mismatch never paints",async()=>{
  const h=harness(); h.context.fetch=async()=>response(view("wrong"));
  await h.run(`selectMethod(${JSON.stringify(symbol("root"))})`);
  assert.match(h.get("sequence-state").textContent,/provenance mismatch/); assert.equal(h.get("sequence-diagram").children.length,0);
  h.context.fetch=async()=>({ok:false,status:409,json:async()=>({error:{code:"revision_conflict",message:"refresh required"}})});
  await h.run("loadSequence()"); assert.equal(h.run("selectedMethod"),null); assert.equal(h.get("sequence-state").textContent,"refresh required");
});

const directory = h => descendants(h.get("file-tree")).find(n => n.tagName === "details");
test("folders show files without a search and keep explicit collapse through rerenders", () => {
  const h = harness(); h.run("files=[{path:'src/a.js',methodCount:1}]; renderFiles()");
  const first = directory(h); assert.equal(first.open, true);
  first.open = false; first.listeners.toggle(); h.run("renderFiles()");
  assert.equal(directory(h).open, false);
  h.get("all-methods").checked = true; h.get("all-methods").listeners.change();
  assert.equal(directory(h).open, false);
  directory(h).open = true; directory(h).listeners.toggle();
  directory(h).open = false; directory(h).listeners.toggle(); h.run("renderFiles()");
  assert.equal(directory(h).open, false);
});
test("filter auto-open and detached native toggle events cannot change folder preference", () => {
  const h = harness(); h.run("files=[{path:'src/a.js',methodCount:1}]; renderFiles()");
  const old = directory(h); old.open = false; old.listeners.toggle();
  h.get("file-filter").value = "a.js"; h.run("renderFiles()");
  assert.equal(directory(h).open, true); directory(h).listeners.toggle();
  old.open = true; old.listeners.toggle();
  h.get("file-filter").value = ""; h.run("renderFiles()");
  assert.equal(directory(h).open, false);
});
test("same-revision refresh retains paginated files, open methods, collapsed folders and selection", async () => {
  const h = harness();
  h.run(`files=[{path:'src/a.js',methodCount:1},{path:'lib/b.js',methodCount:1}]; nextFileOffset=400;
    closedDirectories.add('lib/'); fileStates.set('src/a.js',{open:true,items:[]}); selectedMethod={id:'selected'};`);
  h.context.fetch = async () => response({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},items:[{path:'src/a.js',methodCount:1}],nextOffset:200});
  await h.run("loadFiles(true)");
  assert.equal(h.run("files.length"), 2); assert.equal(h.run("nextFileOffset"), 400);
  assert.equal(h.run("fileStates.get('src/a.js').open"), true);
  assert.equal(h.run("closedDirectories.has('lib/')"), true); assert.equal(h.run("selectedMethod.id"), "selected");
});
for (const fail of [false,true]) test(`obsolete catalog ${fail ? 'failure' : 'success'} cannot overwrite newer refresh`, async () => {
  const h = harness(), old = deferred(); h.context.fetch = () => old.promise;
  const pending = h.run("loadFiles(true)");
  h.context.fetch = async () => response({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},items:[{path:'new.js',methodCount:0}],nextOffset:null});
  await h.run("loadFiles(true)");
  if (fail) old.reject(new Error('obsolete')); else old.resolve(response({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},items:[{path:'old.js',methodCount:0}],nextOffset:200}));
  await pending;
  assert.equal(h.run("files.length"), 1); assert.equal(h.run("files[0].path"), "new.js");
  assert.match(h.get("files-state").textContent, /loaded files/);
});
test("empty paths explain indexing and unmatched filter explains loaded-only scope", async () => {
  const h = harness(); h.run("renderFiles()"); assert.match(text(h.get("file-tree")), /Index workspace/);
  h.run("files=[{path:'src/a.js',methodCount:0}]"); h.get("file-filter").value = "missing"; h.run("renderFiles()");
  assert.match(text(h.get("file-tree")), /Clear the filter or load more/);
});
test("catalog errors have an explicit retry and successful retry clears it", async () => {
  const h = harness(); h.context.fetch = async () => { throw new Error('offline'); };
  await h.run("loadFiles(true)"); assert.equal(h.get("retry-files").hidden, false);
  h.context.fetch = async () => response({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},items:[],nextOffset:null});
  await h.run("loadFiles(retryCatalogReset)"); assert.equal(h.get("retry-files").hidden, true);
});
test("reveal selected method clears filter and reopens its folder and file", () => {
  const h = harness(); h.context.method = symbol('selected');
  h.run("files=[{path:'src/a.js',methodCount:1}]; selectedMethod=method; fileStates.set('src/a.js',{open:false,items:[{symbol:method,consequential:false}]}); closedDirectories.add('src/')");
  h.get("file-filter").value = 'missing'; h.get("reveal-method").listeners.click();
  assert.equal(h.get("file-filter").value, ''); assert.equal(directory(h).open, true);
  assert.equal(h.run("fileStates.get('src/a.js').open"), true);
  assert.ok(descendants(h.get('file-tree')).some(n => n.attrs?.['aria-pressed'] === 'true'));
});

const treePage = (path, items, nextOffset = null) => ({root:'/cwd',indexedWorkspace:'/cwd/sample',path,revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},items,nextOffset,truncated:false});
const folder = path => ({path,name:path.split('/').at(-1),kind:'directory',indexedPath:null,methodCount:null});
const treeFile = (path, indexedPath = null) => ({path,name:path.split('/').at(-1),kind:'file',indexedPath,methodCount:indexedPath ? 1 : null});
test("cwd root renders files without a filter and expands folders lazily", async () => {
  const h = harness(), requests=[];
  h.context.fetch = async path => { requests.push(path); return response(path.includes('path=src') ? treePage('src',[treeFile('src/lib.rs')]) : treePage('',[folder('src'),treeFile('README.md')])); };
  await h.run('loadTreeRoot()');
  assert.match(text(h.get('file-tree')), /README.md/); assert.match(text(h.get('file-tree')), /src/);
  assert.equal(requests.length,1); assert.match(h.get('browse-root').textContent, /Browsing: \/cwd · Index workspace: \/cwd\/sample/);
  await h.run("toggleDirectory('src')"); assert.match(text(h.get('file-tree')), /lib.rs/);
  await h.run("toggleDirectory('src')"); await h.run("toggleDirectory('src')"); assert.equal(requests.length,2);
});
test("one compact row lazily follows an exact sole-directory chain and exposes its full path", async () => {
  const h = harness(), requests = [];
  const pages = new Map([
    ["", [folder("java")]],
    ["java", [folder("java/com")]],
    ["java/com", [folder("java/com/example")]],
    ["java/com/example", [folder("java/com/example/build")]],
    ["java/com/example/build", [folder("java/com/example/build/otel")]],
    ["java/com/example/build/otel", [treeFile("java/com/example/build/otel/Agent.java")]]
  ]);
  h.context.fetch = async request => {
    requests.push(request);
    const value = new URL(request, "http://test").searchParams.get("path");
    return response(treePage(value, pages.get(value)));
  };
  await h.run("loadTreeRoot()");
  assert.deepEqual(requests.map(value => new URL(value, "http://test").searchParams.get("path")), [""]);
  await h.run("toggleDirectory('java')");
  assert.deepEqual(requests.map(value => new URL(value, "http://test").searchParams.get("path")),
    ["", "java", "java/com", "java/com/example", "java/com/example/build", "java/com/example/build/otel"]);
  assert.ok(requests.every(value => value.startsWith("/api/tree?")));
  const picks = descendants(h.get("file-tree")).filter(node => node.tagName === "button");
  assert.equal(picks.length, 1);
  assert.equal(picks[0].textContent, "▾ java/com/example/build/otel/");
  assert.equal(picks[0].title, "java/com/example/build/otel/");
  assert.equal(picks[0].attrs["aria-label"], "Collapse folder java/com/example/build/otel/");
  assert.equal(picks[0].attrs["aria-expanded"], "true");
  assert.match(text(h.get("file-tree")), /Agent\.java/);
});

test("compact rows never cross branches, files, pagination, truncation, errors, or loading states", () => {
  const h = harness();
  const cases = ["branch", "file", "paged", "truncated", "error", "loading", "exact"];
  h.run(`treeMode=true;
    directories.set('', {open:true,items:${JSON.stringify(cases.map(folder))},nextOffset:null,truncated:false});`);
  h.run(`directories.set('branch', {open:false,items:[${JSON.stringify(folder("branch/a"))},${JSON.stringify(folder("branch/b"))}],nextOffset:null,truncated:false});
    directories.set('file', {open:false,items:[${JSON.stringify(treeFile("file/a.txt"))}],nextOffset:null,truncated:false});
    directories.set('paged', {open:false,items:[${JSON.stringify(folder("paged/child"))}],nextOffset:200,truncated:false});
    directories.set('truncated', {open:false,items:[${JSON.stringify(folder("truncated/child"))}],nextOffset:null,truncated:true});
    directories.set('error', {open:false,items:[${JSON.stringify(folder("error/child"))}],nextOffset:null,truncated:false,error:'denied'});
    directories.set('loading', {open:false,items:[${JSON.stringify(folder("loading/child"))}],nextOffset:null,truncated:false,loading:true});
    directories.set('exact', {open:false,items:[${JSON.stringify(folder("exact/child"))}],nextOffset:null,truncated:false});
    renderDirectoryTree();`);
  const labels = descendants(h.get("file-tree")).filter(node => node.tagName === "button").map(node => node.textContent);
  assert.ok(labels.includes("▸ exact/child/"));
  for (const name of cases.slice(0, -1)) {
    assert.ok(labels.includes(`▸ ${name}/`), name);
    assert.ok(!labels.some(label => label.startsWith(`▸ ${name}/child/`)), name);
  }
});

test("collapsing a compact expansion while its request is pending does not reopen it", async () => {
  const h = harness(), pending = deferred(), requests = [];
  h.context.fetch = request => {
    requests.push(request);
    const value = new URL(request, "http://test").searchParams.get("path");
    return value === "" ? Promise.resolve(response(treePage("", [folder("java")]))) : pending.promise;
  };
  await h.run("loadTreeRoot()");
  const expansion = h.run("toggleDirectory('java')");
  await h.run("toggleDirectory('java')");
  pending.resolve(response(treePage("java", [folder("java/com")])));
  await expansion;
  assert.equal(requests.length, 2);
  assert.equal(h.run("directories.get('java').open"), false);
  h.run("renderDirectoryTree()");
  const pick = descendants(h.get("file-tree")).find(node => node.tagName === "button");
  assert.equal(pick.textContent, "▸ java/com/");
  assert.equal(pick.attrs["aria-expanded"], "false");
});

test("retrying a failed compact endpoint resumes the same anchor through its descendants", async () => {
  const h = harness(), requests = [];
  let buildAttempts = 0;
  h.context.fetch = async request => {
    requests.push(request);
    const value = new URL(request, "http://test").searchParams.get("path");
    if (value === "") return response(treePage("", [folder("java")]));
    if (value === "java") return response(treePage("java", [folder("java/com")]));
    if (value === "java/com") return response(treePage("java/com", [folder("java/com/build")]));
    if (value === "java/com/build" && buildAttempts++ === 0) throw new Error("denied once");
    if (value === "java/com/build") return response(treePage(value, [folder("java/com/build/otel")]));
    if (value === "java/com/build/otel") return response(treePage(value, [treeFile(value + "/Agent.java")]));
    throw new Error("unexpected " + value);
  };
  await h.run("loadTreeRoot()");
  await h.run("toggleDirectory('java')");
  assert.match(text(h.get("file-tree")), /denied once/);
  await h.run("retryDirectory('java/com/build', 'java')");
  assert.match(text(h.get("file-tree")), /java\/com\/build\/otel\//);
  assert.match(text(h.get("file-tree")), /Agent\.java/);
  assert.deepEqual(requests.map(value => new URL(value, "http://test").searchParams.get("path")),
    ["", "java", "java/com", "java/com/build", "java/com/build", "java/com/build/otel"]);
});

test("same-revision refresh resumes an open anchor whose branch becomes a sole-directory chain", async () => {
  const h = harness(), requests = [];
  let refreshed = false;
  h.context.fetch = async request => {
    requests.push(request);
    const value = new URL(request, "http://test").searchParams.get("path");
    if (value === "") return response(treePage("", [folder("java")]));
    if (value === "java" && !refreshed) return response(treePage("java", [folder("java/old"), treeFile("java/note.txt")]));
    if (value === "java") return response(treePage("java", [folder("java/com")]));
    if (value === "java/com") return response(treePage("java/com", [treeFile("java/com/Agent.java")]));
    throw new Error("unexpected " + value);
  };
  await h.run("loadTreeRoot()");
  await h.run("toggleDirectory('java')");
  refreshed = true;
  await h.run("refreshTree()");
  assert.match(text(h.get("file-tree")), /java\/com\//);
  assert.match(text(h.get("file-tree")), /Agent\.java/);
  assert.equal(requests.filter(value => new URL(value, "http://test").searchParams.get("path") === "java/com").length, 1);
});

test("stale compact expansion cannot continue into another session request", async () => {
  const h = harness(), pending = deferred(), requests = [];
  h.context.fetch = request => { requests.push(request); return requests.length === 1
    ? Promise.resolve(response(treePage("", [folder("java")]))) : pending.promise; };
  await h.run("loadTreeRoot()");
  const expansion = h.run("toggleDirectory('java')");
  h.run("epoch++; directories.clear();");
  pending.resolve(response(treePage("java", [folder("java/com")])));
  await expansion;
  assert.equal(requests.length, 2);
  assert.equal(h.run("directories.size"), 0);
});

test("Reveal method reopens all known ancestors of a compact row", () => {
  const h = harness();
  h.context.method = {...symbol("selected"), path:"Agent.java"};
  h.context.entry = treeFile("java/com/Agent.java", "Agent.java");
  h.run(`treeMode=true; selectedMethod=method;
    directories.set('', {open:true,items:[${JSON.stringify(folder("java"))}],nextOffset:null,truncated:false});
    directories.set('java', {open:false,items:[${JSON.stringify(folder("java/com"))}],nextOffset:null,truncated:false});
    directories.set('java/com', {open:false,items:[entry],nextOffset:null,truncated:false});
    fileStates.set('Agent.java',{open:false,items:[{symbol:method,consequential:true}]});
    renderDirectoryTree();`);
  h.get("file-filter").value = "missing";
  h.get("reveal-method").listeners.click();
  assert.equal(h.run("directories.get('java').open"), true);
  assert.equal(h.run("directories.get('java/com').open"), true);
  assert.equal(h.run("fileStates.get('Agent.java').open"), true);
  assert.ok(descendants(h.get("file-tree")).some(node => node.attrs?.["aria-pressed"] === "true"));
});

test("compact directory rendering and expansion stop at the depth bound", async () => {
  const h = harness(), depth = 40;
  const paths = Array.from({length:depth}, (_, index) => Array.from({length:index + 1}, (_, part) => `d${part + 1}`).join("/"));
  h.context.paths = paths;
  h.run(`treeMode=true; directories.set('', {open:true,items:[{path:paths[0],name:'d1',kind:'directory'}],nextOffset:null,truncated:false});
    for (let index=0; index<paths.length; index++) directories.set(paths[index], {open:false,items:index+1<paths.length?[{path:paths[index+1],name:'d'+(index+2),kind:'directory'}]:[],nextOffset:null,truncated:false});
    renderDirectoryTree();`);
  await h.run("toggleDirectory(paths[0])");
  const picks = descendants(h.get("file-tree")).filter(node => node.tagName === "button");
  assert.equal(picks[0].title, paths[31] + "/");
  assert.doesNotMatch(picks[0].textContent, /d33/);
  assert.equal(h.run("directories.get(paths[31]).open"), true);
  assert.equal(h.run("directories.get(paths[32]).open"), false);
});

test("nonindexed files and symlinks expose metadata without clickable source or methods", async () => {
  const h=harness(); h.context.fetch=async()=>response(treePage('',[treeFile('.env'),{...treeFile('link'),kind:'symlink'}]));
  await h.run('loadTreeRoot()');
  assert.match(text(h.get('file-tree')), /Not indexed in current workspace/);
  assert.match(text(h.get('file-tree')), /Symbolic link · not followed/);
  assert.equal(descendants(h.get('file-tree')).filter(n=>n.tagName==='button').length,0);
});
test("indexed tree file uses indexedPath rather than cwd path for inline methods", async () => {
  const h=harness(), requests=[];
  h.context.fetch=async path=>{requests.push(path); return response(path.startsWith('/api/tree') ? treePage('',[treeFile('sample/a.js','a.js')]) : {revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},items:[{symbol:symbol('method'),consequential:true}],truncated:false});};
  await h.run('loadTreeRoot()');
  const pick=descendants(h.get('file-tree')).find(n=>n.tagName==='button');
  pick.listeners.click(); await new Promise(resolve=>setImmediate(resolve));
  assert.match(requests[1], /path=a.js/); assert.doesNotMatch(requests[1], /sample/);
  assert.match(text(h.get('file-tree')), /method/);
});
test("folder response cannot reopen a folder closed while loading", async () => {
  const h=harness(), pending=deferred(); h.run('treeMode=true'); h.context.fetch=()=>pending.promise;
  const request=h.run("toggleDirectory('src')"); await h.run("toggleDirectory('src')");
  pending.resolve(response(treePage('src',[treeFile('src/a')] ))); await request;
  assert.equal(h.run("directories.get('src').open"),false);
});
for (const fail of [false,true]) test(`old folder ${fail?'error':'success'} cannot replace newer refresh`, async () => {
  const h=harness(), pending=deferred(); h.context.fetch=()=>pending.promise;
  const request=h.run('loadTreeRoot()');
  h.context.fetch=async()=>response(treePage('',[treeFile('new')])); await h.run('loadTreeRoot()');
  if(fail) pending.reject(new Error('old error')); else pending.resolve(response(treePage('',[treeFile('old')])));
  await request; assert.match(text(h.get('file-tree')),/new/); assert.doesNotMatch(text(h.get('file-tree')),/old/);
});
test("folder pagination and refresh retain loaded pages and collapse state", async () => {
  const h=harness(); let n=0; h.context.fetch=async path=>response(treePage('',[treeFile(path.includes('offset=1&')?'second':'first')],path.includes('offset=1&')?null:1));
  await h.run('loadTreeRoot()'); await h.run("loadDirectory('')");
  assert.match(text(h.get('file-tree')),/second/);
  await h.run('refreshTree()'); assert.equal(h.run("directories.get('').items.length"),2);
});
test("folder failure offers retry and no matches explains unloaded folders", async () => {
  const h=harness(); h.context.fetch=async()=>{throw new Error('Permission denied');}; await h.run('loadTreeRoot()');
  assert.match(text(h.get('file-tree')),/Permission denied/); assert.match(text(h.get('file-tree')),/Retry folder/);
  h.get('file-filter').value='missing'; h.run('renderFiles()'); assert.match(h.get('files-state').textContent,/unopened folders are not searched/);
});

test("filter reveals loaded descendants without changing collapsed folder preference", async () => {
  const h=harness(); h.context.fetch=async path=>response(path.includes('path=src')?treePage('src',[treeFile('src/a.js')]):treePage('',[folder('src')]));
  await h.run('loadTreeRoot()'); await h.run("toggleDirectory('src')"); await h.run("toggleDirectory('src')");
  h.get('file-filter').value='a.js'; h.run('renderFiles()'); assert.match(text(h.get('file-tree')),/a.js/);
  h.get('file-filter').value=''; h.run('renderFiles()'); assert.doesNotMatch(text(h.get('file-tree')),/a.js/);
  assert.equal(h.run("directories.get('src').open"),false);
});
for(const invalidation of ["clearBrowse()", "status={revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:2}}", "$('logout').listeners.click()"])
 test(`cwd tree ignores pending response after ${invalidation}`,async()=>{
  const h=harness(), old=deferred();h.context.fetch=()=>old.promise;
  const pending=h.run('loadTreeRoot()'); h.run(invalidation);
  old.resolve(response(treePage('',[treeFile('obsolete')]))); await pending;
  assert.doesNotMatch(text(h.get('file-tree')),/obsolete/);
 });

test("successful reconnect clears an earlier authentication error", async () => {
  const h=harness(); h.get('error').hidden=false; h.get('error').textContent='Bearer authentication required';
  h.get('token').value='synthetic';
  h.context.fetch = async url => url === "/api/checkouts" ? response({checkouts:[{rootKey:ROOT,workspaceRoot:"/synthetic",state:"available",active:false}]})
    : url === "/api/daemon/status" ? response({activeCheckouts:0}) : response(null);
  h.run('refreshStatus=async()=>{}; loadSaved=async()=>{}; refreshJevStatus=async()=>{}; refreshAcpStatus=async()=>{}');
  h.get('connect-form').listeners.submit({preventDefault(){}});
  await new Promise(resolve=>setImmediate(resolve));
  h.get('checkout-select').value=ROOT; await h.get('checkout-select').listeners.change({target:h.get('checkout-select')});
  assert.equal(h.get('error').hidden,true); assert.equal(h.get('error').textContent,'');
  assert.equal(h.get('workspace').hidden,false);
});

test("file browser puts optional controls behind collapsed details and reserves mobile tree space", () => {
  const html = fs.readFileSync(path.join(__dirname, '../web/index.html'),'utf8');
  const css = fs.readFileSync(path.join(__dirname, '../web/style.css'),'utf8');
  assert.match(html, /<details class="browse-options"><summary>Filter &amp; display options<\/summary><label for="file-filter">/);
  assert.doesNotMatch(html, /<details class="browse-options" open/);
  assert.match(css, /@media\s*\(max-width:\s*900px\)/);
  assert.match(css, /\.file-browser,\s*\.call-inspector\s*\{[^}]*position:\s*absolute;[^}]*top:\s*0;[^}]*bottom:\s*0;[^}]*width:\s*min\(310px,\s*88%\)/);
  assert.match(css, /body\.explorer-open \.file-browser[^}]*display:\s*flex/);
  assert.doesNotMatch(css, /max-height:\s*(?:40|55)vh/);
});


test("server unindexed reasons are plain text, legacy reasons remain, and indexed files ignore reasons", async () => {
  const h = harness();
  h.context.fetch = async () => response(treePage('', [
    {...treeFile('main.rs'), unindexedReason:'Rust indexing not supported yet'},
    {...treeFile('a.ts'), unindexedReason:'TypeScript indexing not supported yet'},
    {...treeFile('unsafe'), unindexedReason:'<img src=x onerror=boom>'},
    treeFile('legacy'),
    {...treeFile('indexed.rs', 'indexed.rs'), unindexedReason:'MUST NOT SHOW'}
  ]));
  await h.run('loadTreeRoot()');
  const rendered = text(h.get('file-tree'));
  assert.match(rendered, /Rust indexing not supported yet/);
  assert.match(rendered, /TypeScript indexing not supported yet/);
  assert.match(rendered, /Not indexed in current workspace/);
  assert.match(rendered, /<img src=x onerror=boom>/);
  assert.doesNotMatch(rendered, /MUST NOT SHOW/);
  const nodes = descendants(h.get('file-tree'));
  assert.equal(nodes.filter(n => n.tagName === 'button').length, 1);
  assert.ok(nodes.every(n => n.tagName !== 'img'));
  assert.equal(h.run('fileStates.size'), 0);
});

test("index scope mismatch stays in primary roots and action label, same root adds no warning", async () => {
  const h = harness();
  h.context.fetch = async () => response(treePage('', []));
  await h.run('loadTreeRoot()');
  assert.match(h.get('browse-root').textContent, /Browsing: \/cwd · Index workspace: \/cwd\/sample/);
  assert.match(h.get('browse-root').textContent, /Indexing applies only to the index workspace/);
  assert.equal(h.get('index').textContent, 'Index sample');
  h.get('file-filter').value = 'missing'; h.run('renderDirectoryTree()');
  assert.match(h.get('browse-root').textContent, /Index workspace: \/cwd\/sample/);
  h.context.fetch = async () => response({...treePage('', []), indexedWorkspace:'/cwd'});
  await h.run('loadTreeRoot()');
  assert.equal(h.get('browse-root').textContent, '/cwd');
  assert.equal(h.get('index').textContent, 'Index workspace');
});


test("workspace change invalidates selections and source even when revision stays equal", async () => {
  const h = harness(), requests = [];
  h.run(`status = {revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1}, workspaceRoot:'/old'}; seed='old';
    result={revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},calls:[],nodes:[]}; packet={packetId:'old'}; focused={};
    selectedMethod={id:'old'}; sourceCache.set('old','cached');
    $('source').append(element('span','old source'));`);
  h.context.fetch = async path => {
    requests.push(path);
    return response(path === '/api/status' ? {revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},workspaceRoot:'/new'}
      : {...treePage('',[treeFile('new.rs')]),root:'/new',indexedWorkspace:'/new'});
  };
  await h.run('refreshStatus()');
  for (const name of ['seed','result','packet','focused','selectedMethod']) assert.equal(h.run(name), null, name);
  assert.equal(h.run('sourceCache.size'), 0);
  assert.equal(h.get('source').children.length, 0);
  assert.ok(requests.some(path => path.startsWith('/api/tree')));
  assert.equal(h.get('browse-root').textContent, '/new');
  assert.match(text(h.get('file-tree')), /new.rs/);
});


test("class context actions use cached file/method identity and never fetch source automatically", async () => {
  const h = harness(), opened = [], menus = [];
  h.context.window.TrellisClasses = {showContextMenu: (event, actions) => menus.push(actions), open: opts => opened.push(opts)};
  h.run(`attachClassMenu($('class-file'), {path:'src/Thing.java'}); attachClassMenu($('class-method'), {path:'src/Thing.java',seed:'method'});`);
  let prevented = false;
  h.get("class-file").listeners.contextmenu({preventDefault(){prevented=true;}});
  assert.ok(prevented); assert.equal(opened.length, 0);
  await menus[0][0].run(); assert.equal(opened[0].path, "src/Thing.java");
  h.get("class-method").listeners.keydown({key:"F10",shiftKey:true,preventDefault(){}});
  await menus[1][0].run(); assert.equal(opened[1].seed, "method");
});

test("obsolete class context actions cannot cross revision or session changes", async () => {
  const h = harness(), menus = [], opened = [];
  h.context.window.TrellisClasses = {showContextMenu: (e,a)=>menus.push(a),open:opts=>opened.push(opts)};
  h.run(`attachClassMenu($('class-file'), {path:'Thing.py'});`);
  h.get("class-file").listeners.contextmenu({preventDefault(){}});
  h.run("status={revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:2}}"); await menus[0][0].run();
  assert.equal(opened.length,0);
  h.get("class-file").listeners.contextmenu({preventDefault(){throw new Error('obsolete menu');}});
  assert.equal(menus.length,1);
});

test("class controller uses authenticated API and clears pending source on navigation", async () => {
  const h = harness(); let hooks;
  h.context.window.TrellisClasses={init:value=>{hooks=value;}}; h.run("initClassView()");
  const requests=[];
  h.context.fetch=async(p,o)=>{requests.push([p,o]);return response({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1}});};
  await hooks.request('/api/class-diagram',{method:'POST',body:{seed:'class',expectedRevision:1}});
  assert.equal(requests.length,1);assert.equal(requests[0][0],'/api/class-diagram');
  assert.equal(requests[0][1].headers.Authorization,'Bearer synthetic');
  assert.deepEqual(JSON.parse(requests[0][1].body),{seed:'class',expectedRevision:1});
  const before=h.run('sourceSerial');hooks.onChange();assert.ok(h.run('sourceSerial')>before);
  h.run("status={revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:2},workspaceRoot:'/different'}");assert.equal(hooks.currentRevision().indexRevision,2);
  assert.match(hooks.currentSession(),/different/);
});

test("unsupported class languages keep their ordinary file and method interactions", () => {
  const h=harness();h.context.window.TrellisClasses={showContextMenu(){throw new Error('unexpected menu');}};
  h.run(`attachClassMenu($('rust-file'),{path:'src/main.rs'}); attachClassMenu($('js-file'),{path:'app.js'});`);
  assert.equal(h.get('rust-file').listeners.contextmenu,undefined);
  assert.equal(h.get('js-file').listeners.contextmenu,undefined);
});


test("source navigation attaches without lookup and follows source serial and dock visibility", async () => {
  const h=harness(), attached=[]; let resets=0;
  h.context.window.TrellisNavigation={reset(){resets++;},attachSource(node,options){attached.push({node,options});}};
  h.context.window.TrellisShell={showSource(){h.get('source-dock').hidden=false;h.get('workspace-source-panel').hidden=false;}};
  h.get('source').scrollIntoView=()=>{};
  const requests=[];h.context.fetch=async p=>{requests.push(p);return response({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},file:{path:'sample.py',text:'def run():\n    work()\n'}});};
  await h.run("showSource({path:'sample.py',range:{startLine:2,endLine:2}},status.revision)");
  assert.equal(requests.length,1);assert.ok(requests[0].startsWith('/api/source?'));
  assert.equal(attached.length,1);assert.equal(attached[0].options.path,'sample.py');assert.equal(attached[0].options.startLine,2);
  assert.ok(attached[0].options.isCurrent());
  h.get('source-dock').hidden=true;assert.equal(attached[0].options.isCurrent(),false);
  h.get('source-dock').hidden=false;h.get('workspace-source-panel').hidden=true;assert.equal(attached[0].options.isCurrent(),false);
  h.get('workspace-source-panel').hidden=false;h.run('clearSource()');assert.equal(attached[0].options.isCurrent(),false);assert.ok(resets>=2);
});

test("class member navigation forwards exact selectors and scope without fetching automatically", () => {
  const h=harness(), lookups=[];let hooks;
  h.context.window.TrellisClasses={init:opts=>{hooks=opts;}};
  h.context.window.TrellisNavigation={reset(){},open:(...args)=>lookups.push(args)};
  h.run('initClassView()');assert.equal(lookups.length,0);
  const event={type:'contextmenu'},selector={classId:'class-a',memberName:'field',startByte:10,endByte:20},options={isCurrent:()=>true};
  hooks.navigateMember(event,selector,options);
  assert.equal(lookups[0][0],event);assert.equal(lookups[0][1],selector);assert.equal(lookups[0][2],options);
});


test("clearing stale source closes the empty dock after navigation", () => {
  const h=harness();let closed=0,resets=0;
  h.context.window.TrellisNavigation={reset(){resets++;}};
  h.context.window.TrellisShell={closeSource(){closed++;h.get('source-dock').hidden=true;}};
  h.get('source-dock').hidden=false;h.get('source').append({textContent:'old'});
  h.run('clearSource()');assert.equal(closed,1);assert.equal(resets,1);assert.equal(h.get('source').children.length,0);
  h.run('clearSource()');assert.equal(closed,1);
});


test("navigation source action forwards measured range and snapshot without selecting a sequence", async () => {
  let hooks;const h=harness({TrellisNavigation:{init(options){hooks=options;},reset(){},attachSource(){}}});
  const requests=[];h.get('source').scrollIntoView=()=>{};
  h.context.window.TrellisShell={showSource(){h.get('source-dock').hidden=false;h.get('workspace-source-panel').hidden=false;}};
  h.context.fetch=async p=>{requests.push(p);return response({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},file:{path:'A.java',text:'class A {\n void target() {}\n}\n'}});};
  h.run("selectedMethod={id:'caller'}");
  const target={id:'target',path:'A.java',range:{startLine:2,endLine:2}};
  assert.equal(requests.length,0);await hooks.openSource(target,h.run("({indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1})"));
  assert.equal(requests.length,1);assert.match(requests[0],/^\/api\/source\?path=A.java&indexGeneration=12345678-1234-4123-8123-123456789abc&indexRevision=1$/);
  assert.equal(h.run('selectedMethod.id'),'caller');assert.ok(h.get('source').children[0].children[1].classList.contains('highlight'));
  h.run(`status={revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:2}}`);await hooks.openSource(target,h.run("({indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1})"));assert.equal(requests.length,1);assert.match(h.get('error').textContent,/snapshot|revision/i);
});

test("same numeric revision with new generation clears browse and source state", async () => {
  const h = harness(), old = deferred();
  h.run(`status={revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},workspaceRoot:'/same'};
    seed='old'; result={revision:status.revision,calls:[],nodes:[]}; sourceCache.set(IndexPin.key(status.revision)+':old.js',{file:{text:'old'}});`);
  h.context.fetch = async url => url === '/api/status' ? response({revision:{indexGeneration:'87654321-4321-4321-8321-abcdef123456',indexRevision:1},workspaceRoot:'/same',stats:{}}) : response({...treePage('',[]),revision:{indexGeneration:'87654321-4321-4321-8321-abcdef123456',indexRevision:1}});
  await h.run('refreshStatus()');
  assert.equal(h.run('seed'),null); assert.equal(h.run('result'),null);
  assert.equal(h.run('sourceCache.size'),0);
  assert.equal(h.run('status.revision.indexGeneration'),'87654321-4321-4321-8321-abcdef123456');
});
test("late catalog with reused numeric revision cannot paint the new generation", async () => {
  const h=harness(), old=deferred();h.context.fetch=()=>old.promise;
  const pending=h.run('loadFiles(true)');
  h.run(`status={revision:{indexGeneration:'87654321-4321-4321-8321-abcdef123456',indexRevision:1}}`);
  old.resolve(response({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},items:[{path:'old.js',methodCount:1}],nextOffset:null}));
  await pending;assert.equal(h.run('files.length'),0);
});

test("non-revision 409 preserves the loaded tree and reports its exact error", async () => {
  const h = harness(), requests = [];
  h.context.fetch = async url => {
    requests.push(url);
    if (url.startsWith("/api/tree")) return response(treePage("", [treeFile("README.md", "README.md")]));
    if (url.startsWith("/api/files") || url.startsWith("/api/methods") || url === "/api/query") return {ok:false,status:409,json:async()=>({error:{code:"storage_busy",message:"Storage is busy"}})};
    if (url === "/api/status") return response({revision:oldPair,stats:{}});
    if (url === "/api/dependencies") return response({state:"disabled",workspaceRevision:oldPair,catalogId:null,packages:[],warnings:[]});
    throw Error(`Unexpected request ${url}`);
  };
  await h.run("loadTreeRoot()");
  assert.match(text(h.get("file-tree")), /README.md/);
  await h.run("loadFiles(true)");
  assert.match(text(h.get("file-tree")), /README.md/);
  assert.equal(h.get("files-state").textContent, "Storage is busy");
  assert.equal(h.run("treeMode"), true);
  await h.run("toggleFile({path:'README.md',methodCount:1})");
  assert.match(text(h.get("file-tree")), /Storage is busy/);
  assert.match(text(h.get("file-tree")), /README.md/);
  await h.run("perform(() => api('/api/query'))");
  assert.match(text(h.get("file-tree")), /README.md/);
  assert.equal(h.get("error").textContent, "Storage is busy");
  assert.equal(requests.filter(url => url === "/api/status").length, 0);
  await h.run("refreshStatus()");
  assert.match(text(h.get("file-tree")), /README.md/);
  assert.equal(requests.filter(url => url.startsWith("/api/tree")).length, 2);
});

test("revision_conflict reloads the complete new pair after clearing browse", async () => {
  const h = harness(), requests = [];
  h.run(`status={revision:{indexGeneration:"12345678-1234-4123-8123-123456789abc",indexRevision:1},workspaceRoot:"/same"}`);
  h.context.fetch = async url => {
    requests.push(url);
    if (url.startsWith("/api/tree")) return response({...treePage("", [treeFile(requests.includes("/api/status") ? "fresh.md" : "old.md")]),
      revision:requests.includes("/api/status") ? newPair : oldPair});
    if (url.startsWith("/api/files")) return {ok:false,status:409,json:async()=>({error:{code:"revision_conflict",message:"Index changed"}})};
    if (url === "/api/status") return response({revision:newPair,workspaceRoot:"/same",stats:{}});
    if (url === "/api/dependencies") return response({state:"disabled",workspaceRevision:newPair,catalogId:null,packages:[],warnings:[]});
    throw Error(`Unexpected request ${url}`);
  };
  await h.run("loadTreeRoot()");
  assert.match(text(h.get("file-tree")), /old.md/);
  await h.run("loadFiles(true)");
  await new Promise(setImmediate);
  assert.equal(h.run("status.revision.indexGeneration"), newPair.indexGeneration);
  assert.match(text(h.get("file-tree")), /fresh.md/);
  assert.doesNotMatch(text(h.get("file-tree")), /old.md/);
  assert.equal(requests.filter(url => url.startsWith("/api/tree")).length, 2);
});

const oldPair = {indexGeneration:"12345678-1234-4123-8123-123456789abc", indexRevision:1};
const newPair = {indexGeneration:"87654321-4321-4321-8321-abcdef123456", indexRevision:1};

function canonicalSaved(kind, attachment, pair = oldPair) {
  if (kind === "view") return {view:{id:"view-a",title:"Saved A",query:{seed:"target-a",depth:2,maxNodes:41,maxCalls:201,includeCallbacks:false,excludePaths:["vendor/"]},pins:{"target-a":{x:1,y:2}},hidden:["hidden-a"],anchor:{never:"echo"}},orphanedIds:[],...pair,attachment};
  return {annotation:{id:"note-a",nodeId:"target-a",title:"Note A",body:"Body A",anchor:{never:"echo"}},orphaned:false,...pair,attachment};
}
const attached = () => ({availability:"ready",result:{status:"attached",targetId:"target-a",reason:"none"}});

for (const kind of ["view", "note"]) test(`${kind} saved response normalization fails closed for legacy and malformed metadata`, () => {
  const h=harness();
  h.run(`status={revision:${JSON.stringify(oldPair)},workspaceRoot:'/same',evidenceFormat:'native-v6'}`);
  const raw=kind==="view"
    ? {id:"raw",title:"Raw",query:{seed:"target-a"},anchor:{untrusted:true}}
    : {id:"raw",nodeId:"target-a",body:"Raw",anchor:{untrusted:true}};
  h.context.saved=raw;
  assert.equal(h.run(`normalizeSavedState(saved,'${kind}').attachment.availability`),"anchorless");
  const nested=kind==="view" ? {view:raw,orphanedIds:[]} : {annotation:raw,orphaned:false};
  h.context.saved=nested;
  assert.equal(h.run(`normalizeSavedState(saved,'${kind}').attachment.availability`),"anchorless");
  const valid=canonicalSaved(kind,attached());h.context.saved=valid;
  assert.equal(h.run(`savedLoadDecision(normalizeSavedState(saved,'${kind}'),'${kind}').enabled`),true);
  const partial={...valid,indexRevision:null};h.context.saved=partial;
  assert.equal(h.run(`savedLoadDecision(normalizeSavedState(saved,'${kind}'),'${kind}').enabled`),false);
  const mismatch=canonicalSaved(kind,{availability:"ready",result:{status:"attached",targetId:"other",reason:"none"}});h.context.saved=mismatch;
  assert.match(h.run(`savedLoadDecision(normalizeSavedState(saved,'${kind}'),'${kind}').reason`),/does not match/);
  for(const reason of ["missing","headerMismatch","groupChanged","unprovenContinuity"]){
    h.context.saved=canonicalSaved(kind,{availability:"ready",result:{status:"orphaned",targetId:null,reason}});
    const decision=h.run(`savedLoadDecision(normalizeSavedState(saved,'${kind}'),'${kind}')`);
    assert.equal(decision.enabled,false,reason);assert.notEqual(decision.reason,"",reason);
  }
  h.context.saved=canonicalSaved(kind,{availability:"indexUnavailable",result:null},{indexGeneration:null,indexRevision:null});
  assert.match(h.run(`savedLoadDecision(normalizeSavedState(saved,'${kind}'),'${kind}').reason`),/unavailable/);
});

test("attached view and note Load send one immutable full-pin query for the original target", async () => {
  const h=harness(), requests=[];
  h.run(`status={revision:${JSON.stringify(oldPair)},workspaceRoot:'/same',evidenceFormat:'native-v6'};seed='target-a'`);
  h.context.viewSaved=canonicalSaved("view",attached());h.context.noteSaved=canonicalSaved("note",attached());
  h.run("views=[normalizeSavedState(viewSaved,'view')];annotations=[normalizeSavedState(noteSaved,'note')];renderViews();renderNotes()");
  h.context.fetch=async(url,opts)=>{requests.push({url,body:opts.body&&JSON.parse(opts.body)});return response({revision:oldPair,query:JSON.parse(opts.body),calls:[],nodes:[]});};
  const viewLoad=descendants(h.get("views")).find(n=>n.attrs?.['data-action']==='load');
  await viewLoad.listeners.click();
  h.run("seed='wrong-selection'");
  const noteLoad=descendants(h.get("annotations")).find(n=>n.attrs?.['data-action']==='load');
  await noteLoad.listeners.click();
  assert.equal(requests.length,2);
  for(const request of requests){
    const url=new URL(request.url,"http://local");
    assert.equal(url.pathname,"/api/query");assert.equal(url.searchParams.get("indexGeneration"),oldPair.indexGeneration);assert.equal(url.searchParams.get("indexRevision"),"1");
    assert.equal(request.body.seed,"target-a");assert.equal(request.body.depth,1);assert.equal(request.body.includeCallbacks,false);
  }
});

for (const kind of ["view","note"]) test(`${kind} stale replay race keeps copied P and never retries at Q`, async () => {
  const h=harness(), requests=[];
  h.run(`status={revision:${JSON.stringify(oldPair)},workspaceRoot:'/same',evidenceFormat:'native-v6'};seed='target-a'`);
  h.context.saved=canonicalSaved(kind,attached());
  h.run(kind==="view" ? "views=[normalizeSavedState(saved,'view')];renderViews()" : "annotations=[normalizeSavedState(saved,'note')];renderNotes()");
  h.context.fetch=async(url,opts)=>{
    requests.push({url,method:opts.method});
    if(url.startsWith('/api/query?')) return {ok:false,status:409,json:async()=>({error:{code:'revision_conflict',message:'Index changed'}})};
    if(url==='/api/status') return response({revision:newPair,workspaceRoot:'/same',evidenceFormat:'native-v6',stats:{}});
    if(url.startsWith('/api/tree?')) return response({...treePage('',[]),revision:newPair});
    if(url==='/api/dependencies') return response({state:'disabled',workspaceRevision:newPair,packages:[],warnings:[]});
    throw Error(`Unexpected request ${url}`);
  };
  h.run("globalThis.originalRunQuery=runQuery;runQuery=(query,pin)=>{status={...status,revision:"+JSON.stringify(newPair)+"};return globalThis.originalRunQuery(query,pin)}");
  const load=descendants(h.get(kind==="view"?"views":"annotations")).find(n=>n.attrs?.['data-action']==='load');
  await load.listeners.click();await new Promise(setImmediate);await new Promise(setImmediate);
  const queries=requests.filter(r=>r.url.startsWith('/api/query?'));
  assert.equal(queries.length,1);
  const url=new URL(queries[0].url,"http://local");assert.equal(url.searchParams.get('indexGeneration'),oldPair.indexGeneration);assert.equal(url.searchParams.get('indexRevision'),'1');
  assert.equal(h.run('result'),null);assert.match(h.get('error').textContent,/Index changed/);
});

for (const kind of ["view","note"]) test(`${kind} edit uses original identity, strips server metadata, and preserves a 409 draft`, async () => {
  const h=harness(), requests=[];
  h.run(`status={revision:${JSON.stringify(oldPair)},workspaceRoot:'/same',evidenceFormat:'native-v6'};seed='wrong-selection';result={revision:status.revision,query:{seed:'wrong-selection'},calls:[],nodes:[]}`);
  h.context.saved=canonicalSaved(kind,attached());
  h.run(kind==="view" ? "views=[normalizeSavedState(saved,'view')];renderViews()" : "seed='target-a';annotations=[normalizeSavedState(saved,'note')];renderNotes()");
  h.run("seed='wrong-selection'");
  const listId=kind==="view"?"views":"annotations";
  const editControl=descendants(h.get(listId)).find(n=>n.attrs?.['data-action']==='edit');
  await editControl.listeners.click();
  if(kind==="view") h.get('view-title').value='Edited title';
  else {h.get('note-title').value='Edited note';h.get('note').value='Edited body';}
  h.context.fetch=async(url,opts)=>{
    const parsed=new URL(url,'http://local');requests.push({url,pathname:parsed.pathname,method:opts.method,body:opts.body&&JSON.parse(opts.body)});
    if(opts.method==='PUT') return {ok:false,status:409,json:async()=>({error:{code:'revision_conflict',message:'Index changed'}})};
    if(url==='/api/status') return response({revision:newPair,workspaceRoot:'/same',evidenceFormat:'native-v6',stats:{}});
    if(url.startsWith('/api/tree?')) return response({...treePage('',[]),revision:newPair});
    if(url==='/api/dependencies') return response({state:'disabled',workspaceRevision:newPair,packages:[],warnings:[]});
    throw Error(`Unexpected request ${url}`);
  };
  h.get(kind==="view"?'save-form':'annotation-form').listeners.submit({preventDefault(){}});
  await new Promise(setImmediate);await new Promise(setImmediate);
  const puts=requests.filter(r=>r.method==='PUT');assert.equal(puts.length,1);
  const put=puts[0], parsed=new URL(put.url,'http://local');assert.equal(parsed.searchParams.get('indexGeneration'),oldPair.indexGeneration);assert.equal(parsed.searchParams.get('indexRevision'),'1');
  if(kind==="view"){
    assert.deepEqual(put.body,{id:'view-a',title:'Edited title',query:{seed:'target-a',depth:2,maxNodes:41,maxCalls:201,includeCallbacks:false,excludePaths:['vendor/']},pins:{'target-a':{x:1,y:2}},hidden:['hidden-a']});
    assert.equal(h.run('editingView.id'),'view-a');assert.equal(h.get('view-title').value,'Edited title');
  } else {
    assert.deepEqual(put.body,{id:'note-a',nodeId:'target-a',body:'Edited body',title:'Edited note'});
    assert.equal(h.run('editingNote.id'),'note-a');assert.equal(h.get('note').value,'Edited body');
  }
  assert.equal(JSON.stringify(put.body).includes('anchor'),false);assert.equal(requests.filter(r=>r.method==='PUT').length,1);
});

test("saved list requests share one copied pin and a racing P batch cannot paint after Q", async () => {
  const h=harness(), pendingViews=deferred(), pendingNotes=deferred(), urls=[];
  h.run(`status={revision:${JSON.stringify(oldPair)},workspaceRoot:'/same',evidenceFormat:'native-v6'};views=[normalizeSavedState({id:'old',title:'Old',query:{seed:'target-a'}},'view')]`);
  h.context.fetch=url=>{urls.push(url);return url.startsWith('/api/views?')?pendingViews.promise:pendingNotes.promise;};
  const pending=h.run('loadSaved()');h.run(`status={...status,revision:${JSON.stringify(newPair)}}`);
  pendingViews.resolve(response([canonicalSaved('view',attached())]));pendingNotes.resolve(response([canonicalSaved('note',attached())]));await pending;
  assert.equal(h.run('views[0].view.id'),'old');
  assert.equal(urls.length,2);
  for(const raw of urls){const url=new URL(raw,'http://local');assert.equal(url.searchParams.get('indexGeneration'),oldPair.indexGeneration);assert.equal(url.searchParams.get('indexRevision'),'1');}
});
test("same-workspace conflict refresh keeps persisted views and notes while invalidating Load", async () => {
  const h = harness(), requests = [];
  const savedView = {view:{id:"v",title:"Keep view",query:{seed:"root",depth:1,includeCallbacks:false}},orphanedIds:[],...oldPair,
    attachment:{availability:"ready",result:{status:"attached",targetId:"root",reason:"none"}}};
  const savedNote = {annotation:{id:"n",nodeId:"root",body:"Keep note"},orphaned:false,...oldPair,
    attachment:{availability:"ready",result:{status:"attached",targetId:"root",reason:"none"}}};
  h.run(`status={revision:${JSON.stringify(oldPair)},workspaceRoot:'/same',evidenceFormat:'native-v6'};seed='root';result={revision:status.revision,calls:[],nodes:[]}`);
  h.context.fetch = async url => {
    requests.push(url);
    if (url.startsWith("/api/views?")) return response([savedView]);
    if (url.startsWith("/api/annotations?")) return response([savedNote]);
    if (url === "/api/status") return response({revision:newPair,workspaceRoot:"/same",evidenceFormat:"native-v6",stats:{}});
    if (url === "/api/tree?path=&offset=0&limit=200") return response({...treePage("",[]),revision:newPair});
    if (url === "/api/dependencies") return response({state:"disabled",workspaceRevision:newPair,catalogId:null,packages:[],warnings:[]});
    return {ok:false,status:409,json:async()=>({error:{code:"revision_conflict",message:"Index changed"}})};
  };
  await h.run("loadSaved()");
  assert.equal(descendants(h.get("views")).find(n=>n.attrs?.['data-action']==='load').disabled,false);
  assert.equal(descendants(h.get("annotations")).find(n=>n.attrs?.['data-action']==='load').disabled,false);
  await h.run("perform(() => api('/api/query','POST',{seed:'root'}))");
  await new Promise(setImmediate);
  assert.equal(h.run("status.revision.indexGeneration"),newPair.indexGeneration);
  assert.equal(h.run("result"),null);
  assert.match(text(h.get("views")),/Keep view/);
  assert.equal(h.run("views[0].view.id"),"v");
  assert.equal(h.run("annotations[0].annotation.body"),"Keep note");
  assert.equal(descendants(h.get("views")).find(n=>n.attrs?.['data-action']==='load').disabled,true);
  assert.equal(descendants(h.get("annotations")).find(n=>n.attrs?.['data-action']==='load').disabled,true);
  assert.ok(requests.includes("/api/status"));
});

test("producer and browse boundaries use complete pairs and reject reused revision responses", async () => {
  for (const operation of ["search", "query", "files", "methods", "sequence", "tree"]) {
    const h=harness(), requests=[];
    h.run(`status={revision:${JSON.stringify(oldPair)},workspaceRoot:'/same'};seed='root'`);
    h.context.fetch=async (url,opts) => {
      requests.push({url,body:opts.body && JSON.parse(opts.body)});
      if (url==="/api/status") return response({revision:newPair,workspaceRoot:"/same",stats:{}});
      if (url.startsWith("/api/dependencies")) return response({state:"disabled",workspaceRevision:newPair,packages:[],warnings:[]});
      if (url.startsWith("/api/tree") && requests.some(r=>r.url==="/api/status")) return response({...treePage("",[]),revision:newPair});
      if (url.startsWith("/api/tree")) return response({...treePage("",[]),revision:newPair});
      if (url==="/api/sequence") return response({...view("root"),revision:newPair});
      if (url==="/api/query") return response({revision:newPair,calls:[],nodes:[],seed:"root"});
      if (url==="/api/symbols?q=&limit=80") return response({revision:newPair,items:[]});
      return response({revision:newPair,items:[],nextOffset:null});
    };
    const file={path:"src/a.js",methodCount:1};
    if(operation==="search") await h.run(`perform(() => $('search-form').listeners.submit({preventDefault(){}}))`);
    if(operation==="query") await h.run("perform(() => runQuery())");
    if(operation==="files") await h.run("loadFiles(true)");
    if(operation==="methods") await h.run(`toggleFile(${JSON.stringify(file)})`);
    if(operation==="sequence") {h.run(`selectedMethod=${JSON.stringify(symbol("root"))}`);await h.run("loadSequence()");}
    if(operation==="tree") await h.run(`loadDirectory('',true)`);
    await new Promise(setImmediate);
    const first=requests[0];
    if (["files","methods"].includes(operation)) {
      const query=new URL(first.url,"http://local").searchParams;
      assert.equal(query.get("indexGeneration"),oldPair.indexGeneration,operation);
      assert.equal(query.get("indexRevision"),"1",operation);
    }
    if(operation==="sequence") assert.deepEqual(first.body.expectedRevision,oldPair);
    if (["search","query","tree"].includes(operation)) assert.doesNotMatch(first.url, /indexGeneration|indexRevision/);
    assert.ok(requests.some(r=>r.url==="/api/status"),operation);
    assert.equal(h.run("status.revision.indexGeneration"),newPair.indexGeneration,operation);
    assert.equal(h.run("result"),null,operation);
  }
});


test("unchanged status does not permanently suppress a later unexpected generation", async () => {
  const h=harness(), requests=[];
  h.run(`status={revision:${JSON.stringify(oldPair)},workspaceRoot:'/same'}`);
  h.context.fetch=async url => {
    requests.push(url);
    if(url==='/api/status') return response({revision:requests.filter(p=>p==='/api/status').length===1?oldPair:newPair,workspaceRoot:'/same',stats:{}});
    if(url.startsWith('/api/tree')) return response({...treePage('',[]),revision:newPair});
    if(url==='/api/dependencies') return response({state:'disabled',workspaceRevision:newPair,packages:[],warnings:[]});
    return response({revision:newPair,items:[],nextOffset:null});
  };
  await h.run('loadFiles(true)'); await new Promise(setImmediate);
  assert.equal(requests.filter(p=>p==='/api/status').length,1);
  await h.run('loadFiles(true)'); await new Promise(setImmediate);
  assert.equal(requests.filter(p=>p==='/api/status').length,2);
  assert.equal(h.run('status.revision.indexGeneration'),newPair.indexGeneration);
});

test("tree mismatch discovered during status reconciliation gets one bounded follow-up", async () => {
  const h=harness(), statusRead=deferred(), requests=[];
  const third={...newPair,indexGeneration:'abcdef12-1234-4123-8123-123456789abc'};
  h.run(`status={revision:${JSON.stringify(oldPair)},workspaceRoot:'/same'}`);
  h.context.fetch=async url => {
    requests.push(url);
    if(url==='/api/status') return requests.filter(p=>p==='/api/status').length===1?statusRead.promise:response({revision:third,workspaceRoot:'/same',stats:{}});
    if(url.startsWith('/api/tree')) return response({...treePage('',[]),revision:third});
    if(url==='/api/dependencies') return response({state:'disabled',workspaceRevision:third,packages:[],warnings:[]});
    return response({revision:newPair,items:[],nextOffset:null});
  };
  const first=h.run('loadFiles(true)');
  statusRead.resolve(response({revision:newPair,workspaceRoot:'/same',stats:{}}));
  await first; await new Promise(setImmediate); await new Promise(setImmediate);
  assert.equal(requests.filter(p=>p==='/api/status').length,2);
  assert.equal(h.run('status.revision.indexGeneration'),third.indexGeneration);
});

test("late source generation mismatch clears every derived panel before status resolves", async () => {
  const h=harness(), source=deferred(), statusRead=deferred(), requests=[];
  h.run(`status={revision:${JSON.stringify(oldPair)},workspaceRoot:'/same'}; seed='root';
    result={revision:status.revision,calls:[],nodes:[]}; packet={packetId:'p',revision:status.revision};
    focused={revision:status.revision,calls:[],nodes:[]}; selectedMethod={id:'m'};`);
  h.get('source').scrollIntoView=()=>{};
  h.context.fetch=url=>{requests.push(url);return url==='/api/status'?statusRead.promise:source.promise;};
  const pending=h.run(`perform(() => showSource({path:'x.rs',range:{startLine:1,endLine:1}},status.revision))`);
  source.resolve(response({revision:newPair,file:{path:'x.rs',text:'old'}}));
  await pending;
  assert.ok(requests.includes('/api/status'));
  for (const value of ['result','packet','focused','selectedMethod','seed']) assert.equal(h.run(value),null,value);
  assert.equal(h.get('source').children.length,0);
  statusRead.resolve(response({revision:oldPair,workspaceRoot:'/same',stats:{}}));
  await new Promise(setImmediate);
  assert.equal(h.run('result'),null);
});


test("branch expansion rejects a new generation before rendering the branch", async () => {
  const h=harness(), requests=[];
  h.run(`status={revision:${JSON.stringify(oldPair)},workspaceRoot:'/same'}`);
  h.context.fetch=async url=>{
    requests.push(url);
    if(url==='/api/status') return response({revision:newPair,workspaceRoot:'/same',stats:{}});
    if(url==='/api/dependencies') return response({state:'disabled',workspaceRevision:newPair,packages:[],warnings:[]});
    return response({...treePage(url.includes('path=src')?'src':'',url.includes('path=src')?[treeFile('src/new.rs')]:[folder('src')]),revision:newPair});
  };
  await h.run("toggleDirectory('src')"); await new Promise(setImmediate);
  assert.ok(requests.some(url=>url.startsWith('/api/tree?path=src&offset=0&limit=200')));
  assert.ok(requests.includes('/api/status'));
  assert.doesNotMatch(text(h.get('file-tree')),/new.rs/);
});

test("later tree page cannot mix equal numeric revisions from distinct generations", async () => {
  const h=harness(), requests=[];
  h.run(`status={revision:${JSON.stringify(oldPair)},workspaceRoot:'/same'}`);
  h.context.fetch=async url=>{
    requests.push(url);
    if(url==='/api/status') return response({revision:newPair,workspaceRoot:'/same',stats:{}});
    if(url==='/api/dependencies') return response({state:'disabled',workspaceRevision:newPair,packages:[],warnings:[]});
    if(url.includes('offset=1&')) return response({...treePage('',[treeFile('new.rs')]),revision:newPair});
    return response({...treePage('',[treeFile('old.rs')],1),revision:requests.includes('/api/status')?newPair:oldPair});
  };
  await h.run('loadTreeRoot()');
  await h.run("loadDirectory('')"); await new Promise(setImmediate);
  assert.ok(requests.some(url=>url.includes('offset=1&')));
  assert.ok(requests.includes('/api/status'));
  assert.doesNotMatch(text(h.get('file-tree')),/new.rs/);
});


test("same-workspace automatic pair refresh retains list/save/delete payloads for views and notes", async () => {
 const h=harness(), calls=[];
 let viewsData=[], notesData=[];
 h.run(`status={revision:${JSON.stringify(oldPair)},workspaceRoot:'/same',evidenceFormat:'native-v6'}; seed='root'; result={revision:status.revision,calls:[],nodes:[],query:{seed:'root',depth:0,includeCallbacks:false}}`);
 h.context.crypto={randomUUID:(()=>{let n=0;return ()=>`item-${++n}`;})()};
 h.context.fetch=async(url,opts)=>{
   const parsed=new URL(url,'http://local');
   calls.push({url,pathname:parsed.pathname,params:parsed.searchParams,method:opts.method,body:opts.body && JSON.parse(opts.body)});
   if(url==='/api/status') return response({revision:newPair,workspaceRoot:'/same',evidenceFormat:'native-v6',stats:{}});
   if(url.startsWith('/api/tree?')) return response({...treePage('',[]),revision:newPair});
   if(url==='/api/dependencies') return response({state:'disabled',workspaceRevision:newPair,packages:[],warnings:[]});
   if(parsed.pathname==='/api/views' && opts.method==='GET') return response(viewsData);
   if(parsed.pathname==='/api/annotations' && opts.method==='GET') return response(notesData);
   if(parsed.pathname==='/api/views/item-1' && opts.method==='PUT') {viewsData=[{view:JSON.parse(opts.body)}];return response({});}
   if(parsed.pathname==='/api/annotations/item-2' && opts.method==='PUT') {notesData=[{annotation:JSON.parse(opts.body)}];return response({});}
   if(parsed.pathname==='/api/views/item-1' && opts.method==='DELETE') {viewsData=[];return response(null);}
   if(parsed.pathname==='/api/annotations/item-2' && opts.method==='DELETE') {notesData=[];return response(null);}
   return response({revision:newPair,items:[],nextOffset:null});
 };
 await h.run('loadFiles(true)'); await new Promise(setImmediate);
 assert.equal(h.run('status.revision.indexGeneration'),newPair.indexGeneration);
 await h.run('loadSaved()');
 const listCalls=calls.filter(c=>['/api/views','/api/annotations'].includes(c.pathname)&&c.method==='GET');
 assert.equal(listCalls.length,2);
 for(const call of listCalls){assert.equal(call.params.get('indexGeneration'),newPair.indexGeneration);assert.equal(call.params.get('indexRevision'),'1');}
 h.run(`result={revision:status.revision,calls:[],nodes:[],query:{seed:'root',depth:1}}; seed='root'; renderResult()`);
 h.get('view-title').value='Keep';h.get('save-form').listeners.submit({preventDefault(){}}); await new Promise(setImmediate); await new Promise(setImmediate);
 h.get('note').value='Remember';h.get('annotation-form').listeners.submit({preventDefault(){}});await new Promise(setImmediate);await new Promise(setImmediate);
 const viewPut=calls.find(c=>c.pathname==='/api/views/item-1'&&c.method==='PUT');
 const notePut=calls.find(c=>c.pathname==='/api/annotations/item-2'&&c.method==='PUT');
 for(const call of [viewPut,notePut]){assert.equal(call.params.get('indexGeneration'),newPair.indexGeneration);assert.equal(call.params.get('indexRevision'),'1');}
 assert.deepEqual(viewPut.body,{id:'item-1',title:'Keep',query:{seed:'root',depth:1,maxNodes:40,maxCalls:200,includeCallbacks:false,excludePaths:[]},pins:{},hidden:[]});
 assert.deepEqual(notePut.body,{id:'item-2',nodeId:'root',body:'Remember'});
 const viewDelete=descendants(h.get('views')).find(n=>n.tagName==='button'&&n.textContent==='Delete');
 const noteDelete=descendants(h.get('annotations')).find(n=>n.tagName==='button'&&n.textContent==='Delete');
 viewDelete.listeners.click();noteDelete.listeners.click();await new Promise(setImmediate);await new Promise(setImmediate);
 assert.equal(calls.find(c=>c.pathname==='/api/views/item-1'&&c.method==='DELETE').body,undefined);
 assert.equal(calls.find(c=>c.pathname==='/api/annotations/item-2'&&c.method==='DELETE').body,undefined);
 assert.equal(h.run('views.length + annotations.length'),0);
});


test("request handlers reject bare numeric pins without sending a request", async () => {
 const h=harness(), requests=[];h.context.fetch=async url=>{requests.push(url);throw Error('invalid request was sent');};
 await assert.rejects(h.run("showSource({path:'x',range:{startLine:1,endLine:1}},1)"),/snapshot pair|revision/);
 assert.deepEqual(requests,[]);
});


test("old target IDs never enable follow-up graph queries", async () => {
  const h = harness(), requests = [];
  const target = {...symbol("target"), kind:"method"};
  const root = {...symbol("root"), kind:"method"};
  const call = {id:"entry",caller:"root",target:"target",candidateSymbols:["target"],calleeText:"target",path:"src/a.js",range:{startLine:2,endLine:2},resolution:"internal"};
  h.run(`status={revision:${JSON.stringify(oldPair)},workspaceRoot:'/same'};seed='root';`);
  h.context.result = {revision:oldPair,nodes:[root,target],calls:[call]};
  h.context.fetch = async url => { requests.push(url); throw Error("Unexpected request " + url); };
  h.run("result = globalThis.result; renderResult()");
  assert.equal(descendants(h.get("calls")).some(n => n.textContent === "Expand outgoing calls"), false);
  assert.equal(descendants(h.get("calls")).some(n => n.textContent === "Read call-site source"), true);
  assert.equal(descendants(h.get("nodes")).some(n => n.textContent === "target · method"), false);
  assert.equal(requests.length, 0);
});

test("failed status reconciliation allows the next unexpected pair to retry", async () => {
  for (const failure of ["network","http"]) {
    const h=harness(), requests=[];
    h.run(`status={revision:${JSON.stringify(oldPair)},workspaceRoot:'/same'};seed='root';
      result={revision:status.revision,calls:[],nodes:[]};packet={packetId:'old',revision:status.revision};
      focused={revision:status.revision,calls:[],nodes:[]};selectedMethod={id:'old'};`);
    h.context.fetch=async (url, options) => {
      requests.push({url,method:options.method});
      if(url==='/api/status') {
        if(requests.filter(r=>r.url==='/api/status').length===1) {
          if(failure==='network') throw Error('offline');
          return {ok:false,status:500,json:async()=>({error:{message:'unavailable'}})};
        }
        return response({revision:newPair,workspaceRoot:'/same',stats:{}});
      }
      if(url.startsWith('/api/tree')) return response({...treePage('',[]),revision:newPair});
      if(url==='/api/dependencies') return response({state:'disabled',workspaceRevision:newPair,packages:[],warnings:[]});
      if(url.startsWith('/api/files')) return response({revision:newPair,items:[],nextOffset:null});
      throw Error(`Unexpected request ${url}`);
    };
    await h.run('loadFiles(true)'); await new Promise(setImmediate);
    assert.equal(requests.filter(r=>r.url==='/api/status').length,1,failure);
    assert.equal(h.run('status.revision.indexGeneration'),oldPair.indexGeneration);
    await h.run('loadFiles(true)'); await new Promise(setImmediate); await new Promise(setImmediate);
    assert.equal(requests.filter(r=>r.url==='/api/status').length,2,failure);
    assert.equal(h.run('status.revision.indexGeneration'),newPair.indexGeneration,failure);
    for(const value of ['result','packet','focused','selectedMethod','seed']) assert.equal(h.run(value),null,`${failure}: ${value}`);
    assert.ok(requests.length<=6,`${failure}: bounded requests`);
    assert.ok(requests.every(r=>! /questions|answer|provider/.test(r.url)));
    assert.equal(requests.filter(r=>r.url.startsWith('/api/files')).length,2);
    assert.ok(requests.filter(r=>r.url.startsWith('/api/files')).every(r=>r.method==='GET'));
  }
});

test("new terminal DTO keeps measured call source without candidate traversal", () => {
  const h=harness(), root={...symbol("root"),kind:"method"};
  h.context.result={revision:oldPair,nodes:[root],calls:[{id:"terminal",caller:"root",calleeText:"open",path:"src/a.js",range:{startLine:4,endLine:4}}],regions:[]};
  h.run(`status={revision:${JSON.stringify(oldPair)}};seed='root';result=globalThis.result;renderResult()`);
  const rows=descendants(h.get("calls"));
  assert.ok(rows.some(item=>item.textContent==="Read call-site source"));
  assert.ok(rows.some(item=>item.textContent==="Syntax only · target unverified"));
  assert.equal(rows.some(item=>item.textContent==="Expand outgoing calls"),false);
});

test("saved callback setting never reaches an old query endpoint", async () => {
  const h=harness(), requests=[];
  h.run(`status={revision:${JSON.stringify(oldPair)}};seed='root';`);
  h.context.fetch=async(url,opts)=>{requests.push({url,body:opts.body&&JSON.parse(opts.body)});return response({revision:oldPair,nodes:[],calls:[]});};
  await h.run("runQuery({seed:'root',depth:5,includeCallbacks:true,maxNodes:40,maxCalls:200,excludePaths:[]})");
  assert.equal(requests[0].url,"/api/query");
  assert.equal(requests[0].body.depth,1);
  assert.equal(requests[0].body.includeCallbacks,false);
});

test("missing or malformed call-site range stays non-actionable on an old DTO",()=>{
  const h=harness(), root={...symbol("root"),kind:"method"};
  h.context.result={revision:oldPair,nodes:[root],calls:[
    {id:"missing",caller:"root",target:"guessed",calleeText:"open",path:"src/a.js"},
    {id:"invalid",caller:"root",target:"guessed",calleeText:"open",path:"src/a.js",range:{startLine:7,endLine:2}}
  ],regions:[]};
  h.run(`status={revision:${JSON.stringify(oldPair)}};seed='root';result=globalThis.result;renderResult()`);
  const rows=descendants(h.get("calls"));
  assert.equal(rows.filter(item=>item.textContent==="Read call-site source").length,0);
  assert.equal(rows.filter(item=>item.textContent==="Expand outgoing calls").length,0);
  assert.equal(rows.filter(item=>item.textContent==="Call site · source unavailable").length,2);
});

test("root and call source actions reject missing, empty, inverted and noninteger witnessed ranges",()=>{
 const h=harness(), root={...symbol("root"),kind:"method",path:"src/a.js",range:{startLine:3,endLine:2}};
 h.context.result={revision:oldPair,nodes:[root],calls:[
   {id:"empty",caller:"root",path:"",range:{startLine:1,endLine:2}},
   {id:"float",caller:"root",path:"src/a.js",range:{startLine:1.5,endLine:2}},
   {id:"byte",caller:"root",path:"src/a.js",range:{startLine:1,endLine:2,startByte:8,endByte:3}},
   {id:"good",caller:"root",path:"src/a.js",range:{startLine:1,endLine:2},calleeText:"measured"}],regions:[]};
 h.run(`status={revision:${JSON.stringify(oldPair)}};seed='root';result=globalThis.result;renderResult()`);
 const rows=descendants(h.get("calls"));
 assert.equal(rows.filter(item=>item.textContent==="Read root source").length,0);
 assert.equal(rows.filter(item=>item.textContent==="Read call-site source").length,1);
 assert.equal(rows.filter(item=>item.textContent==="Expand outgoing calls").length,0);
});
