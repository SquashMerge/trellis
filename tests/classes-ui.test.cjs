"use strict";
// Synthetic cached declarations only. Run: node --test tests/classes-ui.test.cjs
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const source = fs.readFileSync(path.join(__dirname, "../web/classes.js"), "utf8");
const pinSource = fs.readFileSync(path.join(__dirname, "../web/app.js"), "utf8").match(/const IndexPin = Object.freeze\(\{[\s\S]*?\n\}\);\nwindow.TrellisIndexPin = IndexPin;/)[0];
const descendants = node => [node, ...node.children.flatMap(descendants)];
const text = node => descendants(node).map(item => item.textContent).join(" ");
const deferred = () => { let resolve, reject; const promise = new Promise((yes, no) => {resolve = yes; reject = no;}); return {promise, resolve, reject}; };
const range = {startLine: 1, endLine: 8, startByte: 0, endByte: 120};
function definition(id) { return {symbol: {id, name: id, path: `src/${id}.java`, range, provenance: {parser: "syntax"}}, qualifiedName: `sample.${id}`, language: "java", declarationKind: "class", fields: [{name:"other",typeHint:"B",path:`src/${id}.java`,range}], methods: [{name:"execute",symbolId:`${id}.execute`,typeHint:"void",path:`src/${id}.java`,range}], truncated:false}; }
const node = id => ({id, label:id, kind:"class", expandable:true, class:definition(id)});
function diagram(ids = ["A", "B"], revision = {indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1}) { return {revision, seed: ids[0], nodes:ids.map(node), edges:ids.slice(1).map((id,i)=>({id:`e${i}`,owner:ids[0],target:id,typeName:id,kind:"field",matchKind:"syntaxCandidate",candidateIds:[id],path:"A.java",range})), warnings:[],truncated:false}; }
function harness({navigation = false} = {}) {
  const elements = new Map(), calls = [], reads = [], methods = [], changes = [], navigations = [], stale = [];
  let navigate = (event, selector, scope) => { navigations.push({event, selector, scope}); };
  const docListeners = {}, winListeners = {};
  let revision = {indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1}, session = "one";
  let request = async (url, options) => options ? diagram() : {revision,items:[definition("A")],nextOffset:null,warnings:[],truncated:false};
  const document = {activeElement:null, addEventListener(type, fn) { (docListeners[type] ||= []).push(fn); }};
  function element(tagName) {
    const n = {tagName, textContent:"", className:"", attrs:{},dataset:{},style:{},children:[],listeners:{},parentNode:null,value:"",checked:false,disabled:false,tabIndex:-1,scrollTop:0,scrollLeft:0,
      setAttribute(k,v) { this.attrs[k] = String(v); if(k==="class")this.className=String(v); },
      getAttribute(k) {return this.attrs[k];},
      get isConnected() {return this === document.body || !!this.parentNode?.isConnected;},
      addEventListener(type, fn) {(this.listeners[type] ||= []).push(fn);},
      append(...items) {for(const item of items){item.remove();item.parentNode=this;this.children.push(item);}},
      replaceChildren(...items) {for(const item of this.children)item.parentNode=null;this.children=[];this.append(...items);},
      before(item) {item.remove();item.parentNode=this.parentNode;const i=this.parentNode.children.indexOf(this);this.parentNode.children.splice(i,0,item);},
      remove() {if(this.parentNode){this.parentNode.children=this.parentNode.children.filter(item=>item!==this);this.parentNode=null;}},
      contains(item) {return descendants(this).includes(item);},
      querySelector(selector) {return descendants(this).slice(1).find(item=>selector.startsWith(".") ? item.className.split(" ").includes(selector.slice(1)) : item.tagName===selector) || null;},
      focus() {document.activeElement=this; for(const fn of docListeners.focusin||[])fn({target:this});},
      getBoundingClientRect() {return this.className==="classes-context-menu"?{width:220,height:150,left:0,top:0,bottom:150}:{width:32,height:32,left:50,top:30,bottom:62};},
      async fire(type, details={}) {
        const event={type,target:this,currentTarget:this,preventDefault(){this.prevented=true;},stopPropagation(){this.stopped=true;},...details};
        for(let current=this;current;current=current.parentNode){
          event.currentTarget=current;
          for(const fn of current.listeners[type]||[]) await fn(event);
          if(event.stopped)break;
        }
        return event;
      }
    };return n;
  }
  document.body=element("body"); document.createElement=element;document.createElementNS=(_,tag)=>element(tag);
  document.querySelector=selector=>document.body.querySelector(selector);
  document.getElementById=id=>{if(!elements.has(id)){const item=element(id);elements.set(id,item);document.body.append(item);}return elements.get(id);};
  const window = {innerWidth:390,innerHeight:600,addEventListener(type,fn){(winListeners[type] ||= []).push(fn);}};
  vm.runInNewContext(pinSource + "\n" + source,{window,document,URLSearchParams,console});
  window.TrellisClasses.init({...(navigation ? {navigateMember:(...args)=>navigate(...args)} : {}),request:(url,options)=>{calls.push({url,options});return request(url,options);},readSource:(step,rev)=>reads.push({step,revision:rev}),selectMethod:symbol=>methods.push(symbol),currentRevision:()=>revision,currentSession:()=>session,onChange:()=>changes.push(true),onStale:message=>stale.push(message)});
  return {controller:window.TrellisClasses,document,window,calls,reads,methods,changes,navigations,stale,get:document.getElementById,
    setNavigate(fn){navigate=fn;},setRequest(fn){request=fn;},setRevision(value){revision=value;},setSession(value){session=value;},
    async event(type,target){for(const fn of docListeners[type]||[]) await fn({target});},
    menu(){return document.body.querySelector(".classes-context-menu");},
    card(id){return descendants(document.getElementById("classes-diagram")).find(n=>n.dataset.classId===id);},
    button(container,label){const button=descendants(container).find(n=>n.tagName==="button"&&n.textContent===label);assert.ok(button,`Missing ${label}`);return button;}
  };
}
function freeze(value) { if(value&&typeof value==="object"){Object.freeze(value);Object.values(value).forEach(freeze);}return value; }
async function related(h, from, ids) {
 await h.card(from).fire("contextmenu"); await h.button(h.menu(),"Show related classes").fire("click");
 const chooser=h.document.body.querySelector(".classes-chooser");
 for(const id of ids) { const check=descendants(chooser).find(n=>n.tagName==="input"&&n.value===id); assert.ok(check,`Missing choice ${id}`);check.checked=true;await check.fire("change"); }
 await h.button(chooser,"Add selected classes").fire("click");
}
async function allReturned(h) { await h.button(h.document.body,"Show all returned classes").fire("click"); }

