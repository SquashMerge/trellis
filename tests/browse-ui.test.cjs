"use strict";
// Synthetic data only. Run: node --test tests/browse-ui.test.cjs
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const source = ["sequence.js", "shell.js", "files.js", "app.js"].map(file => fs.readFileSync(path.join(__dirname, "../web", file), "utf8")).join("\n");

function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return {promise, resolve, reject};
}
const ROOT = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SCOPED = `/api/checkouts/${ROOT}`;
function harness(windowOptions = {}, withMetrics = false) {
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
  const context = vm.createContext({console, TextEncoder, DOMException, Blob, AbortController, URLSearchParams,
    setTimeout(callback, delay) { const id = ++nextTimer; timers.set(id, {callback, delay}); return id; }, clearTimeout(id) { timers.delete(id); },
    window: {confirm: () => true, ...windowOptions},
    URL: {createObjectURL(blob) { blobs.push(blob); return "blob:synthetic"; }, revokeObjectURL() {}},
    document: {addEventListener() {}, getElementById: get, createElement: node, createElementNS: (ns, tag) => node(tag), createDocumentFragment: node, createTextNode: text => ({textContent:text,children:[]}), body: node()},
    fetch() { throw new Error("Unexpected request"); }});
  const run = code => vm.runInContext(code, context);
  if (withMetrics) {
    run(fs.readFileSync(path.join(__dirname, "../web/metric-heat.js"), "utf8"));
    run(fs.readFileSync(path.join(__dirname, "../web/viewer-metrics.js"), "utf8"));
  }
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


// Minimal browser history: traversal dispatch is explicit so races are deterministic.
function historyWindow(hash = "") {
  const location = {hash}, entries = [{state:null, hash}], events = {};
  let position = 0;
  const history = {
    get state() { return entries[position].state; },
    get length() { return entries.length; },
    replaceState(state, _, next) { entries[position] = {state, hash:next}; location.hash = next; },
    pushState(state, _, next) { entries.splice(++position); entries.push({state, hash:next}); location.hash = next; },
    go(delta) { position += delta; location.hash = entries[position].hash; },
  };
  return {location, history, events, addEventListener(type, fn) { events[type] = fn; }};
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
  const h=harness(); h.context.fetch=async()=>response(treePage('',[treeFile('notes.env'),{...treeFile('link'),kind:'symlink'}]));
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
  h.run('refreshStatus=async()=>{}');
  h.get('connect-form').listeners.submit({preventDefault(){}});
  await new Promise(resolve=>setImmediate(resolve));
  h.get('checkout-select').value=ROOT; await h.get('checkout-select').listeners.change({target:h.get('checkout-select')});
  assert.equal(h.get('error').hidden,true); assert.equal(h.get('error').textContent,'');
  assert.equal(h.get('workspace').hidden,false);
});

