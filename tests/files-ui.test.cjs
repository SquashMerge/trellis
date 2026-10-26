"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const path = require("node:path");
const ROOT = "a".repeat(64);
const pin = {indexGeneration:"12345678-1234-4123-8123-123456789abc", indexRevision:1};
const source = ["sequence.js", "shell.js", "files.js", "app.js"].map(file => fs.readFileSync(path.join(__dirname, "../web", file), "utf8")).join("\n");
const response = data => ({ok:true,status:200,json:async () => data});
function deferred() { let resolve, reject; const promise = new Promise((yes, no) => { resolve = yes; reject = no; }); return {promise, resolve, reject}; }
const all = node => [node, ...(node.children || []).flatMap(all)];
const text = node => all(node).map(item => item.textContent).join(" ");
function harness() {
  const elements = new Map(), requests = [], timers = new Map(); let timer = 0;
  function node(tagName = "div") {
    const classes = new Set();
    return {tagName, children:[], attrs:{}, listeners:{}, textContent:"", value:"", hidden:false, disabled:false,
      classList:{add(c){classes.add(c);},remove(c){classes.delete(c);},contains(c){return classes.has(c);},toggle(c,force){const on = force ?? !classes.has(c); if(on) classes.add(c); else classes.delete(c); return on;}},
      setAttribute(k,v){this.attrs[k]=String(v);}, getAttribute(k){return this.attrs[k];},
      append(...items){this.children.push(...items);}, replaceChildren(...items){this.children=items;this.textContent="";},
      addEventListener(type,fn){this.listeners[type]=fn;}, querySelector(){return null;}, focus(){this.focused=true;}, scrollIntoView(){}};
  }
  const get = id => { if(!elements.has(id)) elements.set(id,node()); return elements.get(id); };
  const document = {getElementById:get,createElement:node,createElementNS:(_,tag)=>node(tag),createTextNode:value=>({...node("#text"),textContent:value}),createDocumentFragment:()=>node("fragment"),body:node("body"),addEventListener(){}};
  let network = () => { throw new Error("Unexpected request"); };
  const context = vm.createContext({document, window:{}, console, AbortController, DOMException, TextEncoder, URLSearchParams,
    setTimeout(fn){timers.set(++timer,fn);return timer;},clearTimeout(id){timers.delete(id);},
    fetch(url, options){ requests.push({url,options}); assert.ok(url.startsWith(`/api/checkouts/${ROOT}/`)); return network("/api/" + url.slice(`/api/checkouts/${ROOT}/`.length),options); }});
  const run = code => vm.runInContext(code, context); run(source);
  run(`token="synthetic"; selectedRootKey="${ROOT}"; checkouts=[{rootKey:"${ROOT}", workspaceRoot:"/fixture", state:"available"}]; status={revision:${JSON.stringify(pin)}, workspaceRoot:"/fixture"}; selectedGit={state:"unavailable",reason:"not_repository"};`);
  get("workspace").hidden=false;
  return {get, run, context, requests, network(fn){network=fn;}};
}
function symbol(id, kind = "method", file = "main.java") { return {id,name:id,kind,path:file,range:{startByte:0,endByte:18,startLine:1,startColumn:1,endLine:2,endColumn:1}}; }
function outline(id, kind = "method", file) { return {symbol:symbol(id,kind,file), nativeKind:kind, header:{kind,name:id,modifiers:["public"],typeParameters:[],parameters:[],resultType:"int",bases:[]}}; }
const view = item => ({revision:pin, seed:item.symbol,participants:[{id:item.symbol.id,label:item.symbol.name,kind:"method"}],steps:[],hiddenSteps:0,truncated:false,warnings:[]});
const matches = (item, items = [], extra = {}) => ({revision:pin,symbolId:item.symbol.id,evidenceKind:"syntaxMatches",scope:{sourceSetId:"production",language:"java",spelling:item.symbol.name},items,continuation:null,partial:false,truncated:false,warnings:[],...extra});
function indexedNetwork(h, item = outline("measured"), extra = () => undefined) {
  h.network((url, options) => {
    const supplied = extra(url, options); if (supplied) return supplied;
    if(url === "/api/status") return response({revision:pin,workspaceRoot:"/fixture"});
    if(url.startsWith("/api/files?")) return response({revision:pin,items:[{path:item.symbol.path,language:"java",methodCount:1}],nextOffset:null});
    if(url === "/api/git") return response({git:{state:"unavailable",reason:"not_repository"},changes:null});
    if(url.startsWith("/api/symbols?")) return response({revision:pin,path:item.symbol.path,items:[item],nextOffset:null,truncated:false,warnings:[]});
    if(url.startsWith("/api/source?")) return response({revision:pin,file:{path:item.symbol.path,language:"java",text:"int measured() {}\n// <img src=x onerror=boom>\n",hash:"cached"}});
    if(url === "/api/syntax-matches") return response(matches(item));
    if(url === "/api/sequence") return response(view(item));
    throw new Error("Unexpected request " + url);
  });
}