test("init is inert; global search and file open only read catalog/diagram",async()=>{
 const h=harness();assert.equal(h.calls.length,0);
 await h.get("classes-search").fire("click");assert.match(h.calls[0].url,/^\/api\/classes\?/);assert.equal(h.calls.length,1);
 assert.equal(new URL(h.calls[0].url,"http://local").searchParams.has("path"),false);
 await h.controller.open({path:"src/A.java"});assert.equal(h.calls.length,3);assert.equal(h.calls[2].options.body.seed,"A");assert.equal(h.reads.length,0);assert.equal(h.methods.length,0);
 assert.equal(new URL(h.calls[1].url,"http://local").searchParams.get("path"),"src/A.java");
 assert.match(text(h.get("classes-diagram")),/Members · 1 field · 1 method/);
 assert.equal(h.card("B"),undefined); assert.equal(h.card("A").querySelector(".classes-member-detail").hidden,false);
 assert.equal(h.get("classes-results").hidden,true);
});
async function assertTerminalClassGraph(kind, nodes = ["A", "B", "C"]) {
 const h=harness(), data=diagram(nodes);
 data.edges.push({id:`legacy-${kind}`,owner:"B",target:"C",kind,matchKind:"actual",candidateIds:["C"],path:"A.java",range});
 const original=JSON.stringify(data);
 h.setRequest(async()=>data);
 await h.controller.open({seed:"A"});
 assert.ok(h.card("A"),"measured declaration remains selectable");
 assert.equal(h.card("B"),undefined,"candidate relation never selects a declaration");
 assert.equal(descendants(h.get("classes-diagram")).filter(n=>n.className==="classes-edge").length,0);
 assert.equal(descendants(h.get("classes-diagram")).filter(n=>n.className==="classes-relationship").length,0);
 await allReturned(h);
 assert.ok(h.card("B"),"all-declarations view lists independent measured classes");
 assert.equal(descendants(h.get("classes-diagram")).filter(n=>n.className==="classes-edge").length,0);
 assert.equal(h.reads.length,0,"display does not fetch source");
 assert.equal(JSON.stringify(data),original,"the old DTO is not mutated");
}
test("old extends class relation remains terminal (1)",async()=>assertTerminalClassGraph("extends"));

test("source is explicit and method click passes a measured symbol ID",async()=>{
 const h=harness();await h.controller.open({seed:"A"});assert.equal(h.reads.length,0);
 await h.card("A").fire("contextmenu",{clientX:100,clientY:100});await h.button(h.menu(),"Read class source").fire("click");
 assert.equal(h.reads.length,1);assert.equal(h.reads[0].revision.indexRevision,1);assert.equal(h.reads[0].step.path,"src/A.java");
 await h.button(h.card("A"),"execute()").fire("click");assert.equal(h.methods[0].id,"A.execute");assert.equal(h.methods[0].parent,"A");assert.equal(h.calls.length,1);
});
async function assertNoInferredClassChooser(kind) {
 const h=harness(), data=diagram(["A","B","C"]);
 data.edges.push({id:`legacy-${kind}`,owner:"A",target:"C",kind,matchKind:"syntaxCandidate",candidateIds:["C"],path:"A.java",range});
 h.setRequest(async()=>data);
 await h.controller.open({seed:"A"});
 const calls=h.calls.length;
 await h.card("A").fire("contextmenu");
 assert.ok(h.menu());
 assert.equal(descendants(h.menu()).some(n=>n.textContent==="Show related classes"),false);
 assert.equal(h.card("B"),undefined);
 assert.equal(h.calls.length,calls,"opening the menu does not fetch neighbor classes");
 assert.equal(h.reads.length,0,"opening the menu does not read source");
 assert.ok(h.card("A"),"measured declaration remains selectable");
}
test("old extends relation exposes no guessed chooser (1)",async()=>assertNoInferredClassChooser("extends"));

test("old implements class relation remains terminal (2)",async()=>assertTerminalClassGraph("implements"));