test("file browser keeps filtering visible, display options collapsed and mobile tree space reserved", () => {
  const html = fs.readFileSync(path.join(__dirname, '../web/index.html'),'utf8');
  const css = fs.readFileSync(path.join(__dirname, '../web/style.css'),'utf8');
  assert.match(html, /<label class="file-filter-label" for="file-filter">Filter loaded entries<\/label><input id="file-filter"/);
  assert.match(html, /<details class="browse-options"><summary>Display options<\/summary>/);
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
  h.run(`status = {revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1}, workspaceRoot:'/old'};
    selectedMethod={id:'old'}; sourceCache.set('old','cached');
    $('source').append(element('span','old source'));`);
  h.context.fetch = async path => {
    requests.push(path);
    return response(path === '/api/status' ? {revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},workspaceRoot:'/new'}
      : {...treePage('',[treeFile('new.rs')]),root:'/new',indexedWorkspace:'/new'});
  };
  await h.run('refreshStatus()');
  for (const name of ['selectedMethod']) assert.equal(h.run(name), null, name);
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
    selectedMethod={id:'old'}; sourceCache.set(IndexPin.key(status.revision)+':old.js',{file:{text:'old'}});`);
  h.context.fetch = async url => url === '/api/status' ? response({revision:{indexGeneration:'87654321-4321-4321-8321-abcdef123456',indexRevision:1},workspaceRoot:'/same',stats:{}}) : response({...treePage('',[]),revision:{indexGeneration:'87654321-4321-4321-8321-abcdef123456',indexRevision:1}});
  await h.run('refreshStatus()');
  assert.equal(h.run('selectedMethod'),null);
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
    if (url.startsWith("/api/files") || url.startsWith("/api/methods")) return {ok:false,status:409,json:async()=>({error:{code:"storage_busy",message:"Storage is busy"}})};
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
  await h.run("perform(() => api('/api/files'))");
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

test("producer and browse boundaries use complete pairs and reject reused revision responses", async () => {
  for (const operation of ["files", "methods", "sequence", "tree"]) {
    const h=harness(), requests=[];
    h.run(`status={revision:${JSON.stringify(oldPair)},workspaceRoot:'/same'}`);
    h.context.fetch=async (url,opts) => {
      requests.push({url,body:opts.body && JSON.parse(opts.body)});
      if (url==="/api/status") return response({revision:newPair,workspaceRoot:"/same",stats:{}});
      if (url.startsWith("/api/dependencies")) return response({state:"disabled",workspaceRevision:newPair,packages:[],warnings:[]});
      if (url.startsWith("/api/tree") && requests.some(r=>r.url==="/api/status")) return response({...treePage("",[]),revision:newPair});
      if (url.startsWith("/api/tree")) return response({...treePage("",[]),revision:newPair});
      if (url==="/api/sequence") return response({...view("root"),revision:newPair});
      return response({revision:newPair,items:[],nextOffset:null});
    };
    const file={path:"src/a.js",methodCount:1};
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
    if (["tree"].includes(operation)) assert.doesNotMatch(first.url, /indexGeneration|indexRevision/);
    assert.ok(requests.some(r=>r.url==="/api/status"),operation);
    assert.equal(h.run("status.revision.indexGeneration"),newPair.indexGeneration,operation);
    assert.equal(h.run("selectedMethod"),null,operation);
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
  h.run(`status={revision:${JSON.stringify(oldPair)},workspaceRoot:'/same'}; selectedMethod={id:'m'};`);
  h.get('source').scrollIntoView=()=>{};
  h.context.fetch=url=>{requests.push(url);return url==='/api/status'?statusRead.promise:source.promise;};
  const pending=h.run(`perform(() => showSource({path:'x.rs',range:{startLine:1,endLine:1}},status.revision))`);
  source.resolve(response({revision:newPair,file:{path:'x.rs',text:'old'}}));
  await pending;
  assert.ok(requests.includes('/api/status'));
  for (const value of ['selectedMethod']) assert.equal(h.run(value),null,value);
  assert.equal(h.get('source').children.length,0);
  statusRead.resolve(response({revision:oldPair,workspaceRoot:'/same',stats:{}}));
  await new Promise(setImmediate);
  assert.equal(h.run('selectedMethod'),null);
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


test("request handlers reject bare numeric pins without sending a request", async () => {
 const h=harness(), requests=[];h.context.fetch=async url=>{requests.push(url);throw Error('invalid request was sent');};
 await assert.rejects(h.run("showSource({path:'x',range:{startLine:1,endLine:1}},1)"),/snapshot pair|revision/);
 assert.deepEqual(requests,[]);
});


test("failed status reconciliation allows the next unexpected pair to retry", async () => {
  for (const failure of ["network","http"]) {
    const h=harness(), requests=[];
    h.run(`status={revision:${JSON.stringify(oldPair)},workspaceRoot:'/same'};selectedMethod={id:'old'};`);
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
    for(const value of ['selectedMethod']) assert.equal(h.run(value),null,`${failure}: ${value}`);
    assert.ok(requests.length<=6,`${failure}: bounded requests`);
    assert.ok(requests.every(r=>! /questions|answer|provider/.test(r.url)));
    assert.equal(requests.filter(r=>r.url.startsWith('/api/files')).length,2);
    assert.ok(requests.filter(r=>r.url.startsWith('/api/files')).every(r=>r.method==='GET'));
  }
});

test("shared source rejects missing, empty, inverted and noninteger witnessed ranges",async()=>{
 const h=harness(), requests=[];
 h.context.fetch=async url=>{requests.push(url);return response({revision:oldPair,file:{path:"src/a.js",text:"measured"}});};
 for(const item of [
   {path:"",range:{startLine:1,endLine:2}},
   {path:"src/a.js",range:{startLine:3,endLine:2}},
   {path:"src/a.js",range:{startLine:1.5,endLine:2}},
   {path:"src/a.js",range:{startLine:1,endLine:2,startByte:8,endByte:3}},
   {path:"src/a.js"}
 ]) {
   h.context.item=item;
   await assert.rejects(h.run("showSource(item,status.revision)"),/Measured source path and range unavailable/);
 }
 assert.equal(requests.length,0);
});

const metricPin = {indexGeneration:"12345678-1234-4123-8123-123456789abc",indexRevision:1};
const metricFile = {path:"src/a.js",sourceSetId:"main",language:"javascript",declarationCount:1,roleCounts:{production:1,test:0},metrics:{cognitive:7}};
const metricsResponse = (scope, {files = [metricFile], directories = [], pin = metricPin} = {}) => response({payload:{...pin,workbench:true,metricsVersion:"metrics:v1",scope,aggregate:"max",role:"production",files,directories,topCognitive:[]},catchingUp:false});

test("selecting a sequence step fences an already pending file metrics inspection", async () => {
  const h = harness({}, true), pending = deferred();
  const step = {id:"selected-call",kind:"call",label:"Selected call",path:"src/a.js",range:{startLine:2,endLine:2},target:"unknown",children:[],alternate:[]};
  const data = {...view("root"),steps:[step]};
  h.context.fetch = async request => {
    if (request === "/api/sequence") return response(data);
    if (request.startsWith("/api/metrics?")) return new URL(request,"http://test").searchParams.get("scope") === "revision" ? metricsResponse("revision") : pending.promise;
    if (request.startsWith("/api/methods?")) return response({...metricPin,revision:metricPin,items:[],truncated:false});
    throw new Error("Unexpected " + request);
  };
  await h.run(`selectMethod(${JSON.stringify(symbol("root"))})`);
  await h.run("viewerMetrics.refresh()");
  h.run("files=[{path:'src/a.js',methodCount:1}]; renderFiles()");
  const file = descendants(h.get("file-tree")).find(node => node.tagName === "button" && node.title === "src/a.js");
  await file.listeners.click();
  assert.equal(h.get("metric-inspector").hidden, false);
  const pick = descendants(h.get("sequence-diagram")).find(node => node.attrs?.class?.split(" ").includes("sequence-source"));
  pick.listeners.click();
  assert.equal(h.get("inspector-title").textContent, "Selected call");
  assert.equal(h.get("metric-inspector").hidden, true);
  pending.resolve(metricsResponse("file"));
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(h.get("metric-inspector").hidden, true);
  assert.equal(h.get("metric-inspector-values").children.length, 0);
  assert.equal(h.get("inspector-title").textContent, "Selected call");
  assert.equal(h.get("inspector-open-source").disabled, false);
  assert.equal(h.networkUrls.some(url => /\/source\?/.test(url)), false);
});

test("cwd compact directory metrics use the terminal indexed-relative path, never an outside prefix", async () => {
  const h = harness({}, true), inspections = [];
  const rows = ["", "src"].map(path => ({path,sourceSetId:"main",declarationCount:1,metrics:{cognitive:path ? 8 : 2}}));
  h.context.fetch = async request => {
    const query = new URL(request,"http://test").searchParams;
    if (request.startsWith("/api/metrics?")) {
      if (query.get("scope") !== "revision") inspections.push(query.get("path"));
      return metricsResponse(query.get("scope"),{files:[],directories:rows.filter(row => query.get("scope") === "revision" || row.path === query.get("path"))});
    }
    throw new Error("Unexpected " + request);
  };
  h.run(`treeMode=true; treeRoot='/cwd'; indexedWorkspace='/cwd/sample';
    directories.set('',{open:true,items:${JSON.stringify([folder('sample'),folder('sample-old')])},nextOffset:null,truncated:false});
    directories.set('sample',{open:false,items:${JSON.stringify([folder('sample/src')])},nextOffset:null,truncated:false});
    directories.set('sample/src',{open:false,items:[],nextOffset:null,truncated:false});
    directories.set('sample-old',{open:false,items:[],nextOffset:null,truncated:false});`);
  await h.run("viewerMetrics.refresh()");
  const compact = descendants(h.get("file-tree")).find(node => node.title === "sample/src/");
  assert.ok(compact);
  assert.match(text(compact), /8/);
  await compact.listeners.click(); await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(inspections, ["src"]);
  assert.equal(h.get("metric-inspector-title").textContent, "src");
  const outside = descendants(h.get("file-tree")).find(node => node.title === "sample-old/");
  await outside.listeners.click(); await new Promise(resolve => setImmediate(resolve));
  assert.deepEqual(inspections, ["src"]);
  assert.equal(h.get("metric-inspector").hidden, true);
  assert.equal(h.run("metricDirectorySelection('sample').path"), "");
  assert.equal(h.run("metricDirectorySelection('')"), null);
  assert.equal(h.run("metricDirectorySelection('sample-old')"), null);
});

for (const transition of ["root", "session", "generation"]) test(`${transition} change clears metrics projection, selection and pending inspection`, async () => {
  const h = harness({}, true), pending = deferred();
  const changedPin = {...metricPin,indexGeneration:"87654321-4321-4321-8321-cba987654321"};
  h.context.fetch = async request => {
    if (request.startsWith("/api/metrics?")) {
      const query = new URL(request,"http://test").searchParams;
      return query.get("scope") !== "revision" ? pending.promise : query.get("indexGeneration") === changedPin.indexGeneration
        ? metricsResponse("revision",{pin:changedPin,files:[]}) : metricsResponse("revision");
    }
    if (request === "/api/status") return response({workspaceRoot:"/synthetic",revision:changedPin,stats:{}});
    if (request.startsWith("/api/tree?")) return response({...treePage("",[]),root:"/synthetic",indexedWorkspace:"/synthetic",revision:changedPin});
    if (request === "/api/dependencies") return response({state:"disabled",workspaceRevision:changedPin,catalogId:null,packages:[],warnings:[]});
    throw new Error("Unexpected " + request);
  };
  await h.run("viewerMetrics.refresh()");
  h.run("files=[{path:'src/a.js',methodCount:1}]; renderFiles()");
  assert.match(text(h.get("metric-legend")), /Displayed/);
  const inspection = h.run("viewerMetrics.inspect({scope:'file',path:'src/a.js'})");
  assert.equal(h.get("metric-inspector").hidden, false);
  if (transition === "root") h.run("resetCheckout()");
  if (transition === "session") h.get("logout").listeners.click();
  if (transition === "generation") await h.run("refreshStatus()");
  pending.resolve(metricsResponse("file")); await inspection;
  assert.equal(h.get("metric-inspector").hidden, true);
  assert.equal(h.get("metric-inspector-values").children.length, 0);
  assert.equal(h.get("metric-inspector-top").children.length, 0);
  assert.doesNotMatch(text(h.get("metric-legend")), /Displayed/);
  assert.equal(h.run("viewerMetrics.decorate(document.createElement('button'),{scope:'file',path:'src/a.js'}).value"), undefined);
});

test("a listed generation-only snapshot change clears derived UI before awaiting status rebind", async () => {
  const h = harness({}, true), statusReply = deferred();
  const nextPin = {...metricPin,indexGeneration:"87654321-4321-4321-8321-cba987654321"};
  h.context.fetch = async request => {
    if (request === "/api/checkouts") return response({checkouts:[{rootKey:ROOT,workspaceRoot:"/synthetic",state:"available",revision:nextPin,catchingUp:false}]});
    if (request === "/api/status") return statusReply.promise;
    if (request.startsWith("/api/metrics?")) {
      const query = new URL(request,"http://test").searchParams;
      return metricsResponse(query.get("scope"),query.get("indexGeneration") === nextPin.indexGeneration ? {pin:nextPin,files:[]} : {});
    }
    if (request.startsWith("/api/tree?")) return response({...treePage("",[]),root:"/synthetic",indexedWorkspace:"/synthetic",revision:nextPin});
    if (request === "/api/dependencies") return response({state:"disabled",workspaceRevision:nextPin,catalogId:null,packages:[],warnings:[]});
    throw new Error("Unexpected " + request);
  };
  h.run("selectedWorkspaceRoot='/synthetic'; files=[{path:'src/a.js',methodCount:1}]");
  await h.run("viewerMetrics.refresh()");
  await h.run("viewerMetrics.inspect({scope:'file',path:'src/a.js'})");
  h.run("selectedMethod={id:'old'}; sourceCache.set('old',{text:'old'}); $('source').append(document.createTextNode('old source')); window.TrellisShell.showSource('workspace')");
  assert.match(text(h.get("metric-legend")),/Displayed/);
  let settled = false;
  const listed = h.run("loadCheckouts()").then(value => { settled = true; return value; });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(h.networkUrls.filter(url => url.endsWith("/status")).length,1);
  assert.equal(settled,false,"list refresh must await the selected checkout's status rebind");
  assert.equal(h.run("selectedMethod"),null);
  assert.equal(h.run("sourceCache.size"),0);
  assert.equal(h.get("source").children.length,0);
  assert.equal(h.get("source-dock").hidden,true);
  assert.equal(h.get("file-tree").children.length,0);
  assert.equal(h.get("metric-inspector").hidden,true);
  assert.equal(h.get("metric-inspector-values").children.length,0);
  assert.doesNotMatch(text(h.get("metric-legend")),/Displayed/);
  statusReply.resolve(response({workspaceRoot:"/synthetic",revision:nextPin,catchingUp:false,stats:{}}));
  assert.equal(await listed,true);
  assert.equal(h.run("selectedRootKey"),ROOT);
  assert.deepEqual(JSON.parse(JSON.stringify(h.run("status.revision"))),nextPin);
  assert.equal(h.get("error").hidden,true);
});


// Retained-pin comparisons observe head without replacing the displayed evidence.
const retainedPin = {indexGeneration:"12345678-1234-4123-8123-123456789abc",indexRevision:1};
const newerHead = {...retainedPin,indexRevision:2};
const rebuiltHead = {indexGeneration:"87654321-4321-4321-8321-cba987654321",indexRevision:0};
const settleHead = () => new Promise(resolve => setImmediate(resolve));
function retainedHarness() {
  const h = harness();
  h.run("status.workspaceRoot='/synthetic'; selectedWorkspaceRoot='/synthetic'; selectedMethod={id:'retained-method'}; sourceCache.set('retained',{text:'retained bytes'}); $('source').append(document.createTextNode('retained bytes'))");
  return h;
}
function assertRetained(h) {
  assert.deepEqual(JSON.parse(JSON.stringify(h.run("status.revision"))),retainedPin);
  assert.equal(h.run("selectedMethod.id"),"retained-method");
  assert.equal(h.get("source").children[0].textContent,"retained bytes");
  assert.equal(h.run("sourceCache.get('retained').text"),"retained bytes");
}
const pinnedRoutes = [
  ["GET","files"], ["GET","methods"], ["GET","source"], ["GET","symbols"],
  ["GET","classes"], ["GET","metrics"], ["GET","architecture"],
  ["POST","sequence"], ["POST","syntax-matches"], ["POST","class-diagram"], ["POST","navigation"],
];
for (const [method, route] of pinnedRoutes) test(`successful retained-pin ${method} ${route} observes head without adopting it`, async () => {
  const h = retainedHarness(), requests = [];
  const returned = ["metrics","architecture"].includes(route) ? {payload:retainedPin} : {revision:retainedPin};
  h.context.fetch = async (request, options) => {
    requests.push({request,method:options.method,body:options.body});
    if (request === "/api/status") return response({workspaceRoot:"/synthetic",revision:newerHead,catchingUp:true});
    if (request.split("?",1)[0] === `/api/${route}`) return response(returned);
    throw new Error("Unexpected " + request);
  };
  const query = new URLSearchParams(retainedPin).toString();
  const result = await h.run(`api('/api/${route}${method === "GET" ? "?" + query : ""}', '${method}', ${method === "POST" ? JSON.stringify({expectedRevision:retainedPin}) : "undefined"})`);
  await settleHead();
  assert.deepEqual(JSON.parse(JSON.stringify(result)),returned);
  assert.equal(requests.length,2);
  assert.equal(requests[0].method,method);
  assert.equal(requests[1].request,"/api/status"); assert.equal(requests[1].method,"GET");
  assertRetained(h);
  assert.equal(h.get("retained-pin-status").hidden,false);
  assert.match(h.get("retained-pin-label").textContent,/Viewing revision 12345678:1 · Older than head 12345678:2/);
  assert.equal(h.get("catching-up").hidden,true,"comparison does not replace checkout freshness");
  assert.equal(h.timers.size,0,"comparison has no polling timer");
});

test("unpaired, mismatched, failed and unrelated reads do not claim a head comparison", async () => {
  const h = retainedHarness(), requests = [];
  h.context.fetch = async request => { requests.push(request); return response({workspaceRoot:"/synthetic",revision:newerHead}); };
  h.run(`observePinnedResponse('/api/files','GET',undefined,{revision:status.revision});
    observePinnedResponse('/api/tree?'+IndexPin.query(status.revision),'GET',undefined,{revision:status.revision});
    observePinnedResponse('/api/files?'+IndexPin.query(status.revision),'GET',undefined,{revision:${JSON.stringify(newerHead)}});
    observePinnedResponse('/api/files?'+IndexPin.query(${JSON.stringify(newerHead)}),'GET',undefined,{revision:${JSON.stringify(newerHead)}});
    observePinnedResponse('/api/sequence','POST',{expectedRevision:${JSON.stringify(newerHead)}},{revision:status.revision});
    observePinnedResponse('/api/sequence','POST',{expectedRevision:status.revision},{revision:${JSON.stringify(newerHead)}});
    observePinnedResponse('/api/metrics?'+IndexPin.query(status.revision),'GET',undefined,{revision:status.revision});`);
  await settleHead();
  assert.deepEqual(requests,[]);
  h.context.fetch = async request => { requests.push(request); return {ok:false,status:409,json:async()=>({error:{code:"revision_conflict",message:"not retained"}})}; };
  await assert.rejects(h.run("api('/api/files?'+IndexPin.query(status.revision))"),/not retained/);
  assert.equal(requests.length,1); assert.match(requests[0],/^\/api\/files\?/);
  assertRetained(h); assert.equal(h.get("retained-pin-status").hidden,true);
});

test("equal head hides the indication, later reads discover newer head and lower observations do not erase it", async () => {
  const h = retainedHarness(), requests = [], heads = [retainedPin,newerHead,retainedPin];
  h.context.fetch = async request => { requests.push(request); return response({workspaceRoot:"/synthetic",revision:heads.shift()}); };
  h.run("observePinnedHead()"); await settleHead();
  assert.equal(h.get("retained-pin-status").hidden,true); assert.equal(h.get("retained-pin-label").textContent,"");
  h.run("observePinnedHead()"); await settleHead();
  assert.match(h.get("retained-pin-label").textContent,/Older than head 12345678:2/);
  h.run("observePinnedHead()"); await settleHead();
  assert.match(h.get("retained-pin-label").textContent,/Older than head 12345678:2/);
  assert.deepEqual(requests,["/api/status","/api/status","/api/status"]);
  assertRetained(h); assert.equal(h.timers.size,0);
});

test("different generation is rebuilt, never numerically ordered or silently adopted", async () => {
  const h = retainedHarness();
  h.context.fetch = async () => response({workspaceRoot:"/synthetic",revision:rebuiltHead});
  h.run("observePinnedHead()"); await settleHead();
  assert.equal(h.get("retained-pin-status").hidden,false);
  assert.match(h.get("retained-pin-label").textContent,/Viewing revision 12345678:1 · Index rebuilt\. Reload at head\./);
  assert.doesNotMatch(h.get("retained-pin-label").textContent,/Older than head/); assertRetained(h);
});

for (const failure of ["network","http","json","workspace","invalid-pin"]) test(`failed ${failure} head comparison preserves evidence and the next read retries`, async () => {
  const h = retainedHarness(), requests = [];
  h.context.fetch = async request => {
    requests.push(request);
    if (requests.length > 1) return response({workspaceRoot:"/synthetic",revision:newerHead});
    if (failure === "network") throw new Error("offline");
    if (failure === "http") return {ok:false,status:503,json:async()=>({error:{code:"storage_busy",message:"busy"}})};
    if (failure === "json") return {ok:true,status:200,json:async()=>{ throw new Error("bad JSON"); }};
    return response({workspaceRoot:failure === "workspace" ? "/other" : "/synthetic",revision:failure === "invalid-pin" ? {indexRevision:2} : newerHead});
  };
  h.run("observePinnedHead()"); await settleHead();
  assert.match(h.get("retained-pin-label").textContent,/Head comparison unavailable/);
  assertRetained(h); assert.equal(h.get("error").hidden,true);
  h.run("observePinnedHead()"); await settleHead();
  assert.deepEqual(requests,["/api/status","/api/status"]);
  assert.match(h.get("retained-pin-label").textContent,/Older than head 12345678:2/); assertRetained(h);
});

for (const transition of ["epoch++",`selectedRootKey='${"b".repeat(64)}'`,"status.workspaceRoot='/other'",`status.revision=${JSON.stringify(newerHead)}`,`status.revision=${JSON.stringify({...rebuiltHead,indexRevision:1})}`]) {
  for (const outcome of ["success","failure"]) test(`late head ${outcome} is inert after ${transition}`, async () => {
    const h = retainedHarness(), pending = deferred();
    h.context.fetch = () => pending.promise;
    h.run("observePinnedHead()"); h.run(transition);
    if (outcome === "success") pending.resolve(response({workspaceRoot:"/synthetic",revision:newerHead}));
    else pending.reject(new Error("obsolete failure"));
    await settleHead();
    assert.equal(h.get("retained-pin-status").hidden,true); assert.equal(h.get("retained-pin-label").textContent,"");
    assert.equal(h.get("source").children[0].textContent,"retained bytes"); assert.equal(h.get("error").hidden,true);
  });
}

test("overlapping reads coalesce to one pending observation and one follow-up", async () => {
  const h = retainedHarness(), first = deferred(), second = deferred(), requests = [];
  h.context.fetch = request => { requests.push(request); return requests.length === 1 ? first.promise : second.promise; };
  h.run("observePinnedHead(); observePinnedHead(); observePinnedHead()");
  assert.deepEqual(requests,["/api/status"]);
  first.resolve(response({workspaceRoot:"/synthetic",revision:retainedPin})); await settleHead();
  assert.deepEqual(requests,["/api/status","/api/status"]); assert.equal(h.get("retained-pin-status").hidden,true);
  h.run("observePinnedHead(); observePinnedHead()");
  second.resolve(response({workspaceRoot:"/synthetic",revision:newerHead})); await settleHead();
  assert.deepEqual(requests,["/api/status","/api/status"]);
  assert.match(h.get("retained-pin-label").textContent,/Older than head 12345678:2/); assertRetained(h);
  h.run("observePinnedHead()"); await settleHead();
  assert.deepEqual(requests,["/api/status","/api/status","/api/status"],"a later idle read starts a fresh comparison");
});

test("an obsolete observation cannot erase a successor's completed comparison", async () => {
  const h = retainedHarness(), old = deferred(); let calls = 0;
  h.context.fetch = () => ++calls === 1 ? old.promise : response({workspaceRoot:"/synthetic",revision:newerHead});
  h.run("observePinnedHead(); epoch++; resetHeadObservation(); observePinnedHead()"); await settleHead();
  assert.match(h.get("retained-pin-label").textContent,/Older than head 12345678:2/);
  old.resolve(response({workspaceRoot:"/synthetic",revision:retainedPin})); await settleHead();
  assert.match(h.get("retained-pin-label").textContent,/Older than head 12345678:2/); assertRetained(h);
});

test("cached source reads compare head without refetching or replacing retained source bytes", async () => {
  const h = retainedHarness(), requests = [];
  h.run(`sourceCache.set(IndexPin.key(status.revision)+':src/a.js',{revision:status.revision,file:{path:'src/a.js',text:'retained cached bytes'}})`);
  h.context.fetch = async request => { requests.push(request); return response({workspaceRoot:"/synthetic",revision:newerHead}); };
  await h.run("showCachedSource('src/a.js',{startLine:1,endLine:1},status.revision)"); await settleHead();
  assert.deepEqual(requests,["/api/status"]);
  assert.match(text(h.get("source")),/retained cached bytes/);
  assert.deepEqual(JSON.parse(JSON.stringify(h.run("workspaceSource.data.revision"))),retainedPin);
  assert.deepEqual(JSON.parse(JSON.stringify(h.run("status.revision"))),retainedPin);
  assert.match(h.get("retained-pin-label").textContent,/Older than head 12345678:2/);
});

test("Reload at head reads actual current status and clears old selection, source and cached identities", async () => {
  const h = retainedHarness(), requests = [], actualHead = {...retainedPin,indexRevision:3};
  h.context.fetch = async request => { requests.push(request); return response({workspaceRoot:"/synthetic",revision:newerHead}); };
  h.run("observePinnedHead()"); await settleHead();
  h.run("section='files'; filesFile={path:'src/a.js'}; filesSymbol={id:'old-symbol'}; filesCatalogLoaded=true");
  h.context.fetch = async request => {
    requests.push(request);
    if (request === "/api/git") return response({git:null});
    if (request === "/api/status") return response({workspaceRoot:"/synthetic",revision:actualHead,stats:{}});
    if (request.startsWith("/api/files?")) return response({revision:actualHead,items:[{path:"src/a.js",methodCount:1}],nextOffset:null});
    if (request === "/api/dependencies") return response({state:"disabled",workspaceRevision:actualHead,catalogId:null,packages:[],warnings:[]});
    throw new Error("Unexpected " + request);
  };
  h.get("reload-head").listeners.click(); await settleHead();
  const routes = requests.map(request => request.split("?",1)[0]);
  assert.equal(routes.filter(route => route === "/api/status").length,3,"initial comparison, explicit refresh and at-head comparison");
  assert.deepEqual(routes.filter(route => route !== "/api/status").sort(),["/api/dependencies","/api/files","/api/git"]);
  assert.equal(requests.some(request => /source|symbols|sequence|index|provider/.test(request.split("?",1)[0])),false,"reload does not reuse old source or symbol identities");
  assert.deepEqual(JSON.parse(JSON.stringify(h.run("status.revision"))),actualHead);
  assert.equal(h.run("selectedMethod"),null); assert.equal(h.run("filesSymbol"),null); assert.equal(h.run("filesFile"),null);
  assert.equal(h.run("sourceCache.size"),0); assert.equal(h.get("source").children.length,0);
  assert.equal(h.get("retained-pin-status").hidden,true); assert.equal(h.get("retained-pin-label").textContent,"");
  assert.equal(h.get("reload-head").disabled,false); assert.equal(h.get("error").hidden,true);
});


test("a failed later comparison keeps an already known older head and retained evidence", async () => {
  const h = retainedHarness(); let calls = 0;
  h.context.fetch = async () => {
    if (++calls === 1) return response({workspaceRoot:"/synthetic",revision:newerHead});
    throw new Error("head unavailable");
  };
  h.run("observePinnedHead()"); await settleHead();
  h.run("observePinnedHead()"); await settleHead();
  assert.match(h.get("retained-pin-label").textContent,/Older than head 12345678:2/);
  assert.equal(h.get("retained-pin-status").hidden,false); assertRetained(h);
});

for (const statusCode of [401,403]) test(`head comparison ${statusCode} preserves authoritative authentication invalidation`, async () => {
  const h = retainedHarness();
  h.context.fetch = async request => {
    assert.equal(request,"/api/status");
    return {ok:false,status:statusCode,json:async()=>({error:{code:"unauthorized",message:"Authentication required"}})};
  };
  h.run("observePinnedHead()"); await settleHead();
  assert.equal(h.run("token"),""); assert.equal(h.run("selectedRootKey"),null); assert.equal(h.run("status"),null);
  assert.equal(h.run("selectedMethod"),null); assert.equal(h.run("sourceCache.size"),0); assert.equal(h.get("source").children.length,0);
  assert.equal(h.get("retained-pin-status").hidden,true); assert.equal(h.get("retained-pin-label").textContent,"");
  assert.equal(h.get("connect-form").hidden,false); assert.equal(h.get("error").hidden,false);
  assert.match(h.get("error").textContent,/Authentication failed/);
});


test("a pinned read queued at comparison completion starts the next head observation", async () => {
  const h = retainedHarness(), requests = [];
  let label = "", notified = false;
  // Queue the next successful-read notification at the presentation boundary,
  // before a detached promise.finally handler would release pending ownership.
  Object.defineProperty(h.get("retained-pin-label"),"textContent",{get:()=>label,set(value) {
    label = value;
    if (!notified) {
      notified = true;
      Promise.resolve().then(() => h.run("observePinnedResponse('/api/files?'+IndexPin.query(status.revision),'GET',undefined,{revision:status.revision})"));
    }
  }});
  h.context.fetch = async request => {
    requests.push(request);
    return response({workspaceRoot:"/synthetic",revision:requests.length === 1 ? retainedPin : newerHead});
  };
  h.run("observePinnedHead()"); await settleHead();
  assert.deepEqual(requests,["/api/status","/api/status"]);
  assert.match(h.get("retained-pin-label").textContent,/Older than head 12345678:2/); assertRetained(h);
});

test("dot entries and indexed-snapshot folders without files stay hidden until Show hidden/ignored", async () => {
  let known = path => path === 'src';
  const metrics = {indexedDirectory:path => known(path), decorate(){}, renderLegend(){}, clearInspection(){}, inspect(){}, refresh(){}, reset(){}};
  const h = harness({TrellisMetrics:{create:() => metrics}});
  h.context.fetch = async () => response({...treePage('', [
    {path:'.git',name:'.git',kind:'directory'}, {path:'build',name:'build',kind:'directory'}, {path:'src',name:'src',kind:'directory'},
    treeFile('.env'), treeFile('README.md')]), indexedWorkspace:'/cwd'});
  await h.run('loadTreeRoot()');
  let rendered = text(h.get('file-tree'));
  assert.match(rendered, /src\//); assert.match(rendered, /README\.md/);
  assert.doesNotMatch(rendered, /\.git|\.env|build/);
  assert.match(h.get('files-state').textContent, /3 hidden or unindexed entries not shown/);
  const toggle = h.get('show-hidden-files');
  toggle.checked = true; toggle.listeners.change({target:toggle});
  rendered = text(h.get('file-tree'));
  assert.match(rendered, /\.git\//); assert.match(rendered, /\.env/); assert.match(rendered, /build\//);
  assert.equal(toggle.checked, true);
  known = () => null;
  toggle.checked = false; toggle.listeners.change({target:toggle});
  rendered = text(h.get('file-tree'));
  assert.match(rendered, /build\//, "folders stay visible while the indexed snapshot is unknown");
  assert.doesNotMatch(rendered, /\.git/);
});


test("sid routes wait for an admitted checkout, resolve only the requested seed, and store no source in browser state", async () => {
  const browser = historyWindow("#sequence?sid=explicit%3Aid"), h = harness(browser), requests = [];
  h.run("selectedRootKey=null; token=''");
  await h.run("readNavigationLocation()"); assert.equal(h.networkUrls.length, 0);
  h.run(`token='synthetic'; selectedRootKey='${ROOT}'`);
  h.context.fetch = async (path, options) => { requests.push([path, options]); return response(view("explicit:id")); };
  await h.run("readNavigationLocation(true)");
  assert.equal(h.run("selectedMethod.id"), "explicit:id");
  assert.deepEqual(requests.map(([path]) => path), ["/api/sequence"]);
  assert.equal(JSON.parse(requests[0][1].body).seed, "explicit:id");
  assert.deepEqual(Object.keys(browser.history.state), ["trellisNavigation"]);
  assert.equal(browser.history.length, 1);
});

for (const hash of ["#sequence", "#sequence?sid=", "#sequence?sid=a&sid=b", `#sequence?sid=a&rootKey=${"b".repeat(64)}`,
  "#sequence?sid=a&indexGeneration=87654321-1234-4123-8123-123456789abc&indexRevision=1"]) {
  test(`invalid or mismatched route refuses requests: ${hash}`, async () => {
    const h = harness(historyWindow(hash));
    await h.run("readNavigationLocation()");
    assert.equal(h.networkUrls.length, 0);
    assert.equal(h.get("error").hidden, false);
    assert.match(h.get("sequence-state").textContent, /Cannot open sequence/);
    assert.equal(h.run("selectedMethod"), null);
  });
}