test("Files opens pinned catalog without choosing source; explicit file reads zero-callable source and all kinds", async () => {
  const h=harness(), item=outline("Holder","class"); indexedNetwork(h,item);
  await h.run('setSection("files")');
  assert.ok(h.run('document.body.classList.contains("files-section")'));
  assert.ok(h.requests.every(request=>!request.url.includes("/source?")&&!request.url.endsWith("/sequence")));
  assert.match(h.requests[0].url,/indexGeneration=.*indexRevision=1/);
  h.run('files[0].methodCount=0'); await h.run('selectFilesFile(files[0])');
  assert.match(text(h.get("source")),/int measured/); assert.match(text(h.get("file-tree")),/class · Holder/);
  assert.equal(h.get("files-working").hidden,true); assert.match(h.get("files-git-state").textContent,/not_repository/);
  h.context.item=item; await h.run('selectFilesSymbol(item)');
  assert.match(h.get("files-mini-state").textContent,/noncallable/);
  assert.equal(h.requests.filter(request=>request.url.endsWith("/sequence")).length,0);
  assert.ok(all(h.get("source")).every(node=>node.tagName!=="img"));
});

test("callable mini uses measured renderer; Open full preserves exact DTO seed/pin without another request", async () => {
  const h=harness(), item=outline("measured"); indexedNetwork(h,item);
  await h.run('setSection("files")'); await h.run('selectFilesFile(files[0])'); h.context.item=item;
  await h.run('selectFilesSymbol(item)');
  assert.ok(all(h.get("files-mini-sequence")).some(node=>node.tagName==="svg"));
  assert.equal(h.get("files-open-full").disabled,false);
  const post=h.requests.find(request=>request.url.endsWith("/sequence"));
  assert.deepEqual(JSON.parse(post.options.body),{seed:"measured",expectedRevision:pin,showAll:false});
  assert.equal(all(h.get("source")).filter(node=>node.classList?.contains("highlight")).length,1);
  await h.run('openFilesFull()');
  assert.equal(h.run("section"),"diagrams"); assert.equal(h.run("selectedMethod.id"),"measured");
  assert.match(h.get("sequence-state").textContent,/revision 12345678:1/);
  assert.equal(h.requests.filter(request=>request.url.endsWith("/sequence")).length,1);
});

test("syntax paging echoes continuation, safely displays snippet and exact pinned location resets old inspector", async () => {
  const h=harness(), item=outline("same"), continuation={revision:pin,symbolId:"same",path:"other.java",offset:8,partial:true};
  const location={path:"other.java",sourceHash:"cached-other",range:{startByte:7,endByte:11,startLine:2,startColumn:2,endLine:2,endColumn:6},snippet:"é <script>same</script>"};
  indexedNetwork(h,item,(url,options)=>{
    if(url==="/api/syntax-matches") return response(matches(item,[location],JSON.parse(options.body).continuation ? {} : {continuation,partial:true,truncated:true}));
    if(url.startsWith("/api/source?path=other.java")) return response({revision:pin,file:{path:"other.java",language:"java",text:"é\n same();",hash:"cached-other"}});
  });
  await h.run('setSection("files")'); await h.run('selectFilesFile(files[0])'); h.context.item=item; await h.run('selectFilesSymbol(item)');
  assert.match(text(h.get("files-matches")),/é <script>same<\/script>/); assert.match(h.get("files-matches-state").textContent,/Incomplete coverage/);
  assert.equal(h.get("files-matches-more").hidden,false); await h.get("files-matches-more").listeners.click();
  assert.deepEqual(JSON.parse(h.requests.filter(r=>r.url.endsWith("/syntax-matches")).at(-1).options.body).continuation,continuation);
  const row=all(h.get("files-matches")).find(node=>node.className==="files-match");
  assert.equal(row.attrs["data-start-byte"],"7"); await row.listeners.click();
  assert.equal(h.run("filesSymbol"),null); assert.equal(h.run("filesBasis"),"indexed");
  assert.equal(h.get("files-symbol-title").textContent,"Pinned source location"); assert.equal(h.get("files-mini-sequence").children.length,0);
  assert.match(h.get("source-path").textContent,/other.java/); assert.ok(all(h.get("files-matches")).every(node=>node.tagName!=="script"));
});