test("menu keyboard navigation skips disabled items, clamps viewport, and restores focus",async()=>{
 const h=harness(),data=diagram();data.nodes[0].expandable=false;h.setRequest(async()=>data);await h.controller.open({seed:"A"});const anchor=h.card("A");anchor.focus();
 const event=await anchor.fire("keydown",{key:"F10",shiftKey:true});assert.equal(event.prevented,true);const menu=h.menu();assert.equal(h.document.activeElement.textContent,"Read class source");
 await menu.fire("keydown",{key:"End"});assert.equal(h.document.activeElement.textContent,"Focus this class");
 await menu.fire("keydown",{key:"ArrowDown"});assert.equal(h.document.activeElement.textContent,"Read class source");
 await menu.fire("keydown",{key:"ArrowUp"});assert.equal(h.document.activeElement.textContent,"Focus this class");
 await menu.fire("keydown",{key:"Home"});assert.equal(h.document.activeElement.textContent,"Read class source");
 await menu.fire("keydown",{key:"Escape"});assert.equal(h.menu(),null);assert.equal(h.document.activeElement,anchor);
 await anchor.fire("contextmenu",{clientX:389,clientY:599});assert.equal(h.menu().style.left,"162px");assert.equal(h.menu().style.top,"442px");
 await h.event("pointerdown",h.get("classes-query"));assert.equal(h.menu(),null);
});
test("visible touch menu and ContextMenu key work; scroll dismisses",async()=>{
 const h=harness();await h.controller.open({seed:"A"});await h.button(h.card("A"),"⋯").fire("click");assert.ok(h.menu());
 h.get("classes-diagram").scrollTop++;await h.event("scroll",h.get("classes-diagram"));assert.equal(h.menu(),null);
 await h.card("A").fire("keydown",{key:"ContextMenu"});assert.ok(h.menu());await h.menu().fire("keydown",{key:"Tab"});assert.equal(h.menu(),null);
});
test("stale open responses, revision responses and old actions do not affect new view",async()=>{
 const h=harness(), first=deferred();h.setRequest((_,options)=>options.body.seed==="A"?first.promise:Promise.resolve(diagram(["B"])));
 const old=h.controller.open({seed:"A"});await h.controller.open({seed:"B"});first.resolve(diagram());await old;assert.ok(h.card("B"));assert.equal(h.card("A"),undefined);
 await h.card("B").fire("contextmenu");const source=h.button(h.menu(),"Read class source");h.setRevision({indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:2});await source.fire("click");assert.equal(h.reads.length,0);
 h.setRequest(async()=>diagram(["C"],1));await h.controller.open({seed:"C"});assert.equal(h.get("classes-state").dataset.state,"stale");assert.equal(h.card("C"),undefined);
});
test("reset invalidates pending lookup and source callbacks; session guards reject late failures",async()=>{
 const h=harness(), pending=deferred();h.setRequest(()=>pending.promise);const work=h.controller.open({});h.controller.reset();pending.resolve({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},items:[definition("A")],warnings:[]});await work;assert.equal(h.get("classes-results").children.length,0);
 const next=deferred();h.setRequest(()=>next.promise);const work2=h.controller.open({seed:"A"});h.setSession("two");next.reject(Error("old failure"));await work2;assert.doesNotMatch(text(h.get("classes-state")),/old failure/);
 assert.ok(h.changes.length>=4);
});
test("loading a new view invalidates keyboard member/source callbacks in old diagram",async()=>{
 const h=harness();await h.controller.open({seed:"A"});const member=h.button(h.card("A"),"execute()");
 const pending=deferred();h.setRequest(()=>pending.promise);const work=h.controller.open({seed:"B"});await member.fire("click");assert.equal(h.methods.length,0);
 pending.resolve(diagram(["B"]));await work;
});
test("unsupported, unindexed, empty, partial and error states are explicit",async()=>{
 const h=harness();await h.controller.open({path:"src/main.go"});assert.equal(h.get("classes-state").dataset.state,"unsupported");assert.equal(h.calls.length,0);
 h.setRequest(async()=>({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},items:[],warnings:["Index workspace to populate class declarations."],requireIndex:true}));await h.controller.open({});assert.equal(h.get("classes-state").dataset.state,"unindexed");
 h.setRequest(async()=>({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},items:[],warnings:[]}));await h.controller.open({});assert.equal(h.get("classes-state").dataset.state,"empty");
 h.setRequest(async()=>({...diagram(),truncated:true,warnings:["Bound reached"]}));await h.controller.open({seed:"A"});assert.equal(h.get("classes-state").dataset.state,"partial");
 h.setRequest(async()=>{throw Error("Not a class or enclosing method");});await h.controller.open({seed:"bad"});assert.match(text(h.get("classes-state")),/Not a class/);assert.equal(h.get("classes-state").dataset.state,"error");
});
test("old field class relation remains terminal (3)",async()=>assertTerminalClassGraph("field"));

test("public context menu actions share guards and return focus to root anchors",async()=>{
 const h=harness(), anchor=h.get("root-method");let calls=0;
 h.controller.showContextMenu({type:"keydown",key:"ContextMenu",currentTarget:anchor,preventDefault(){},stopPropagation(){}},[{label:"Show enclosing class",run:()=>calls++}]);
 await h.button(h.menu(),"Show enclosing class").fire("click");assert.equal(calls,1);assert.equal(h.document.activeElement,anchor);
});

test("large warning sets stay collapsed and bounded outside the diagram status",async()=>{
 const h=harness(), data=diagram();data.warnings=Array.from({length:100},(_,i)=>`Notice ${i}: `+"x".repeat(3000));
 h.setRequest(async()=>data);await h.controller.open({seed:"A"});
 const notices=h.document.body.querySelector(".classes-warnings");assert.equal(notices.open,false);assert.equal(notices.hidden,false);
 assert.match(text(notices),/100 indexing \/ diagram notices/);assert.match(text(notices),/notice shortened/);assert.match(text(notices),/50 more notices omitted/);
 assert.ok(h.get("classes-state").textContent.length<500);assert.ok(text(notices).length<110000);
 h.controller.reset();assert.equal(notices.hidden,true);
});

test("old implements relation exposes no guessed chooser (2)",async()=>assertNoInferredClassChooser("implements"));