for (const invalidate of ["clearNavigationHistory()", "epoch++", "selectedRootKey=null", "status.revision={indexGeneration:'87654321-1234-4123-8123-123456789abc',indexRevision:1}"]) {
  test(`late sid response cannot publish after ${invalidate}`, async () => {
    const browser = historyWindow("#sequence?sid=old"), h = harness(browser), old = deferred();
    h.context.fetch = () => old.promise;
    const pending = h.run("readNavigationLocation()");
    await Promise.resolve(); await Promise.resolve();
    h.run(invalidate); old.resolve(response(view("old"))); await pending;
    assert.equal(h.run("selectedMethod"), null);
    assert.equal(h.get("error").hidden, true);
  });
}

test("Back/new hash fences old sid responses without adding traversal entries", async () => {
  const browser = historyWindow("#sequence?sid=old"), h = harness(browser), old = deferred();
  h.context.fetch = () => old.promise;
  const pending = h.run("readNavigationLocation()");
  await Promise.resolve(); await Promise.resolve();
  browser.location.hash = "#sequence?sid=new";
  h.context.fetch = async () => response(view("new"));
  await h.run("readNavigationLocation()");
  old.resolve(response(view("old"))); await pending;
  assert.equal(h.run("selectedMethod.id"), "new");
  assert.equal(browser.history.length, 1);
  await h.run("readNavigationLocation()");
  assert.equal(browser.history.length, 1);
});