for (const failure of [false,true]) test(`superseded outline/source/matches success and errors do not publish (${failure})`, async () => {
  const h=harness(), old=deferred(), first=outline("first"), second=outline("second","field","second.java");
  indexedNetwork(h,first,(url)=>url.startsWith("/api/symbols?") ? old.promise : undefined);
  await h.run('setSection("files")'); const pending=h.run('selectFilesFile(files[0])');
  indexedNetwork(h,second); h.context.file={path:"second.java",methodCount:0}; await h.run('selectFilesFile(file)');
  if(failure) old.resolve({ok:false,status:409,json:async()=>({error:{code:"revision_conflict",message:"obsolete"}})});
  else old.resolve(response({revision:pin,path:"main.java",items:[first],nextOffset:null,truncated:false,warnings:[]}));
  await pending; assert.equal(h.run("filesFile.path"),"second.java"); assert.doesNotMatch(text(h.get("file-tree")),/first/);
  const refs=deferred(); indexedNetwork(h,second,(url)=>url==="/api/syntax-matches"?refs.promise:undefined);
  h.context.item=second; const referencePending=h.run('selectFilesSymbol(item)');
  h.run('sectionSerial++; filesSerial++; clearFilesInspector()');
  if(failure) refs.reject(new Error("obsolete refs")); else refs.resolve(response(matches(second,[{path:"old",range:{startByte:0,endByte:1,startLine:1,startColumn:1},snippet:"old"}])));
  await referencePending; assert.equal(h.get("files-matches").children.length,0);
});

test("file/session/full-pin/section fences suppress late source and saved reference actions", async () => {
  for(const transition of ['filesFile={path:"other.java"}', 'epoch++', 'status.revision={...status.revision,indexRevision:2}', 'sectionSerial++']) {
    const h=harness(), pendingSource=deferred(), item=outline("same"); indexedNetwork(h,item,(url)=>url.startsWith("/api/source?")?pendingSource.promise:undefined);
    await h.run('setSection("files")'); const pending=h.run('selectFilesFile(files[0])'); h.run(transition);
    pendingSource.resolve(response({revision:pin,file:{path:"main.java",text:"OLD",hash:"old",language:"java"}}));
    await pending; assert.doesNotMatch(text(h.get("source")),/OLD/);
  }
  const h=harness(), item=outline("same"), location={path:"main.java",range:{startByte:0,endByte:1,startLine:1,startColumn:1,endLine:1,endColumn:2},snippet:"same"};
  indexedNetwork(h,item,(url)=>url==="/api/syntax-matches"?response(matches(item,[location])):undefined);
  await h.run('setSection("files")'); await h.run('selectFilesFile(files[0])'); h.context.item=item; await h.run('selectFilesSymbol(item)');
  const row=all(h.get("files-matches")).find(node=>node.className==="files-match"), before=h.requests.length;
  h.run('sectionSerial++'); await row.listeners.click(); assert.equal(h.requests.length,before);
});

const git={state:"available",head:{kind:"branch",name:"main"},upstream:null};
const fileDiff={path:"main.java",status:"modified",previousPath:null,binary:false,hunks:[{oldStart:4,newStart:4,oldLines:2,newLines:2,header:"@@ -4,2 +4,2 @@\n",lines:[{kind:"context",text:"same\n"},{kind:"deleted",text:"<old>\n"},{kind:"added",text:"<new>"}]}]};
test("Working tree renders exact HEAD hunks/terminators and +/- gutters without indexed navigation", async () => {
  const h=harness(); indexedNetwork(h); await h.run('setSection("files")'); await h.run('selectFilesFile(files[0])');
  h.run(`selectedGit=${JSON.stringify(git)}`);
  h.network(url=>url==="/api/git"?response({git,changes:[{path:"main.java",status:"modified"}]}):response({git,file:fileDiff}));
  await h.run('setFilesBasis("working")');
  assert.equal(h.get("source").hidden,true); assert.match(h.get("files-basis-note").textContent,/Uncommitted vs HEAD · Not indexed/);
  const rows=all(h.get("files-diff")).filter(node=>node.className==="files-diff-line");
  assert.deepEqual(rows.map(row=>row.children.map(child=>child.textContent)),[["4","4"," ","same\n"],["5","","−","<old>\n"],["","5","+","<new>"]]);
  assert.ok(rows.every(row=>!row.classList.contains("source-line"))); assert.equal(h.get("files-diff").children[1].textContent,fileDiff.hunks[0].header);
  await h.run('setFilesBasis("indexed")'); assert.equal(h.get("source").hidden,false); assert.equal(h.get("files-diff").hidden,true);
});