test("failed lookup restores visible diagram actions after superseding a pending diagram refresh",async()=>{
 const h=harness();await h.controller.open({seed:"A"});const card=h.card("A"), pending=deferred();
 h.setRequest((url,options)=>options ? pending.promise : Promise.reject(Object.assign(Error("Storage is busy"),{status:409,code:"storage_busy"})));
 const refreshing=h.get("classes-unmatched").fire("change");
 await h.get("classes-search").fire("click");
 assert.equal(h.card("A"),card);assert.equal(h.get("classes-state").dataset.state,"error");
 assert.equal(h.stale.length,0);
 await h.button(card,"execute()").fire("click");assert.equal(h.methods.length,1);
 pending.resolve(diagram(["A","B","C"]));await refreshing;
 assert.equal(h.card("A"),card);assert.equal(h.card("C"),undefined);
 await h.button(card,"execute()").fire("click");assert.equal(h.methods.length,2);
 assert.equal(h.stale.length,0);
});

test("class search keeps its prior diagram and controls through storage_busy, then clears on revision_conflict",async()=>{
 const h=harness();await h.controller.open({seed:"A"});
 const card=h.card("A"), control=h.button(h.document.body,"Show all returned classes"), pending=deferred();
 h.setRequest(()=>pending.promise);
 const searching=h.get("classes-search").fire("click");
 assert.equal(h.card("A"),card);assert.equal(control.parentNode.hidden,false);
 const busy=Error("Storage is busy");busy.status=409;busy.code="storage_busy";
 pending.reject(busy);await searching;
 assert.equal(h.card("A"),card);assert.equal(control.parentNode.hidden,false);
 assert.equal(h.get("classes-state").dataset.state,"error");
 assert.match(text(h.get("classes-state")),/Storage is busy/);
 assert.equal(h.stale.length,0);
 await h.button(card,"execute()").fire("click");assert.equal(h.methods.length,1);
 h.setRequest(async()=>{const error=Error("Index changed");error.status=409;error.code="revision_conflict";throw error;});
 await h.get("classes-search").fire("click");
 assert.equal(h.card("A"),undefined);assert.equal(control.parentNode.hidden,true);
 assert.equal(h.get("classes-state").dataset.state,"stale");
 assert.equal(h.stale.length,1);
 await h.button(card,"execute()").fire("click");assert.equal(h.methods.length,1);
});

test("old field relation exposes no guessed chooser (3)",async()=>assertNoInferredClassChooser("field"));

test("old extends relation exposes no guessed chooser (4)",async()=>assertNoInferredClassChooser("extends"));

test("card headings use short names and retain accessible qualified identities",async()=>{
 const h=harness(), data=diagram();data.nodes[0].label="com.example.very.long.namespace.A";data.nodes[0].class.qualifiedName=data.nodes[0].label;
 data.truncated=true;h.setRequest(async()=>data);await h.controller.open({seed:"A"});
 assert.equal(h.card("A").querySelector("h3").textContent,"A");
 const qualified=h.card("A").querySelector(".classes-qualified");assert.equal(qualified.textContent,data.nodes[0].label);assert.equal(qualified.title,data.nodes[0].label);
 assert.match(h.card("A").getAttribute("aria-label"),/com.example.very.long.namespace.A/);
 assert.match(text(h.get("classes-state")),/some index or diagram details are omitted/);assert.doesNotMatch(text(h.get("classes-state")),/display limits reached/);
});


test("file context menus preserve the existing inline-expansion aria state", async () => {
  const h=harness(), anchor=h.get("file-trigger");anchor.setAttribute("aria-expanded","true");
  h.controller.showContextMenu({currentTarget:anchor,preventDefault(){},stopPropagation(){}},[{label:"Class diagram",run(){}}]);
  h.controller.closeContextMenu();assert.equal(anchor.getAttribute("aria-expanded"),"true");
});

test("queued pre-menu scroll is ignored but actual subsequent scrolling dismisses", async () => {
  const h=harness(), anchor=h.get("file-trigger"), parent=anchor.parentNode;
  parent.scrollTop=100;parent.scrollLeft=0;
  h.controller.showContextMenu({currentTarget:anchor,preventDefault(){},stopPropagation(){}},[{label:"Class diagram",run(){}}]);
  const sibling=h.get("file-tree");sibling.scrollTop=40;
  await h.event("scroll",sibling);assert.ok(h.menu());
  await h.event("scroll",parent);assert.ok(h.menu());
  parent.scrollTop=101;await h.event("scroll",parent);assert.equal(h.menu(),null);
});


test("members reveal is reversible and preserves measured source/method actions without requests", async () => {
 const h=harness();await h.controller.open({seed:"A"});const a=h.card("A"),calls=h.calls.length;
 assert.equal(a.querySelector(".classes-members-toggle").getAttribute("aria-expanded"),"true","members show by default (#559)");
 assert.equal(a.querySelector(".classes-member-detail").hidden,false);assert.equal(a.style.height,"390px");
 await h.button(a,"execute()").fire("click");assert.equal(h.methods[0].range.startLine,range.startLine);assert.equal(h.methods[0].id,"A.execute");
 await a.querySelector(".classes-members-toggle").fire("click");assert.equal(h.card("A"),a);assert.equal(a.querySelector(".classes-member-detail").hidden,true);
 assert.equal(a.querySelector(".classes-members-toggle").getAttribute("aria-expanded"),"false");assert.equal(a.style.height,"116px");
 assert.equal(h.document.activeElement,a.querySelector(".classes-members-toggle"));
 await a.querySelector(".classes-members-toggle").fire("click");assert.equal(a.querySelector(".classes-member-detail").hidden,false);
 assert.equal(h.calls.length,calls);assert.equal(h.reads.length,0);
});
test("old extends class relation remains terminal (4)",async()=>assertTerminalClassGraph("extends"));

test("old implements class relation remains terminal (5)",async()=>assertTerminalClassGraph("implements"));