test("navigation callbacks bind the explicit source pin and switch Files before sequence selection", async () => {
  let owner;
  const browser = historyWindow(), h = harness({...browser, TrellisNavigation:{init(value){owner=value;}, reset(){}}});
  h.context.fetch = async () => response(view("target"));
  h.run("section='files'; document.body.classList.add('files-section')");
  await owner.selectMethod(symbol("target"), view("target").revision);
  assert.equal(h.run("section"), "diagrams");
  assert.equal(h.run("selectedMethod.id"), "target");
  const query = new URLSearchParams(browser.location.hash.split("?")[1]);
  assert.equal(query.get("sid"), "target"); assert.equal(query.get("rootKey"), ROOT);
  assert.equal(query.get("indexGeneration"), view("target").revision.indexGeneration);
  assert.equal(browser.history.length, 2);
  const requests = h.networkUrls.length;
  await owner.selectMethod(symbol("stale"), {...view("target").revision,indexRevision:2});
  assert.equal(h.networkUrls.length, requests); assert.match(h.get("error").textContent, /stale/);
});


test("captured declaration Inspect source rejects replaced Inspector and changed source scope at the same pin", async () => {
  const h=harness(), callbacks=[];
  h.context.window.TrellisShell.inspectSymbol=(symbol,revision,readSource)=>callbacks.push(readSource);
  h.context.fetch=async()=>response(view("next"));
  h.run(`inspectSourceSymbol(${JSON.stringify(symbol("first"))},status.revision)`);
  h.run(`inspectSourceSymbol(${JSON.stringify(symbol("second"))},status.revision)`);
  await callbacks[0](); assert.equal(h.networkUrls.length,0);
  h.run("sourceSerial++"); await callbacks[1](); assert.equal(h.networkUrls.length,0);
  h.run(`inspectSourceSymbol(${JSON.stringify(symbol("third"))},status.revision)`);
  await h.run(`selectMethod(${JSON.stringify(symbol("next"))})`);
  const count=h.networkUrls.length; await callbacks[2](); assert.equal(h.networkUrls.length,count);
  assert.equal(h.run("selectedMethod.id"),"next");
});