test("Git unchanged, unavailable, binary, rename and delete stay explicit and safe", async () => {
  const h=harness(); h.context.git=git;
  h.run('window.TrellisFiles.diff($("files-diff"),{git,file:null})'); assert.match(text(h.get("files-diff")),/No uncommitted/);
  h.run('window.TrellisFiles.diff($("files-diff"),{git:{state:"unavailable",reason:"git_changed"},file:null})'); assert.match(text(h.get("files-diff")),/git_changed/);
  h.context.diff={...fileDiff,binary:true}; h.run('window.TrellisFiles.diff($("files-diff"),{git,file:diff})'); assert.match(text(h.get("files-diff")),/Binary/);
  h.context.diff={...fileDiff,status:"renamed",previousPath:"<old>.java"}; h.run('window.TrellisFiles.diff($("files-diff"),{git,file:diff})'); assert.match(text(h.get("files-diff")),/<old>.java → main.java/);
  h.context.diff={...fileDiff,status:"deleted"}; h.run('window.TrellisFiles.diff($("files-diff"),{git,file:diff})'); assert.match(text(h.get("files-diff")),/deleted/);
  assert.ok(all(h.get("files-diff")).every(node=>node.tagName!=="old"));
});

test("no diff request for unchanged path; obsolete diff cannot replace a newer basis", async () => {
  const h=harness(); indexedNetwork(h); await h.run('setSection("files")'); await h.run('selectFilesFile(files[0])'); h.run(`selectedGit=${JSON.stringify(git)}`);
  h.network(()=>response({git,changes:[]})); await h.run('setFilesBasis("working")');
  assert.equal(h.requests.filter(r=>r.url.includes("/git/diff")).length,0); assert.match(text(h.get("files-diff")),/No uncommitted/);
  const old=deferred(), requested=deferred(); h.network(url=>{
    if(url==="/api/git") return response({git,changes:[{path:"main.java"}]});
    requested.resolve(); return old.promise;
  });
  const pending=h.run('setFilesBasis("working")'); await requested.promise;
  await h.run('setFilesBasis("indexed")'); old.resolve(response({git,file:fileDiff})); await pending;
  assert.equal(h.get("files-diff").hidden,true); assert.doesNotMatch(text(h.get("files-diff")),/<old>/);
});


test("cached catalog pages retain deleted-live paths and zero-callable rows", async () => {
  const h=harness(), item=outline("Holder","class","cached-only.java");
  indexedNetwork(h,item,(url)=>url.startsWith("/api/files?") ? response({revision:pin,items:[{path:url.includes("offset=200")?"zero.java":"cached-only.java",language:"java",methodCount:0}],nextOffset:url.includes("offset=200")?null:200}) : undefined);
  await h.run('setSection("files")'); await h.run('loadFiles()');
  assert.match(text(h.get("file-tree")),/cached-only.java/); assert.match(text(h.get("file-tree")),/zero.java/);
  assert.equal(h.run("files.length"),2); assert.ok(h.requests.every(r=>!r.url.includes("/tree?")&&!r.url.includes("/source?")));
});

for(const failure of [false,true]) test(`mini replacement and reset ignore delayed ${failure?"failure":"success"}`, async () => {
  const h=harness(), item=outline("measured"), old=deferred();
  indexedNetwork(h,item,(url)=>url==="/api/sequence"?old.promise:undefined);
  await h.run('setSection("files")'); await h.run('selectFilesFile(files[0])'); h.context.item=item;
  const pending=h.run('selectFilesSymbol(item)');
  h.context.field=outline("field","field"); await h.run('selectFilesSymbol(field)');
  if(failure) old.resolve({ok:false,status:409,json:async()=>({error:{code:"revision_conflict",message:"old"}})});
  else old.resolve(response(view(item)));
  await pending; assert.equal(h.run("filesSymbol.symbol.id"),"field"); assert.equal(h.get("files-open-full").disabled,true);
  assert.match(h.get("files-mini-state").textContent,/noncallable/);
  h.run('clearBrowse()'); assert.equal(h.run("filesFile"),null); assert.equal(h.get("files-mini-sequence").children.length,0);
});