test("search results collapse after selection and Change class restores them without another request", async () => {
 const h=harness();await h.controller.open({path:"src/A.java"});const results=h.get("classes-results"),calls=h.calls.length;
 assert.equal(results.hidden,true);const change=h.button(h.document.body,"Change class");assert.equal(change.getAttribute("aria-expanded"),"false");
 await change.fire("click");assert.equal(results.hidden,false);assert.equal(change.getAttribute("aria-expanded"),"true");assert.equal(h.document.activeElement,h.get("classes-query"));
 assert.equal(h.calls.length,calls);await results.children[0].fire("click");assert.equal(results.hidden,true);
});
test("old implements relation exposes no guessed chooser (5)",async()=>assertNoInferredClassChooser("implements"));

test("old field relation exposes no guessed chooser (6)",async()=>assertNoInferredClassChooser("field"));

test("old association class relation remains terminal (6)",async()=>assertTerminalClassGraph("association"));

test("old extends relation exposes no guessed chooser (7)",async()=>assertNoInferredClassChooser("extends"));

test("late search pagination cannot replace a selected diagram status or notices", async () => {
 const h=harness(),page=deferred();h.setRequest(async(url,options)=>{
  if(options)return {...diagram(),warnings:["CURRENT DIAGRAM NOTICE"]};
  const offset=new URL(url,"http://local").searchParams.get("offset");
  return offset==="100"?page.promise:{revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},items:[definition("A")],nextOffset:100,warnings:[]};
 });
 await h.controller.open({});const results=h.get("classes-results"),choose=results.children[0];
 const more=h.button(results,"More classes").fire("click");await choose.fire("click");
 const status=h.get("classes-state").textContent;
 page.resolve({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},items:[definition("B")],warnings:["OLD SEARCH WARNING"],truncated:true});await more;
 assert.equal(h.get("classes-state").textContent,status);assert.equal(results.children.length,1);assert.equal(results.hidden,true);
 const notices=h.document.body.querySelector(".classes-warnings");assert.match(text(notices),/CURRENT DIAGRAM NOTICE/);assert.doesNotMatch(text(notices),/OLD SEARCH WARNING/);
});


test("choosing a class moves focus from hidden search results to its focus card",async()=>{
 const h=harness();await h.controller.open({});const choose=h.get("classes-results").children[0];choose.focus();await choose.fire("click");
 assert.equal(h.get("classes-results").hidden,true);assert.equal(h.document.activeElement,h.card("A"));
});
for (const status of [401,403]) test(`authentication failure ${status} clears measured class state and old actions`,async()=>{
 const h=harness();await h.controller.open({seed:"A"});const member=h.button(h.card("A"),"execute()");
 h.setRequest(async()=>{const e=Error("Not authorized");e.status=status;throw e;});
 await h.controller.open({seed:"A"});
 assert.equal(h.card("A"),undefined);assert.equal(h.document.body.querySelector(".classes-controls").hidden,true);
 await member.fire("click");assert.equal(h.methods.length,0);
});
test("old extends relation exposes no guessed chooser (10)",async()=>assertNoInferredClassChooser("extends"));

test("old implements relation exposes no guessed chooser (11)",async()=>assertNoInferredClassChooser("implements"));

test("old field relation exposes no guessed chooser (12)",async()=>assertNoInferredClassChooser("field"));

test("old extends class relation remains terminal (7)",async()=>assertTerminalClassGraph("extends"));

test("old implements class relation remains terminal (8)",async()=>assertTerminalClassGraph("implements"));

test("old extends class relation remains terminal (9)",async()=>assertTerminalClassGraph("extends"));

test("old field class relation remains terminal (10)",async()=>assertTerminalClassGraph("field"));

test("old extends class relation remains terminal (11)",async()=>assertTerminalClassGraph("extends"));

test("old extends relation exposes no guessed chooser (13)",async()=>assertNoInferredClassChooser("extends"));

test("old implements class relation remains terminal (12)",async()=>assertTerminalClassGraph("implements"));

test("old extends class relation remains terminal (13)",async()=>assertTerminalClassGraph("extends"));

test("old field class relation remains terminal (14)",async()=>assertTerminalClassGraph("field"));

// Member navigation delegates cached resolution; the class UI never searches type text.
const memberRows = (h, id = "A") => descendants(h.card(id)).filter(n=>n.className==="classes-member");
async function toggleMembers(h, id = "A") { await h.card(id).querySelector(".classes-members-toggle").fire("click"); }
test("member navigation is optional; method name stays direct and type text is inert without a hook",async()=>{
 const h=harness();await h.controller.open({seed:"A"});
 const [field,method]=memberRows(h);
 assert.equal(field.querySelector(".classes-member-type").tagName,"span");assert.equal(field.querySelector(".classes-member-menu"),null);
 await method.querySelector(".classes-member-name").fire("click");assert.equal(h.methods[0].id,"A.execute");
 assert.deepEqual(JSON.parse(JSON.stringify(h.methods[0].range)),range);assert.equal(h.calls.length,1);assert.equal(h.reads.length,0);
});
test("opening, folding and revealing members never fetches navigation or source",async()=>{
 const h=harness({navigation:true});await h.controller.open({seed:"A"});assert.equal(h.navigations.length,0);
 await toggleMembers(h);await toggleMembers(h);assert.equal(h.navigations.length,0);assert.equal(h.calls.length,1);assert.equal(h.reads.length,0);
 const [field,method]=memberRows(h);
 await field.querySelector(".classes-member-name").fire("click");assert.equal(h.navigations.length,0);
 await method.querySelector(".classes-member-name").fire("click");assert.equal(h.methods[0].id,"A.execute");assert.equal(h.navigations.length,0);
 await method.querySelector(".classes-member-type").fire("click");assert.equal(h.methods.length,1);assert.equal(h.navigations.length,1);
 assert.equal(h.calls.length,1);assert.equal(h.reads.length,0);
});
test("type hint and member menu pass only exact recorded owner/name/byte selectors, including overloads",async()=>{
 const h=harness({navigation:true}),data=diagram();
 data.nodes[0].class.fields[0]={name:"数据",typeHint:"same.Name<Δ>",path:"src/A.java",range:{startByte:13,endByte:39,startLine:2,endLine:2}};
 data.nodes[0].class.methods=[{name:"运行",symbolId:"A.运行#1",path:"src/A.java",range:{startByte:40,endByte:70,startLine:3,endLine:5}},
 {name:"运行",symbolId:"A.运行#2",typeHint:"same.Name<Δ>",path:"src/A.java",range:{startByte:75,endByte:115,startLine:6,endLine:8}}];
 freeze(data);const before=JSON.stringify(data);h.setRequest(async()=>data);await h.controller.open({seed:"A"});
 const [field,one,two]=memberRows(h);await field.querySelector(".classes-member-type").fire("click");
 await one.querySelector(".classes-member-menu").fire("click");await two.querySelector(".classes-member-type").fire("click");
 assert.deepEqual(h.navigations.map(n=>JSON.parse(JSON.stringify(n.selector))),[
 {classId:"A",memberName:"数据",startByte:13,endByte:39,memberPath:"src/A.java"},{classId:"A",memberName:"运行",startByte:40,endByte:70,memberPath:"src/A.java"},{classId:"A",memberName:"运行",startByte:75,endByte:115,memberPath:"src/A.java"}]);
 for(const item of h.navigations)assert.equal(item.scope.isCurrent(),true);
 await one.querySelector(".classes-member-name").fire("click");await two.querySelector(".classes-member-name").fire("click");
 assert.deepEqual(h.methods.map(m=>m.id),["A.运行#1","A.运行#2"]);
 assert.deepEqual(h.methods.map(m=>JSON.parse(JSON.stringify(m.range))),data.nodes[0].class.methods.map(m=>m.range));
 assert.equal(JSON.stringify(data),before);assert.equal(h.calls.length,1);assert.equal(h.reads.length,0);
});
test("member right-click, keyboard and visible touch action stop bubbling into class actions",async()=>{
 const h=harness({navigation:true});await h.controller.open({seed:"A"});
 const [field,method]=memberRows(h);
 for(const [target,type,details] of [[field,"contextmenu",{clientX:88,clientY:99}],
  [method.querySelector(".classes-member-name"),"keydown",{key:"F10",shiftKey:true}],
  [field.querySelector(".classes-member-type"),"keydown",{key:"ContextMenu"}],
  [method.querySelector(".classes-member-menu"),"click",{pointerType:"touch"}]]){
  const event=await target.fire(type,details);assert.equal(event.prevented,true);assert.equal(event.stopped,true);assert.equal(h.menu(),null);
 }
 assert.equal(h.navigations.length,4);assert.equal(h.navigations[0].event.clientX,88);
 assert.equal(h.navigations[3].event.currentTarget,method.querySelector(".classes-member-menu"));
 assert.equal(h.methods.length,0);assert.equal(h.reads.length,0);
 await field.querySelector(".classes-member-type").fire("keydown",{key:"ArrowDown"});assert.equal(h.navigations.length,4);
 await h.card("A").fire("contextmenu");assert.ok(h.menu());assert.equal(h.navigations.length,4);
});
test("shared member menu preserves focus and safe labels without nested buttons",async()=>{
 const h=harness({navigation:true}),data=diagram();data.nodes[0].class.fields[0].typeHint="<img src=x onerror=bad()>";
 data.nodes[0].class.methods[0].name="<script>运行</script>";h.setRequest(async()=>freeze(data));await h.controller.open({seed:"A"});
 let run=0;h.setNavigate((event,selector,{isCurrent})=>h.controller.showContextMenu(event,[{label:`Declared type ${data.nodes[0].class.fields[0].typeHint}`,run:()=>{if(isCurrent())run++;}}]));
 const anchor=memberRows(h)[0].querySelector(".classes-member-type");anchor.focus();await anchor.fire("click");
 assert.match(text(h.menu()),/<img src=x onerror=bad\(\)>/);
 assert.equal(descendants(h.document.body).some(n=>n.tagName==="script"||n.tagName==="img"),false);
 for(const button of descendants(h.card("A")).filter(n=>n.tagName==="button"))assert.equal(descendants(button).slice(1).some(n=>n.tagName==="button"),false);
 await h.menu().fire("keydown",{key:"Escape"});assert.equal(h.document.activeElement,anchor);assert.equal(run,0);
 await anchor.fire("click");await h.menu().children[0].fire("click");assert.equal(run,1);assert.equal(h.document.activeElement,anchor);assert.equal(anchor.getAttribute("aria-expanded"),"false");
 assert.equal(h.calls.length,1);assert.equal(h.reads.length,0);
});
for(const change of ["session","revision","reset","hidden","fold","rerender","new diagram","pending diagram"])test(`member scope rejects stale ${change} and old triggers`,async()=>{
 const h=harness({navigation:true});await h.controller.open({seed:"A"});
 const trigger=memberRows(h)[0].querySelector(".classes-member-type");await trigger.fire("click");const {isCurrent}=h.navigations[0].scope;assert.equal(isCurrent(),true);
 let work,pending;
 if(change==="session")h.setSession("two");
 if(change==="revision")h.setRevision({indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:2});
 if(change==="reset")h.controller.reset();
 if(change==="hidden")h.get("classes-panel").hidden=true;
 if(change==="fold")await toggleMembers(h);
 if(change==="rerender")await allReturned(h);
 if(change==="new diagram")await h.controller.open({seed:"A"});
 if(change==="pending diagram"){pending=deferred();h.setRequest(()=>pending.promise);work=h.controller.open({seed:"B"});}
 assert.equal(isCurrent(),false);await trigger.fire("click");assert.equal(h.navigations.length,1);assert.equal(h.reads.length,0);
 if(pending){pending.resolve(diagram(["B"]));await work;assert.equal(isCurrent(),false);}
});
test("recoverable requests cannot revive a captured navigation ticket; fresh member intents can retry",async()=>{
 const h=harness({navigation:true});await h.controller.open({seed:"A"});
 const trigger=memberRows(h)[0].querySelector(".classes-member-type");await trigger.fire("click");const {isCurrent}=h.navigations[0].scope;
 h.setRequest(async()=>{const error=Error("Temporary failure");error.status=400;throw error;});await h.controller.open({seed:"B"});
 assert.equal(isCurrent(),false);await trigger.fire("click");assert.equal(h.navigations.length,2);assert.equal(h.navigations[1].scope.isCurrent(),true);
 assert.equal(h.reads.length,0);
});
test("delayed shared navigation work cannot publish or act after panel changes",async()=>{
 const h=harness({navigation:true});await h.controller.open({seed:"A"});
 const pending=deferred();let published=0;
 h.setNavigate(async(event,selector,{isCurrent})=>{await pending.promise;if(isCurrent())published++;});
 const work=memberRows(h)[0].querySelector(".classes-member-type").fire("click");
 h.get("classes-panel").hidden=true;pending.resolve();await work;assert.equal(published,0);assert.equal(h.menu(),null);assert.equal(h.reads.length,0);
});