test("Working status and diff failures clear previous hunks and no-Git hides the toggle", async () => {
  const h=harness(); indexedNetwork(h); await h.run('setSection("files")'); await h.run('selectFilesFile(files[0])');
  h.run(`selectedGit=${JSON.stringify(git)}`);
  h.network(url=>url==="/api/git"?response({git,changes:[{path:"main.java"}]}):response({git,file:fileDiff}));
  await h.run('setFilesBasis("working")'); assert.match(text(h.get("files-diff")),/<old>/);
  h.network(()=>response({git:{state:"unavailable",reason:"git_changed"},changes:null}));
  await h.run('setFilesBasis("working")'); assert.doesNotMatch(text(h.get("files-diff")),/<old>/);
  assert.match(text(h.get("files-diff")),/git_changed/); assert.equal(h.get("files-working").hidden,true);
  h.run(`selectedGit=${JSON.stringify(git)}`);
  h.network(url=>url==="/api/git"?response({git,changes:[{path:"main.java"}]}):Promise.reject(new Error("Git changed during diff")));
  await h.run('setFilesBasis("working")'); assert.doesNotMatch(text(h.get("files-diff")),/<old>/); assert.match(h.get("files-diff").textContent,/Git changed during diff/);
});


test("section return rebinds cached source navigation without fetching and Working reentry only exposes diff", async () => {
  const h=harness(), attachments=[]; h.context.attachments=attachments;
  h.run('window.TrellisNavigation={reset(){}, attachSource(node,options){attachments.push(options)}}');
  indexedNetwork(h); await h.run('setSection("files")'); await h.run('selectFilesFile(files[0])');
  const fileAttachment=attachments.at(-1), reads=h.requests.filter(r=>r.url.includes("/source?")).length;
  assert.equal(fileAttachment.isCurrent(),true); await h.run('setSection("diagrams")');
  assert.equal(fileAttachment.isCurrent(),false); assert.equal(attachments.at(-1).isCurrent(),true);
  assert.equal(h.requests.filter(r=>r.url.includes("/source?")).length,reads);
  await h.run('setSection("files")'); h.run(`selectedGit=${JSON.stringify(git)}`);
  h.network(url=>url==="/api/git"?response({git,changes:[{path:"main.java"}]}):response({git,file:fileDiff}));
  await h.run('setFilesBasis("working")'); const indexedAttachment=attachments.at(-1);
  await h.run('setSection("diagrams")'); await h.run('setSection("files")');
  assert.equal(h.get("source").hidden,true); assert.equal(h.get("files-diff").hidden,false);
  assert.equal(indexedAttachment.isCurrent(),false); assert.match(text(h.get("files-diff")),/<old>/);
});

test("newer Working Git observation wins over a held earlier refresh success and failure", async () => {
  for(const failure of [false,true]) {
    const h=harness(), old=deferred(); indexedNetwork(h); await h.run('setSection("files")'); await h.run('selectFilesFile(files[0])');
    h.run(`selectedGit=${JSON.stringify(git)}`); h.network(()=>old.promise); const pending=h.run('refreshGit()');
    h.network(url=>url==="/api/git"?response({git,changes:[{path:"main.java"}]}):response({git,file:fileDiff}));
    await h.run('setFilesBasis("working")');
    if(failure) old.reject(new Error("obsolete Git read")); else old.resolve(response({git:{state:"unavailable",reason:"not_repository"},changes:null}));
    await pending; assert.equal(h.run("selectedGit.state"),"available"); assert.equal(h.get("files-working").hidden,false);
    assert.match(text(h.get("files-diff")),/<old>/);
  }
});