const menuEvent = anchor => ({type:"keydown",currentTarget:anchor,target:anchor,preventDefault(){},stopPropagation(){}});
test("shared menu reports replacement, action and dismissal once and preserves action focus order",async()=>{
 const h=harness(),anchor=h.get("lifecycle-anchor"),events=[];
 anchor.setAttribute("aria-expanded","true");
 const first=h.controller.showContextMenu(menuEvent(anchor),[{label:"First",run(){}}],{onClose:reason=>events.push(`first:${reason}`)});
 h.controller.showContextMenu(menuEvent(anchor),[{label:"Second",run(){events.push(`run:${h.document.activeElement===anchor}:${h.menu()===null}`);}}],{onClose:reason=>events.push(`second:${reason}`)});
 assert.deepEqual(events,["first:replace"]);first.close();assert.ok(h.menu());
 await h.button(h.menu(),"Second").fire("click");
 assert.deepEqual(events,["first:replace","second:action","run:true:true"]);assert.equal(anchor.getAttribute("aria-expanded"),"true");
 const last=h.controller.showContextMenu(menuEvent(anchor),[{label:"Last",run(){}}],{onClose:reason=>events.push(`last:${reason}`)});
 last.close();last.close();h.controller.closeContextMenu();
 assert.deepEqual(events,["first:replace","second:action","run:true:true","last:dismiss"]);
});
test("shared menu clears lifecycle callback before reentrant close and ignores retained menu actions",async()=>{
 const h=harness(),anchor=h.get("lifecycle-anchor");let closed=0,runs=0;
 h.controller.showContextMenu(menuEvent(anchor),[{label:"Old",run(){runs++;}}],{onClose(){closed++;h.controller.closeContextMenu();}});
 const oldMenu=h.menu(),oldAction=oldMenu.children[0];
 h.controller.showContextMenu(menuEvent(anchor),[{label:"New",run(){}}]);assert.equal(closed,1);
 await oldAction.fire("click");await oldMenu.fire("keydown",{key:"Escape"});
 assert.equal(runs,0);assert.equal(h.menu().children[0].textContent,"New");assert.equal(closed,1);
});
for(const dismissal of ["focus","escape","outside","scroll","reset"])test(`shared menu ${dismissal} dismissal notifies its owner`,async()=>{
 const h=harness(),anchor=h.get("lifecycle-anchor"),reasons=[];
 h.controller.showContextMenu(menuEvent(anchor),[{label:"Active",run(){}}],{onClose:reason=>reasons.push(reason)});
 if(dismissal==="focus")h.get("other-anchor").focus();
 if(dismissal==="escape")await h.menu().fire("keydown",{key:"Escape"});
 if(dismissal==="outside")await h.event("pointerdown",h.get("other-anchor"));
 if(dismissal==="scroll"){anchor.scrollTop++;await h.event("scroll",anchor);}
 if(dismissal==="reset")h.controller.reset();
 assert.equal(h.menu(),null);assert.deepEqual(reasons,["dismiss"]);
});
function integratedNavigation() {
 const h=harness({navigation:true}),navigationSource=fs.readFileSync(path.join(__dirname,"../web/navigation.js"),"utf8");
 vm.runInNewContext(navigationSource,{window:h.window,document:h.document,console});
 const nav=h.window.TrellisNavigation,selected=[];
 let request=async()=>({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},targets:[{symbol:{id:"A.run",name:"run",path:"A.java",range},action:"sequence",reason:"declaration",matchKind:"measured"}],warnings:[]});
 nav.init({currentRevision:()=>({indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1}),currentSession:()=>"one",request:(...args)=>request(...args),
  showMenu:(event,actions,options)=>h.controller.showContextMenu(event,actions,options),
  selectMethod:symbol=>selected.push(symbol),openClass:symbol=>selected.push(symbol)});
 h.setNavigate((...args)=>nav.open(...args));
 return {...h,nav,selected,setNavigationRequest(fn){request=fn;}};
}
for(const replacement of [false,true])test(`actual shared navigation never reopens after focus dismissal${replacement?" and newer class menu":""}`,async()=>{
 const h=integratedNavigation(),pending=deferred();h.setNavigationRequest(()=>pending.promise);
 const anchor=h.get("source-navigation-anchor"),other=h.get("other-class-anchor");
 const work=h.nav.open(menuEvent(anchor),{path:"A.java",line:1});assert.match(text(h.menu()),/Finding cached/);
 other.focus();assert.equal(h.menu(),null);
 if(replacement)h.controller.showContextMenu(menuEvent(other),[{label:"Other class menu",run(){}}]);
 pending.resolve({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},targets:[],warnings:[]});await work;
 if(replacement){assert.equal(h.menu().children[0].textContent,"Other class menu");assert.equal(h.document.activeElement,h.menu().children[0]);}
 else{assert.equal(h.menu(),null);assert.equal(h.document.activeElement,other);}
 assert.equal(h.selected.length,0);assert.equal(h.reads.length,0);
});
test("shared navigation does not select an inferred class relation or call",async()=>{
 const h=integratedNavigation();await h.controller.open({seed:"A"});
 const trigger=memberRows(h)[0].querySelector(".classes-member-type");await trigger.fire("click");
 assert.equal(h.selected.length,0);assert.equal(h.reads.length,0);
 assert.ok(h.menu());
 h.nav.reset();assert.equal(h.menu(),null);
});

test("actual navigation reset closes its menu but cannot close a replacement class menu",async()=>{
 const h=integratedNavigation(),anchor=h.get("source-navigation-anchor");
 await h.nav.open(menuEvent(anchor),{path:"A.java",line:1});assert.ok(h.menu());h.nav.reset();assert.equal(h.menu(),null);
 await h.nav.open(menuEvent(anchor),{path:"A.java",line:1});
 h.controller.showContextMenu(menuEvent(h.get("other-class-anchor")),[{label:"Other class menu",run(){}}]);
 h.nav.reset();assert.equal(h.menu().children[0].textContent,"Other class menu");
});

test("class results cannot cross generation with the same revision",async()=>{
  const h=harness(),gate=deferred();h.setRequest(()=>gate.promise);
  const work=h.controller.open({seed:'A'});
  h.setRevision({indexGeneration:'87654321-4321-4321-8321-abcdef123456',indexRevision:1});
  gate.resolve(diagram());await work;
  assert.equal(h.card('A'),undefined);
});

test("class search, page, diagram, and expansion reject a reused revision and ask for status refresh",async()=>{
 const old={indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1};
 const next={indexGeneration:'87654321-4321-4321-8321-abcdef123456',indexRevision:1};
 for(const action of ['search','page','diagram','expansion']) {
   const h=harness();
   if(action==='expansion') await h.controller.open({seed:'A'});
   h.setRequest((url,options)=>options ? diagram(['A'],next) : {revision:next,items:[definition('A')],nextOffset:1});
   if(action==='search') await h.controller.open({});
   if(action==='page') { h.setRequest((url,options)=>options ? diagram() : {revision:old,items:[definition('A')],nextOffset:100}); await h.controller.open({}); h.setRequest(()=>({revision:next,items:[],nextOffset:null})); await h.button(h.get('classes-results'),'More classes').fire('click'); }
   if(action==='diagram') await h.controller.open({seed:'A'});
   if(action==='expansion') {h.get('classes-unmatched').checked=true;await h.get('classes-unmatched').fire('change');}
   const call=h.calls.at(-1);
   if(call.options) assert.deepEqual(JSON.parse(JSON.stringify(call.options.body.expectedRevision)),old,action);
   else {const params=new URL(call.url,'http://local').searchParams;
     assert.equal(params.get('indexGeneration'),old.indexGeneration,action);
     assert.equal(params.get('indexRevision'),'1',action);
   }
   assert.equal(h.stale.length,1,action);
   assert.equal(h.card('A'),undefined,action);
   assert.match(h.get('classes-state').textContent,/revision changed/i,action);
 }
});

test("future class DTO without edge collection still selects a measured declaration",async()=>{
 const h=harness(),data=diagram(["A"]);delete data.edges;
 h.setRequest(async()=>data);await h.controller.open({seed:"A"});
 assert.ok(h.card("A"));
 assert.equal(descendants(h.get("classes-diagram")).filter(n=>n.className==="classes-edge").length,0);
 assert.match(text(h.get("classes-state")),/measured declarations/);
 assert.equal(h.reads.length,0);
});

test("terminal class list keeps the 24-declaration cap without relation expansion",async()=>{
 const h=harness(), ids=["A",...Array.from({length:39},(_,i)=>`C${i}`)],data=diagram(ids);
 h.setRequest(async()=>data);await h.controller.open({seed:"A"});
 assert.equal(descendants(h.get("classes-diagram")).filter(n=>n.dataset.classId).length,1);
 await allReturned(h);
 assert.equal(descendants(h.get("classes-diagram")).filter(n=>n.dataset.classId).length,24);
 assert.equal(descendants(h.get("classes-diagram")).filter(n=>n.className==="classes-edge").length,0);
 assert.equal(h.calls[0].options.body.includeHierarchy,false);
 assert.equal(h.reads.length,0);
});

test("stale terminal class response cannot replace a newer selected declaration",async()=>{
 const h=harness(), first=deferred();h.setRequest(async()=>first.promise);
 const old=h.controller.open({seed:"A"});
 h.setRequest(async()=>diagram(["B","A"]));await h.controller.open({seed:"B"});
 first.resolve(diagram(["A","B"]));await old;
 assert.ok(h.card("B"));assert.equal(h.card("A"),undefined);
 assert.equal(h.reads.length,0);
});