test("active Files click keeps navigation alive; explicit diagram source reconciles old Files declaration", async () => {
  const h=harness(), attachments=[], item=outline("measured"); h.context.attachments=attachments;
  h.run('window.TrellisNavigation={reset(){},attachSource(node,options){attachments.push(options)}}');
  indexedNetwork(h,item); await h.run('setSection("files")'); await h.run('selectFilesFile(files[0])'); h.context.item=item;
  await h.run('selectFilesSymbol(item)'); const attached=attachments.at(-1), before=attachments.length;
  await h.run('setSection("files")'); assert.equal(attachments.length,before); assert.equal(attached.isCurrent(),true);
  await h.run('setSection("diagrams")');
  h.network(()=>response({revision:pin,file:{path:"other.java",text:"other source",language:"java",hash:"other"}}));
  await h.run(`showCachedSource("other.java",null,${JSON.stringify(pin)})`);
  await h.run('setSection("files")');
  assert.equal(h.run("filesSymbol"),null); assert.equal(h.run("filesFile.path"),"other.java");
  assert.equal(h.get("files-mini-sequence").children.length,0); assert.match(h.get("source-path").textContent,/other.java/);
  assert.doesNotMatch(h.get("files-symbol-title").textContent,/measured/); assert.equal(attachments.at(-1).isCurrent(),true);
  // A different measured range in the same file is also a new source intent.
  indexedNetwork(h,item); await h.run('selectFilesFile({path:"main.java"})'); await h.run('selectFilesSymbol(item)');
  await h.run('setSection("diagrams")');
  await h.run(`showCachedSource("main.java",{startLine:2,endLine:2,startColumn:1,endColumn:3},${JSON.stringify(pin)})`);
  await h.run('setSection("files")'); assert.equal(h.run("filesSymbol"),null);
  assert.equal(h.run("workspaceSource.range.startLine"),2); assert.equal(h.get("files-mini-sequence").children.length,0);
});


test("retained outline More and Retry intents cannot read a successor checkout/session/full pin", async () => {
  for(const label of ["More symbols", "Retry symbols"] ) for(const transition of ['epoch++', 'selectedRootKey="b".repeat(64)', 'status.revision={...status.revision,indexRevision:2}']) {
    const h=harness(), item=outline("Holder","class"); indexedNetwork(h,item); await h.run('setSection("files")'); await h.run('selectFilesFile(files[0])');
    if(label==="More symbols") h.run('fileStates.get("main.java").outlineNext=200;renderFiles()');
    else h.run('fileStates.get("main.java").outlineError="retry me";renderFiles()');
    const pick=all(h.get("file-tree")).find(node=>node.tagName==="button"&&node.textContent.startsWith(label));
    assert.ok(pick); h.run(transition); const count=h.requests.length;
    await pick.listeners.click(); assert.equal(h.requests.length,count);
  }
});


test("selecting another file collapses previous outline; interrupted outline exposes retry on return", async () => {
  const h=harness(), first=outline("First","class"), second=outline("Second","class","other.java"); indexedNetwork(h,first);
  await h.run('setSection("files")'); await h.run('selectFilesFile(files[0])');
  assert.match(text(h.get("file-tree")),/class · First/);
  h.context.secondFile={path:"other.java",methodCount:0}; h.run('files.push(secondFile)'); indexedNetwork(h,second);
  await h.run('selectFilesFile(secondFile)'); assert.doesNotMatch(text(h.get("file-tree")),/class · First/);
  assert.match(text(h.get("file-tree")),/class · Second/);
  const held=deferred(); indexedNetwork(h,second,url=>url.startsWith("/api/symbols?")?held.promise:undefined);
  const pending=h.run('loadFilesOutline(secondFile)'); await h.run('setSection("diagrams")');
  held.resolve(response({revision:pin,path:"other.java",items:[second],nextOffset:null,truncated:false,warnings:[]})); await pending;
  await h.run('setSection("files")'); assert.match(text(h.get("file-tree")),/Retry symbols · Symbol read interrupted/);
  assert.doesNotMatch(text(h.get("file-tree")),/Loading symbols/);
});


test("changed-only catalog action cannot disable Indexed after the full pin captures that path", async () => {
  const h=harness(); indexedNetwork(h); await h.run('setSection("files")');
  h.context.observedGit=git;
  h.run('filesGitStatus={git:observedGit,changes:[{path:"new.java",status:"added"}]};renderFiles()');
  const old=all(h.get("file-tree")).find(node=>node.className==="files-change-pick");
  assert.ok(old);
  h.run('status.revision={...status.revision,indexRevision:2};files.push({path:"new.java",language:"java",methodCount:0});filesFile={path:"new.java"};filesBasis="indexed";updateFilesBasis()');
  const before=h.requests.length;
  await old.listeners.click();
  assert.equal(h.requests.length,before);
  assert.equal(h.run('filesFile.changeOnly'),undefined);
  assert.equal(h.get("files-indexed").disabled,false);
});
