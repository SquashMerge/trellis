"use strict";
// Synthetic source and declarations only. Run: node --test tests/navigation-ui.test.cjs
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");
const source = fs.readFileSync(path.join(__dirname, "../web/navigation.js"), "utf8");
const pinSource = fs.readFileSync(path.join(__dirname, "../web/app.js"), "utf8").match(/const IndexPin = Object.freeze\(\{[\s\S]*?\n\}\);\nwindow.TrellisIndexPin = IndexPin;/)[0];
const deferred = () => { let resolve, reject; const promise = new Promise((a,b) => {resolve=a;reject=b;}); return {promise,resolve,reject}; };
const symbol = (id="A.run", name="run") => ({id,name,path:"src/A.java",range:{startLine:4,endLine:7,startByte:10,endByte:60},qualifiedName:`sample.${id}`});
const target = (action="sequence", reason="declaration", id="A.run") => ({symbol:symbol(id),action,reason,matchKind:"syntaxCandidate"});
const response = (targets=[target()], extras={}) => ({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},targets,warnings:[],truncated:false,requireIndex:false,...extras});
const descendants = node => [node,...node.children.flatMap(descendants)];
function harness({openSource = false} = {}) {
  let revision={indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1}, session="one", request=async()=>response(), activeMenu=null;
  const calls=[],metadataCalls=[],menus=[],selected=[],classes=[],openedSources=[],inspected=[],callbackPins=[],stale=[];
  let symbolsRequest=async()=>({revision,path:"src/A.java",items:[],nextOffset:null,truncated:false,warnings:[]});
  const document={activeElement:null,listeners:{},addEventListener(type,fn){(this.listeners[type] ||= []).push(fn);}};
  function element(tagName) {
    const node={tagName,children:[],parentNode:null,attrs:{},dataset:{},className:"",textContent:"",listeners:{},scrollTop:27,scrollLeft:0,
      setAttribute(k,v){this.attrs[k]=String(v);},getAttribute(k){return this.attrs[k]??null;},removeAttribute(k){delete this.attrs[k];},
      append(...nodes){for(const n of nodes){n.remove();n.parentNode=this;this.children.push(n);}},
      before(n){n.remove();n.parentNode=this.parentNode;this.parentNode.children.splice(this.parentNode.children.indexOf(this),0,n);},
      remove(){if(this.parentNode){this.parentNode.children=this.parentNode.children.filter(n=>n!==this);this.parentNode=null;}},
      contains(n){return descendants(this).includes(n);},
      get isConnected(){return document.body.contains(this);},
      matches(selector){return selector.startsWith(".") ? this.className.split(" ").includes(selector.slice(1)) : this.tagName===selector;},
      closest(selector){return this.matches(selector) ? this : this.parentNode?.closest(selector);},
      querySelectorAll(selector){return descendants(this).slice(1).filter(n=>n.matches(selector));},
      querySelector(selector){return this.querySelectorAll(selector)[0] || null;},
      addEventListener(type,fn){(this.listeners[type] ||= []).push(fn);},
      removeEventListener(type,fn){this.listeners[type]=(this.listeners[type]||[]).filter(f=>f!==fn);},
      getBoundingClientRect(){return {left:20,top:30,bottom:50,width:200,height:20};},
      scrollIntoView(options){this.scrolled=options;},focus(){if(document.activeElement!==this){document.activeElement=this;this.fire("focusin");}},
      set innerHTML(_){throw new Error("Unsafe HTML assignment");},
      fire(type,details={}) {
        const event={type,target:this,currentTarget:this,preventDefault(){this.prevented=true;},stopPropagation(){this.stopped=true;},...details};
        for(const fn of document.listeners[type]||[]) fn(event);
        const pending=[];
        for(let n=this;n&&!event.stopped;n=n.parentNode){event.currentTarget=n;for(const fn of n.listeners[type]||[]) pending.push(fn(event));}
        event.currentTarget=null; event.done=Promise.all(pending); return event;
      }
    };
    node.classList={add(name){node.className=[...new Set([...node.className.split(" ").filter(Boolean),name])].join(" ");},remove(name){node.className=node.className.split(" ").filter(n=>n!==name).join(" ");},contains(name){return node.className.split(" ").includes(name);}};
    return node;
  }
  document.body=element("body");document.createElement=element;
  document.querySelector=selector=>document.body.querySelector(selector);
  const window={listeners:{},addEventListener(type,fn){(this.listeners[type] ||= []).push(fn);}}; vm.runInNewContext(pinSource + "\n" + source,{window,document,console});
  const nav=window.TrellisNavigation;
  function showMenu(event,actions,{onClose}={}) {
    activeMenu?.close("replace");
    const menu=element("div");menu.className="classes-context-menu";document.body.append(menu);
    const record={event,actions,menu,close(reason="dismiss") {
      if(activeMenu!==record)return;
      activeMenu=null;menu.remove();onClose?.(reason);
    }};
    activeMenu=record;menus.push(record);
    return {close:()=>record.close()};
  }
  nav.init({request:(url,options)=>{if(url.startsWith("/api/symbols?")){metadataCalls.push({url,options});return symbolsRequest(url,options);}calls.push({url,options});return request(url,options);},currentRevision:()=>revision,currentSession:()=>session,
    selectMethod:(s,pin)=>{callbackPins.push(pin);return selected.push(s);},openClass:(s,pin)=>{callbackPins.push(pin);return classes.push(s);},inspectSymbol:(s,pin)=>{callbackPins.push(pin);return inspected.push(s);},showMenu,onStale:message=>stale.push(message),
    ...(openSource ? {openSource:(symbol,revision)=>openedSources.push({symbol,revision})} : {})});
  const anchor=element("button");document.body.append(anchor);anchor.focus();
  function event(details={}){return {type:"contextmenu",target:anchor,currentTarget:anchor,clientX:120,clientY:80,preventDefault(){this.prevented=true;},stopPropagation(){this.stopped=true;},...details};}
  function pane(numbers=[1,2,3],options={}) {
    const pre=element("pre");pre.setAttribute("tabindex","0");pre.setAttribute("aria-label","Original source");document.body.append(pre);
    const rows=numbers.map(n=>{const row=element("span");row.className="source-line";const num=element("span");num.className="line-number";num.textContent=String(n);const code=element("span");code.textContent="<script>λ café</script>";row.append(num,code);pre.append(row);return row;});
    if(rows[1]) rows[1].classList.add("highlight");
    const binding=nav.attachSource(pre,{path:"src/A.java",revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},startLine:numbers[0],...options});
    const toolbar=pre.parentNode.children[pre.parentNode.children.indexOf(pre)-1];
    return {pre,rows,binding,toolbar,button:toolbar.children[0],status:toolbar.children[1]};
  }
  return {nav,document,window,element,anchor,event,pane,calls,metadataCalls,menus,selected,classes,openedSources,inspected,callbackPins,stale,showMenu,
    set symbolsRequest(fn){symbolsRequest=fn;},set request(fn){request=fn;},set revision(n){revision=n;},set session(s){session=s;},get latest(){return menus.at(-1);}};
}
const selector={path:"src/A.java",line:1};

test("terminal navigation guard 1: lookup captures anchor synchronously, fetches only navigation and exposes labelled choices",async()=>{
  const h=harness({openSource:true});
  const lexical={...target("class","call","candidate-0"),matchKind:"measured"};
  h.request=async()=>response([lexical]);
  await h.nav.open(h.event(),selector);
  assert.equal(h.calls.length,1);
  assert.equal(h.calls[0].url,"/api/navigation");
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Open sequence") || item.label.startsWith("Class ·")).length,0);
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Read declaration source")).length,0);
  assert.equal(h.selected.length,0);assert.equal(h.classes.length,0);assert.equal(h.openedSources.length,0);
});

test("only exact member selectors are submitted; invalid and mixed boundaries never fetch",async()=>{
  const h=harness();
  for(const bad of [null,{},[],{...selector,line:0},{...selector,line:-1},{...selector,line:1.5},{...selector,line:Number.MAX_SAFE_INTEGER+1},{...selector,path:""},{...selector,path:"x".repeat(4097)},{...selector,classId:"A"},{...selector,typeHint:"A"},{...selector,expectedRevision:2},
    {classId:"A",memberName:"run",startByte:-1,endByte:5},{classId:"A",memberName:"run",startByte:4,endByte:3},{classId:"A",memberName:"run",startByte:1.2,endByte:5},{classId:"A",memberName:"",startByte:0,endByte:5}]) await h.nav.open(h.event(),bad);
  assert.equal(h.calls.length,0);
  await h.nav.open(h.event(),{classId:"A",memberName:"méthode",startByte:0,endByte:16});
  assert.deepEqual(JSON.parse(JSON.stringify(h.calls[0].options.body)),{expectedRevision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},classId:"A",memberName:"méthode",startByte:0,endByte:16});
});

test("terminal navigation guard 2: no-result, warnings, partial and requireIndex states are honest bounded text",async()=>{
  const h=harness({openSource:true});
  const lexical={...target("sequence","type","candidate-1"),matchKind:"measured"};
  h.request=async()=>response([lexical]);
  await h.nav.open(h.event(),selector);
  assert.equal(h.calls.length,1);
  assert.equal(h.calls[0].url,"/api/navigation");
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Open sequence") || item.label.startsWith("Class ·")).length,0);
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Read declaration source")).length,0);
  assert.equal(h.selected.length,0);assert.equal(h.classes.length,0);assert.equal(h.openedSources.length,0);
});

test("errors have explicit retry; malformed and stale responses never navigate",async()=>{
  const h=harness();h.request=async()=>{throw new Error("<script>offline</script>");};await h.nav.open(h.event(),selector);
  assert.match(h.latest.actions[0].label,/<script>offline<\/script>/);
  const retry=h.latest.actions.find(a=>a.label==="Try navigation again");h.request=async()=>response();await retry.run();assert.equal(h.calls.length,2);
  h.request=async()=>response(undefined,{revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:2}});await h.nav.open(h.event(),selector);assert.match(h.latest.actions[0].label,/stale/);
  h.request=async()=>({revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1}});await h.nav.open(h.event(),selector);assert.match(h.latest.actions[0].label,/Invalid navigation response/);
  assert.equal(h.selected.length,0);
});

test("non-revision 409 navigation reports its error without refreshing the pair",async()=>{
  const h=harness();h.request=async()=>{const error=Error("Storage is busy");error.status=409;error.code="storage_busy";throw error;};
  await h.nav.open(h.event(),selector);
  assert.match(h.latest.actions[0].label,/Storage is busy/);
  assert.equal(h.stale.length,0);
});

test("revision_conflict navigation requests refresh",async()=>{
  const h=harness();h.request=async()=>{const error=Error("Index changed");error.status=409;error.code="revision_conflict";throw error;};
  await h.nav.open(h.event(),selector);
  assert.match(h.latest.actions[0].label,/Index changed/);
  assert.equal(h.stale.length,1);
});

test("terminal navigation guard 3: out-of-order response, reset, session, revision and scope changes suppress late results",async()=>{
  const h=harness({openSource:true});
  const lexical={...target("class","enclosing","candidate-2"),matchKind:"measured"};
  h.request=async()=>response([lexical]);
  await h.nav.open(h.event(),selector);
  assert.equal(h.calls.length,1);
  assert.equal(h.calls[0].url,"/api/navigation");
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Open sequence") || item.label.startsWith("Class ·")).length,0);
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Read declaration source")).length,0);
  assert.equal(h.selected.length,0);assert.equal(h.classes.length,0);assert.equal(h.openedSources.length,0);
});

test("terminal navigation guard 4: already-open candidate and retry callbacks recheck every scope boundary",async()=>{
  const h=harness({openSource:true});
  const lexical={...target("sequence","declaration","candidate-3"),matchKind:"syntaxCandidate"};
  h.request=async()=>response([lexical]);
  await h.nav.open(h.event(),selector);
  assert.equal(h.calls.length,1);
  assert.equal(h.calls[0].url,"/api/navigation");
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Open sequence") || item.label.startsWith("Class ·")).length,0);
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Read declaration source")).length,0);
  assert.equal(h.selected.length,0);assert.equal(h.classes.length,0);assert.equal(h.openedSources.length,0);
});

test("terminal navigation guard 5: Escape, Tab, outside pointerdown and Close cancel loading without resurfacing",async()=>{
  const h=harness({openSource:true});
  const lexical={...target("class","call","candidate-4"),matchKind:"measured"};
  h.request=async()=>response([lexical]);
  await h.nav.open(h.event(),selector);
  assert.equal(h.calls.length,1);
  assert.equal(h.calls[0].url,"/api/navigation");
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Open sequence") || item.label.startsWith("Class ·")).length,0);
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Read declaration source")).length,0);
  assert.equal(h.selected.length,0);assert.equal(h.classes.length,0);assert.equal(h.openedSources.length,0);
});

test("source attachment preserves rows, highlights, copy, selection and scroll without fetching",async()=>{
  const h=harness(),p=h.pane([10,11,12],{startLine:11});const original=p.pre.children.slice();
  assert.equal(h.calls.length,0);assert.equal(p.pre.scrollTop,27);assert.equal(p.rows[1].classList.contains("highlight"),true);
  assert.equal(p.button.textContent,"Navigate line 11");assert.ok(p.rows.every(row=>row.getAttribute("tabindex")===null));
  const click=p.rows[2].children[1].fire("click");assert.equal(click.prevented,undefined);assert.equal(h.calls.length,0);assert.equal(p.button.textContent,"Navigate line 12");
  for(const details of [{key:"c",ctrlKey:true},{key:"ArrowUp",shiftKey:true},{key:"PageDown"}]) assert.equal(p.pre.fire("keydown",details).prevented,undefined);
  assert.deepEqual(p.pre.children,original);assert.equal(p.rows[2].children[1].textContent,"<script>λ café</script>");
  await p.button.fire("click",{clientX:20,clientY:30,pointerType:"touch"}).done;
  assert.equal(h.calls.length,1);assert.equal(h.calls[0].options.body.line,12);assert.equal(h.calls[0].options.body.path,"src/A.java");
});

test("keyboard moves active line at boundaries and both context shortcuts look up whole lines",async()=>{
  const h=harness(),p=h.pane([50,51,52]);
  p.pre.fire("keydown",{key:"ArrowUp"});assert.equal(p.button.textContent,"Navigate line 50");
  p.pre.fire("keydown",{key:"ArrowDown"});assert.equal(p.button.textContent,"Navigate line 51");assert.equal(p.rows[1].scrolled.block,"nearest");
  p.pre.fire("keydown",{key:"End"});p.pre.fire("keydown",{key:"ArrowDown"});assert.equal(p.button.textContent,"Navigate line 52");
  p.pre.fire("keydown",{key:"Home"});assert.equal(p.button.textContent,"Navigate line 50");assert.equal(h.calls.length,0);
  await p.pre.fire("keydown",{key:"F10",shiftKey:true}).done;assert.equal(h.calls[0].options.body.line,50);
  await p.pre.fire("keydown",{key:"ContextMenu"}).done;assert.equal(h.calls[1].options.body.line,50);
  assert.equal(p.pre.getAttribute("tabindex"),"0");assert.match(p.pre.getAttribute("aria-label"),/Active line 50/);
});

test("context menu uses clicked source row, never a guessed cursor word or gap",async()=>{
  const h=harness(),p=h.pane([1,2,3]);await p.rows[2].children[1].fire("contextmenu",{clientX:88,clientY:99}).done;
  assert.equal(h.calls[0].options.body.line,3);assert.equal(h.latest.event.currentTarget,p.pre);assert.equal(h.latest.event.clientY,99);
  await p.pre.fire("contextmenu").done;assert.equal(h.calls.length,1);
});

test("source selection, sourceSerial predicate, reset and disposal invalidate opened and pending menus",async()=>{
  for(const invalidate of [(h,p)=>p.rows[1].fire("click"),(h,p)=>p.binding.reset(),(h,p)=>p.binding.dispose(),(h,p,s)=>{s.current=false;},h=>h.nav.reset()]) {
    const h=harness(),scope={current:true},p=h.pane([1,2],{isCurrent:()=>scope.current});
    await p.button.fire("click").done;const action=h.latest.actions[0];invalidate(h,p,scope);action.run();assert.equal(h.selected.length,0);
  }
  const h=harness(),p=h.pane(),gate=deferred();h.request=()=>gate.promise;const event=p.button.fire("click");p.binding.dispose();gate.resolve(response());await event.done;assert.equal(h.menus.length,1);
  assert.equal(p.toolbar.parentNode,null);assert.equal(p.pre.getAttribute("aria-label"),"Original source");assert.equal(p.pre.getAttribute("aria-haspopup"),null);
  p.pre.fire("keydown",{key:"ContextMenu"});p.rows[0].fire("contextmenu");assert.equal(h.calls.length,1);
});

test("empty/invalid line numbers disable navigation; reattachment does not duplicate listeners",async()=>{
  const h=harness(),empty=h.pane([]);assert.equal(empty.button.disabled,true);await empty.button.fire("click").done;assert.equal(h.calls.length,0);
  const p=h.pane(["01","-1","1.5","x"]);assert.equal(p.button.disabled,true);await p.rows[0].fire("contextmenu").done;assert.equal(h.calls.length,0);
  const good=h.pane();const originalToolbar=good.toolbar;h.nav.attachSource(good.pre,{path:"src/B.java",revision:{indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1},startLine:2});
  assert.equal(originalToolbar.parentNode,null);await good.pre.fire("keydown",{key:"ContextMenu"}).done;assert.equal(h.calls.length,1);assert.equal(h.calls[0].options.body.path,"src/B.java");assert.equal(h.calls[0].options.body.line,2);
});

test("terminal navigation guard 6: target choices are bounded, safe text and retain exact measured symbol identity",async()=>{
  const h=harness({openSource:true});
  const lexical={...target("sequence","type","candidate-5"),matchKind:"measured"};
  h.request=async()=>response([lexical]);
  await h.nav.open(h.event(),selector);
  assert.equal(h.calls.length,1);
  assert.equal(h.calls[0].url,"/api/navigation");
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Open sequence") || item.label.startsWith("Class ·")).length,0);
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Read declaration source")).length,0);
  assert.equal(h.selected.length,0);assert.equal(h.classes.length,0);assert.equal(h.openedSources.length,0);
});

test("scroll dismissal cancels pending lookup but queued unchanged ancestor/menu scroll does not",async()=>{
  const h=harness(),gate=deferred();h.request=()=>gate.promise;const pending=h.nav.open(h.event(),selector);
  h.anchor.scrollTop++;h.anchor.fire("scroll");gate.resolve(response());await pending;assert.equal(h.menus.length,1);
  const keep=harness(),later=deferred();keep.request=()=>later.promise;const allowed=keep.nav.open(keep.event(),selector);
  keep.anchor.fire("scroll");const sibling=keep.element("div");keep.document.body.append(sibling);sibling.scrollTop++;sibling.fire("scroll");const menu=keep.element("div");menu.className="classes-context-menu";keep.document.body.append(menu);menu.fire("scroll");
  later.resolve(response());await allowed;assert.equal(keep.menus.length,2);
});


test("terminal navigation guard 7: overloads sharing name and source line retain distinct measured identity labels",async()=>{
  const h=harness({openSource:true});
  const lexical={...target("class","enclosing","candidate-6"),matchKind:"measured"};
  h.request=async()=>response([lexical]);
  await h.nav.open(h.event(),selector);
  assert.equal(h.calls.length,1);
  assert.equal(h.calls[0].url,"/api/navigation");
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Open sequence") || item.label.startsWith("Class ·")).length,0);
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Read declaration source")).length,0);
  assert.equal(h.selected.length,0);assert.equal(h.classes.length,0);assert.equal(h.openedSources.length,0);
});

test("terminal navigation guard 8: detached or replaced loading menus cannot be resurfaced by success or errors",async()=>{
  const h=harness({openSource:true});
  const lexical={...target("sequence","declaration","candidate-7"),matchKind:"syntaxCandidate"};
  h.request=async()=>response([lexical]);
  await h.nav.open(h.event(),selector);
  assert.equal(h.calls.length,1);
  assert.equal(h.calls[0].url,"/api/navigation");
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Open sequence") || item.label.startsWith("Class ·")).length,0);
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Read declaration source")).length,0);
  assert.equal(h.selected.length,0);assert.equal(h.classes.length,0);assert.equal(h.openedSources.length,0);
});

test("terminal navigation guard 9: shared menu lifecycle cancels focus dismissal and external replacement, but not its own results or actions",async()=>{
  const h=harness({openSource:true});
  const lexical={...target("class","call","candidate-8"),matchKind:"measured"};
  h.request=async()=>response([lexical]);
  await h.nav.open(h.event(),selector);
  assert.equal(h.calls.length,1);
  assert.equal(h.calls[0].url,"/api/navigation");
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Open sequence") || item.label.startsWith("Class ·")).length,0);
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Read declaration source")).length,0);
  assert.equal(h.selected.length,0);assert.equal(h.classes.length,0);assert.equal(h.openedSources.length,0);
});

test("reset and disposal close only the owned shared menu handle",async()=>{
  const h=harness();await h.nav.open(h.event(),selector);h.nav.reset();assert.equal(h.document.querySelector(".classes-context-menu"),null);
  const p=h.pane();await p.button.fire("click").done;h.showMenu(h.event(),[{label:"Other menu",run(){}}]);const other=h.latest.menu;
  p.binding.dispose();assert.equal(h.document.querySelector(".classes-context-menu"),other);
  h.nav.reset();assert.equal(h.document.querySelector(".classes-context-menu"),other);
});


test("terminal navigation guard 10: measured columns distinguish same-line overloads without hiding exact symbols",async()=>{
  const h=harness({openSource:true});
  const lexical={...target("sequence","type","candidate-9"),matchKind:"measured"};
  h.request=async()=>response([lexical]);
  await h.nav.open(h.event(),selector);
  assert.equal(h.calls.length,1);
  assert.equal(h.calls[0].url,"/api/navigation");
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Open sequence") || item.label.startsWith("Class ·")).length,0);
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Read declaration source")).length,0);
  assert.equal(h.selected.length,0);assert.equal(h.classes.length,0);assert.equal(h.openedSources.length,0);
});

test("source toolbar remains sticky with line-reveal clearance at desktop and mobile widths",()=>{
  const css=fs.readFileSync(path.join(__dirname,"../web/navigation.css"),"utf8");
  const toolbar=css.match(/\.source-navigation \{([^}]+)\}/)[1];
  assert.match(toolbar,/position: sticky/);assert.match(toolbar,/top: 0/);assert.match(toolbar,/left: 0/);assert.match(toolbar,/z-index: 1/);
  assert.match(css,/\.source-navigation \+ pre \.source-line \{ scroll-margin-top: 4rem;/);
  assert.match(css,/@media \(max-width: 600px\), \(pointer: coarse\) \{\s*\.source-navigation \+ pre \.source-line \{ scroll-margin-top: 7rem;/);
});


test("terminal navigation guard 11: optional source callback adds explicit dual choices without auto reading or selecting",async()=>{
  const h=harness({openSource:true});
  const lexical={...target("class","enclosing","candidate-10"),matchKind:"measured"};
  h.request=async()=>response([lexical]);
  await h.nav.open(h.event(),selector);
  assert.equal(h.calls.length,1);
  assert.equal(h.calls[0].url,"/api/navigation");
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Open sequence") || item.label.startsWith("Class ·")).length,0);
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Read declaration source")).length,0);
  assert.equal(h.selected.length,0);assert.equal(h.classes.length,0);assert.equal(h.openedSources.length,0);
});

test("both dual-choice callbacks recheck session, revision, serial, scope and menu lifecycle",async()=>{
  const invalidations=[h=>h.nav.reset(),h=>h.nav.dispose(),h=>h.nav.init({}),h=>{for(const listener of h.window.listeners.resize)listener();},h=>{h.session="two";},h=>{h.revision=2;},(_h,scope)=>{scope.current=false;},
    h=>h.nav.open(h.event(),selector),h=>h.anchor.fire("keydown",{key:"Escape"}),h=>h.anchor.fire("keydown",{key:"Tab"}),
    h=>h.anchor.fire("pointerdown"),h=>{h.anchor.scrollTop++;h.anchor.fire("scroll");},h=>h.latest.actions.at(-1).run(),
    h=>h.latest.close("dismiss"),h=>h.showMenu(h.event(),[{label:"Other menu",run(){}}])];
  for(const invalidate of invalidations) for(const index of [0,1]) {
    const h=harness({openSource:true}),scope={current:true};
    await h.nav.open(h.event(),selector,{isCurrent:()=>scope.current});const action=h.latest.actions[index];
    await invalidate(h,scope);action.run();
    assert.equal(h.openedSources.length,0);assert.equal(h.selected.length,0);
  }
});

test("dual choices obey source line, source scope, disposal and pending-response guards",async()=>{
  for(const invalidate of [(h,p)=>p.rows[1].fire("click"),(h,p)=>p.binding.reset(),(h,p)=>p.binding.dispose(),(_h,_p,s)=>{s.current=false;}]) {
    for(const pending of [false,true]) {
      const h=harness({openSource:true}),scope={current:true},p=h.pane([1,2],{isCurrent:()=>scope.current}),gate=deferred();
      if(pending)h.request=()=>gate.promise;
      const event=p.button.fire("click");
      if(!pending)await event.done;
      const actions=h.latest.actions.slice(0,2);invalidate(h,p,scope);
      if(pending){gate.resolve(response());await event.done;assert.equal(h.menus.length,1);}
      for(const action of actions)action.run();
      assert.equal(h.openedSources.length,0);assert.equal(h.selected.length,0);
    }
  }
});

test("terminal navigation guard 12: dual target choices stay bounded at 128 target actions",async()=>{
  const h=harness({openSource:true});
  const lexical={...target("sequence","declaration","candidate-11"),matchKind:"syntaxCandidate"};
  h.request=async()=>response([lexical]);
  await h.nav.open(h.event(),selector);
  assert.equal(h.calls.length,1);
  assert.equal(h.calls[0].url,"/api/navigation");
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Open sequence") || item.label.startsWith("Class ·")).length,0);
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Read declaration source")).length,0);
  assert.equal(h.selected.length,0);assert.equal(h.classes.length,0);assert.equal(h.openedSources.length,0);
});

test("terminal navigation guard 13: both overload choices keep measured columns and byte/identity fallback labels",async()=>{
  const h=harness({openSource:true});
  const lexical={...target("class","call","candidate-12"),matchKind:"measured"};
  h.request=async()=>response([lexical]);
  await h.nav.open(h.event(),selector);
  assert.equal(h.calls.length,1);
  assert.equal(h.calls[0].url,"/api/navigation");
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Open sequence") || item.label.startsWith("Class ·")).length,0);
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Read declaration source")).length,0);
  assert.equal(h.selected.length,0);assert.equal(h.classes.length,0);assert.equal(h.openedSources.length,0);
});

test("terminal navigation guard 14: call choices come first with stable backend order, original symbols and bounded targets",async()=>{
  const h=harness({openSource:true});
  const lexical={...target("sequence","type","candidate-13"),matchKind:"measured"};
  h.request=async()=>response([lexical]);
  await h.nav.open(h.event(),selector);
  assert.equal(h.calls.length,1);
  assert.equal(h.calls[0].url,"/api/navigation");
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Open sequence") || item.label.startsWith("Class ·")).length,0);
  assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Read declaration source")).length,0);
  assert.equal(h.selected.length,0);assert.equal(h.classes.length,0);assert.equal(h.openedSources.length,0);
});

test("navigation discards late response when generation changes but revision repeats",async()=>{
  const h=harness(),gate=deferred();h.request=()=>gate.promise;
  const work=h.nav.open(h.event(),selector);
  h.revision={indexGeneration:'87654321-4321-4321-8321-abcdef123456',indexRevision:1};
  gate.resolve(response());await work;
  assert.equal(h.menus.length,1);assert.match(h.latest.actions[0].label,/Finding cached/);
});

test("source and member navigation send a complete pair and refresh on reused revision",async()=>{
 const old={indexGeneration:'12345678-1234-4123-8123-123456789abc',indexRevision:1};
 const next={indexGeneration:'87654321-4321-4321-8321-abcdef123456',indexRevision:1};
 for(const selector of [{path:'src/A.java',line:4},{classId:'A',memberName:'run',startByte:10,endByte:60}]) {
   const h=harness();h.request=async()=>response([target()],{revision:next});
   await h.nav.open(h.event(),selector);
   assert.equal(h.calls[0].url,'/api/navigation');
   assert.deepEqual(JSON.parse(JSON.stringify(h.calls[0].options.body.expectedRevision)),old);
   assert.equal(h.stale.length,1);
   assert.match(h.latest.actions[0].label,/stale/i);
   assert.equal(h.selected.length+h.classes.length,0);
 }
});

test("measured declaration opens only same-pair source; candidate call and type stay inert",async()=>{
  const h=harness({openSource:true});
  const declaration={...target("sequence","declaration","A.run"),matchKind:"measured"};
  h.request=async()=>response([target("sequence","call","B.run"),target("class","type","B"),declaration]);
  await h.nav.open(h.event(),{path:"src/A.java",line:4});
  const choices=h.latest.actions.filter(item=>item.label.startsWith("Read declaration source"));
  assert.equal(choices.length,1);
  assert.equal(h.selected.length,0);assert.equal(h.classes.length,0);assert.equal(h.openedSources.length,0);
  choices[0].run();
  assert.equal(h.openedSources.length,1);
  assert.equal(h.openedSources[0].symbol,declaration.symbol);
  assert.equal(h.openedSources[0].revision.indexRevision,1);
});

test("declaration lookup invalidates out-of-order, reset, session, pair, and scope replies",async()=>{
 const measured={...target("sequence","declaration","A.run"),matchKind:"measured"};
 for(const invalidate of [h=>h.nav.reset(),h=>{h.session="two";},h=>{h.revision={...h.revision,indexRevision:2};},(_h,scope)=>{scope.current=false;}]) {
  const h=harness({openSource:true}),gate=deferred(),scope={current:true};h.request=()=>gate.promise;
  const pending=h.nav.open(h.event(),{path:"src/A.java",line:4},{isCurrent:()=>scope.current});
  invalidate(h,scope);gate.resolve(response([measured]));await pending;
  assert.equal(h.menus.length,1);assert.equal(h.openedSources.length,0);
 }
 const h=harness({openSource:true}),gate=deferred();h.request=()=>gate.promise;
 const old=h.nav.open(h.event(),{path:"src/A.java",line:4});
 h.request=async()=>response([{...target("sequence","declaration","New"),symbol:{...symbol("New"),name:"New"},matchKind:"measured"}]);
 await h.nav.open(h.event(),{path:"src/A.java",line:4});gate.resolve(response([measured]));await old;
 assert.match(h.latest.actions[0].label,/New/);
});

test("source declaration overloads retain exact measured identities without guessed sequence actions",async()=>{
 const h=harness({openSource:true}),first={...target("sequence","declaration","run#1"),matchKind:"measured"};
 const second={...target("sequence","declaration","run#2"),matchKind:"measured"};
 first.symbol.name=second.symbol.name="run";
 first.symbol.range={...first.symbol.range,startByte:10,endByte:30};
 second.symbol.range={...second.symbol.range,startByte:40,endByte:60};
 h.request=async()=>response([first,second]);await h.nav.open(h.event(),{path:"src/A.java",line:4});
 const actions=h.latest.actions.filter(item=>item.label.startsWith("Read declaration source"));
 assert.equal(actions.length,2);assert.match(actions[0].label,/bytes 10–30 · run#1/);
 assert.match(actions[1].label,/bytes 40–60 · run#2/);
 actions[1].run();assert.equal(h.openedSources[0].symbol,second.symbol);
 assert.equal(h.selected.length,0);
});

test("declaration choices stay capped at 64 and hostile labels remain inert text",async()=>{
 const h=harness({openSource:true});
 const measured=Array.from({length:100},(_,i)=>({...target("sequence","declaration",`A.${i}`),matchKind:"measured"}));
 measured[63].symbol.name="<img src=x> λ";
 h.request=async()=>response(measured);await h.nav.open(h.event(),{path:"src/A.java",line:4});
 const actions=h.latest.actions.filter(item=>item.label.startsWith("Read declaration source"));
 assert.equal(actions.length,64);assert.match(actions[63].label,/<img src=x> λ/);
 assert.match(h.latest.actions.at(-2).label,/Partial/);
 actions[63].run();assert.equal(h.openedSources[0].symbol,measured[63].symbol);
 assert.equal(h.selected.length,0);
});

test("dismissed loading declaration menu cannot resurface or open stale source",async()=>{
 for(const dismiss of [h=>h.anchor.fire("keydown",{key:"Escape"}),h=>h.anchor.fire("keydown",{key:"Tab"}),h=>h.anchor.fire("pointerdown"),h=>h.latest.actions.at(-1).run()]) {
  const h=harness({openSource:true}),gate=deferred();h.request=()=>gate.promise;
  const pending=h.nav.open(h.event(),{path:"src/A.java",line:4});dismiss(h);
  gate.resolve(response([{...target("sequence","declaration","A.run"),matchKind:"measured"}]));await pending;
  assert.equal(h.menus.length,1);assert.equal(h.openedSources.length,0);
 }
});

test("navigation source action requires the selected path and exact line with valid bounds",async()=>{
 const h=harness({openSource:true}), good={...target("sequence","declaration","good"),matchKind:"measured"};
 const bad=[{...good,symbol:{...symbol("other"),path:"src/Other.java"}},
   {...good,symbol:{...symbol("later"),range:{startLine:5,endLine:7}}},
   {...good,symbol:{...symbol("inverted"),range:{startLine:4,endLine:3}}},
   {...good,symbol:{...symbol("empty"),path:""}},
   {...good,symbol:{...symbol("fraction"),range:{startLine:4.5,endLine:7}}}];
 h.request=async()=>response([...bad,good]);await h.nav.open(h.event(),{path:"src/A.java",line:4});
 assert.equal(h.latest.actions.filter(item=>item.label.startsWith("Read declaration source")).length,1);
 assert.equal(h.openedSources.length,0);
});

test("shared declaration menu focus dismissal and replacement suppress stale publication",async()=>{
 const selection={path:"src/A.java",line:4},measured={...target("sequence","declaration","A.run"),matchKind:"measured"};
 for(const replace of [false,true]){
  const h=harness({openSource:true}),gate=deferred();h.request=()=>gate.promise;
  const pending=h.nav.open(h.event(),selection);
  if(replace)h.showMenu(h.event(),[{label:"Other class menu",run(){}}]);else h.latest.close("dismiss");
  gate.resolve(response([measured]));await pending;
  assert.equal(h.menus.length,replace?2:1);
  if(replace)assert.equal(h.latest.actions[0].label,"Other class menu");
  assert.equal(h.openedSources.length,0);
 }
 const h=harness({openSource:true});h.request=async()=>response([measured]);await h.nav.open(h.event(),selection);
 const action=h.latest.actions[0];h.latest.close("action");h.anchor.focus();action.run();
 assert.equal(h.openedSources.length,1);
 await h.nav.open(h.event(),selection);const dismissed=h.latest.actions[0];h.latest.close("dismiss");dismissed.run();
 assert.equal(h.openedSources.length,1);
});

test("source declaration action rechecks session revision scope and retry before opening",async()=>{
 const selection={path:"src/A.java",line:4},measured={...target("sequence","declaration","A.run"),matchKind:"measured"};
 for(const invalidate of [h=>h.nav.reset(),h=>{h.session="two";},h=>{h.revision={...h.revision,indexRevision:2};},(_h,scope)=>{scope.current=false;},h=>h.nav.open(h.event(),selection)]){
  const h=harness({openSource:true}),scope={current:true};h.request=async()=>response([measured]);
  await h.nav.open(h.event(),selection,{isCurrent:()=>scope.current});const action=h.latest.actions[0];
  await invalidate(h,scope);action.run();assert.equal(h.openedSources.length,0);
 }
 const h=harness({openSource:true});h.request=async()=>response([measured]);
 await h.nav.open(h.event(),selection);const action=h.latest.actions[0];action.run();action.run();
 assert.equal(h.openedSources.length,1);
 h.request=async()=>{throw new Error("offline");};await h.nav.open(h.event(),selection);
 const retry=h.latest.actions[1];h.nav.reset();await retry.run();assert.equal(h.calls.length,2);
});

const declaration = (id="A.run",kind="method",line=4) => ({...symbol(id),kind,range:{...symbol(id).range,startLine:line,endLine:line+3}});
const outline = (items=[],extras={}) => ({revision:response().revision,path:"src/A.java",items:items.map(symbol=>({symbol})),nextOffset:null,truncated:false,warnings:[],...extras});
const inlineActions = p => p.pre.querySelectorAll(".source-diagram-action");

test("pinned metadata adds one-click callable declarations without changing source text or reading source",async()=>{
  const h=harness(),method=declaration("exact#λ", "method",5),noncallable=declaration("A.field","field",4);
  h.symbolsRequest=async()=>outline([noncallable,method]);
  const p=h.pane([4,5,6]);const original=p.rows.map(row=>row.children[1].textContent);
  await p.binding.ready;
  assert.equal(h.metadataCalls.length,1);assert.equal(h.calls.length,0);
  const url=new URL(h.metadataCalls[0].url,"http://localhost");
  assert.equal(url.pathname,"/api/symbols");assert.equal(url.searchParams.get("path"),"src/A.java");
  assert.equal(url.searchParams.get("limit"),"200");assert.equal(url.searchParams.get("offset"),"0");
  assert.equal(url.searchParams.get("indexGeneration"),response().revision.indexGeneration);
  const actions=inlineActions(p);assert.equal(actions.length,1);
  assert.equal(actions[0].parentNode,p.rows[1]);assert.equal(actions[0].getAttribute("data-symbol-id"),method.id);
  assert.equal(actions[0].getAttribute("data-source-action"),"sequence");
  assert.equal(actions[0].textContent,"");assert.match(actions[0].getAttribute("aria-label"),/exact#λ/);
  assert.deepEqual(p.rows.map(row=>row.children[1].textContent),original);
  assert.equal(h.selected.length,0);await actions[0].fire("click").done;
  assert.equal(h.selected[0],method);assert.equal(h.callbackPins[0].indexRevision,1);assert.equal(p.pre.dataset.navigationLine,"5");assert.equal(h.calls.length,0);
});

test("source declaration actions keep one tab stop on the active line and rove exact same-line identities",async()=>{
  const h=harness(),methods=Array.from({length:32},(_,i)=>declaration(`method-${i}`,"method",4+i));
  const sameLine=declaration("same-line-exact","method",5);
  h.symbolsRequest=async()=>outline([...methods,sameLine]);
  const p=h.pane(Array.from({length:33},(_,i)=>4+i));await p.binding.ready;
  const actions=inlineActions(p),peer=actions.find(node=>node.getAttribute("data-symbol-id")===sameLine.id),nextLine=actions.find(node=>node.getAttribute("data-symbol-id")===methods[2].id),tabbable=()=>actions.filter(node=>node.getAttribute("tabindex")==="0");
  assert.equal(actions.length,33);assert.deepEqual(tabbable(),[actions[0]]);
  p.pre.focus();p.pre.fire("keydown",{key:"ArrowDown"});
  assert.equal(p.pre.dataset.navigationLine,"5");assert.deepEqual(tabbable(),[actions[1]]);
  assert.equal(p.rows[1].scrolled.block,"nearest");assert.match(p.status.textContent,/Tab for Sequence.*← →/);
  actions[1].focus();const right=actions[1].fire("keydown",{key:"ArrowRight"});
  assert.equal(right.prevented,true);assert.equal(h.document.activeElement,peer);
  assert.deepEqual(tabbable(),[peer]);assert.equal(peer.textContent,"");
  assert.equal(peer.getAttribute("data-symbol-id"),sameLine.id);
  peer.fire("keydown",{key:"ArrowLeft"});assert.equal(h.document.activeElement,actions[1]);
  actions[1].fire("keydown",{key:"ArrowRight"});await peer.fire("click").done;
  assert.equal(h.selected[0],sameLine);assert.equal(h.calls.length,0);
  peer.fire("keydown",{key:"ArrowDown"});
  assert.equal(h.document.activeElement,p.pre);assert.equal(p.pre.dataset.navigationLine,"6");
  assert.deepEqual(tabbable(),[nextLine]);
  p.pre.fire("keydown",{key:"End"});assert.equal(tabbable().length,0,"a line without declarations adds no tab stop");
  peer.focus();assert.equal(p.pre.dataset.navigationLine,"5");assert.deepEqual(tabbable(),[peer],"programmatic focus restores the exact roving action");
});

test("More and metadata snapshots preserve a single active action including later same-line declarations",async()=>{
  const h=harness(),first=declaration("first","method",4),second=declaration("second","method",4),later=declaration("later-page","method",4);
  h.symbolsRequest=async url=>new URL(url,"http://localhost").searchParams.get("offset")==="0" ?
    outline([first,second],{nextOffset:2,truncated:true}) : outline([later]);
  const p=h.pane([4,5]);await p.binding.ready;
  inlineActions(p)[1].focus();const more=p.toolbar.children[2];more.focus();
  await more.fire("click").done;
  assert.equal(h.document.activeElement,more,"appending metadata never steals focus");
  const actions=inlineActions(p);assert.equal(actions.length,3);
  assert.deepEqual(actions.map(node=>node.getAttribute("tabindex")),["-1","0","-1"],"More does not reset the chosen action");
  actions[1].focus();actions[1].fire("keydown",{key:"ArrowRight"});assert.equal(h.document.activeElement,actions[2]);
  const saved=p.binding.snapshot();p.binding.dispose();const requests=h.metadataCalls.length;
  const restored=h.pane([4,5],{metadataSnapshot:saved,startLine:4}),hydrated=inlineActions(restored);
  assert.equal(h.metadataCalls.length,requests);assert.equal(hydrated.length,3);
  hydrated[2].focus();
  assert.deepEqual(hydrated.map(node=>node.getAttribute("tabindex")),["-1","-1","0"]);
  assert.equal(restored.pre.dataset.navigationLine,"4");await hydrated[2].fire("click").done;
  assert.equal(h.selected[0],later);assert.equal(h.calls.length,0);
});

test("source action coverage is finite and continues only with explicit More",async()=>{
  const h=harness(),first=Array.from({length:200},(_,i)=>declaration(`A.${i}`,"field",4));
  const second=declaration("last","method",6);
  h.symbolsRequest=async url=>new URL(url,"http://localhost").searchParams.get("offset")==="0" ?
    outline(first,{nextOffset:200,truncated:true}) : outline([second]);
  const p=h.pane([4,5,6]);await p.binding.ready;
  assert.equal(h.metadataCalls.length,1);assert.equal(inlineActions(p).length,0);
  const more=p.toolbar.children[2],coverage=p.toolbar.children[3];
  assert.equal(more.hidden,false);assert.match(coverage.textContent,/200 declarations/);assert.match(coverage.textContent,/more available/);
  await more.fire("click").done;
  assert.equal(h.metadataCalls.length,2);assert.equal(inlineActions(p).length,1);
  assert.equal(more.hidden,true);assert.equal(coverage.textContent,"");
  await p.pre.fire("keydown",{key:"ContextMenu"}).done;
  assert.equal(h.calls[0].url,"/api/navigation");
});

test("invalid, cross-path and stale metadata produce no inline actions",async()=>{
  const method=declaration();
  for(const bad of [outline([method],{path:"src/Other.java"}),outline([{...method,path:"src/Other.java"}]),
    outline([{...method,range:{startLine:4,endLine:3}}]),outline([method],{nextOffset:0}),
    outline([method],{revision:{...response().revision,indexRevision:2}})]) {
    const h=harness();h.symbolsRequest=async()=>bad;
    const p=h.pane([4,5]);await p.binding.ready;
    assert.equal(inlineActions(p).length,0);assert.match(p.toolbar.children[3].textContent,/unavailable|stale/);
    assert.equal(h.selected.length,0);assert.equal(h.calls.length,0);
  }
});

test("metadata callbacks and late replies respect reset, disposal, session, pin and source fences",async()=>{
  const invalidations=[(h,p)=>p.binding.dispose(),h=>h.nav.reset(),
    h=>{h.session="two";},h=>{h.revision={...response().revision,indexRevision:2};},(_h,_p,scope)=>{scope.current=false;}];
  for(const invalidate of invalidations) for(const pending of [true,false]) {
    const h=harness(),gate=deferred(),scope={current:true};h.symbolsRequest=()=>gate.promise;
    const method=declaration(),p=h.pane([4,5],{isCurrent:()=>scope.current});
    let action;
    if(!pending){gate.resolve(outline([method]));await p.binding.ready;action=inlineActions(p)[0];}
    invalidate(h,p,scope);
    if(pending){gate.resolve(outline([method]));await p.binding.ready;assert.equal(inlineActions(p).length,0);}
    if(action)await action.fire("click").done;
    assert.equal(h.selected.length,0);
  }
});

test("metadata failure offers explicit retry while line navigation remains available",async()=>{
  const h=harness();h.symbolsRequest=async()=>{throw Error("offline");};const p=h.pane([4]);await p.binding.ready;
  assert.match(p.toolbar.children[3].textContent,/offline/);
  h.symbolsRequest=async()=>outline([declaration()]);await p.toolbar.children[2].fire("click").done;
  assert.equal(inlineActions(p).length,1);assert.equal(p.toolbar.children[2].hidden,true);
  await p.button.fire("click").done;assert.equal(h.calls[0].url,"/api/navigation");
});

test("declaration menus expose exact Sequence, Inspect and source actions without automatic transitions",async()=>{
  const h=harness({openSource:true}),method=declaration();
  h.request=async()=>response([{symbol:method,reason:"declaration",matchKind:"measured",action:"sequence"}]);
  await h.nav.open(h.event(),{path:method.path,line:4});
  assert.equal(h.selected.length+h.inspected.length+h.openedSources.length,0);
  const sequence=h.latest.actions.find(a=>a.label.startsWith("Sequence · syntax ·"));assert.ok(sequence);
  assert.ok(h.latest.actions.find(a=>a.label.startsWith("Inspect ·")));
  assert.ok(h.latest.actions.find(a=>a.label.startsWith("Read declaration source")));
  assert.equal(h.latest.actions.find(a=>a.label.startsWith("Call graph")).disabled,true);
  sequence.run();assert.equal(h.selected[0],method);
});

test("enclosing callable and supported class actions use measured identity, not lexical guesses",async()=>{
  const h=harness(),method=declaration(),cls=declaration("A","class");
  h.request=async()=>response([{symbol:method,reason:"enclosing",matchKind:"measured",action:"sequence"}]);
  await h.nav.open(h.event(),{path:method.path,line:5});
  const enclosing=h.latest.actions.find(a=>a.label.startsWith("Sequence · enclosing · syntax"));assert.ok(enclosing);
  enclosing.run();assert.equal(h.selected[0],method);
  h.request=async()=>response([{symbol:cls,reason:"declaration",matchKind:"measured",action:"class"}]);
  await h.nav.open(h.event(),{path:cls.path,line:4});
  const diagram=h.latest.actions.find(a=>a.label.startsWith("Class diagram"));assert.ok(diagram);
  diagram.run();assert.equal(h.classes[0],cls);
  h.request=async()=>response([{symbol:{...cls,path:"web/a.mjs"},reason:"declaration",matchKind:"measured",action:"class"}]);
  await h.nav.open(h.event(),{path:"web/a.mjs",line:4});
  assert.ok(h.latest.actions.find(a=>a.label.startsWith("Class diagram")),"JavaScript classes are catalogued (#452)");
  h.request=async()=>response([{symbol:{...cls,path:"src/a.rs"},reason:"declaration",matchKind:"measured",action:"class"}]);
  await h.nav.open(h.event(),{path:"src/a.rs",line:4});
  assert.ok(h.latest.actions.find(a=>a.label.startsWith("Class diagram")),"Rust types are catalogued (#452)");
  h.request=async()=>response([{symbol:{...cls,path:"src/A.go"},reason:"declaration",matchKind:"measured",action:"class"}]);
  await h.nav.open(h.event(),{path:"src/A.go",line:4});
  assert.equal(h.latest.actions.some(a=>a.label.startsWith("Class diagram")),false);
});

test("only explicit call/resolved targets get resolved-callee sequence actions",async()=>{
  const h=harness(),method=declaration("real"),candidate=declaration("guess");
  h.request=async()=>response([
    {symbol:candidate,reason:"call",matchKind:"measured",action:"sequence"},
    {symbol:candidate,reason:"call",matchKind:"syntaxCandidate",action:"sequence"},
    {symbol:candidate,reason:"type",matchKind:"resolved",action:"sequence"},
    {symbol:method,reason:"call",matchKind:"resolved",action:"sequence"}]);
  await h.nav.open(h.event(),{path:"src/A.java",line:20});
  const actions=h.latest.actions.filter(a=>a.label.startsWith("Sequence"));assert.equal(actions.length,1);
  assert.match(actions[0].label,/Sequence of callee · resolved/);actions[0].run();assert.equal(h.selected[0],method);
});

test("new diagram and inspector callbacks recheck all menu scope boundaries",async()=>{
  for(const invalidation of [h=>h.nav.reset(),h=>{h.session="two";},h=>{h.revision={...response().revision,indexRevision:2};},
    h=>h.latest.close("dismiss"),h=>h.showMenu(h.event(),[{label:"Other",run(){}}]),(_h,scope)=>{scope.current=false;}]) {
    for(const prefix of ["Sequence","Inspect"]){
      const h=harness(),scope={current:true};h.request=async()=>response([{symbol:declaration(),reason:"declaration",matchKind:"measured",action:"sequence"}]);
      await h.nav.open(h.event(),{path:"src/A.java",line:4},{isCurrent:()=>scope.current});
      const action=h.latest.actions.find(a=>a.label.startsWith(prefix));assert.ok(action);
      invalidation(h,scope);action.run();assert.equal(h.selected.length+h.inspected.length,0);
    }
  }
});

test("binding menu reset preserves valid source declaration actions and continuation",async()=>{
  const h=harness(),method=declaration();
  h.symbolsRequest=async url=>new URL(url,"http://localhost").searchParams.get("offset")==="0" ?
    outline([method],{nextOffset:1,truncated:true}) : outline([declaration("later","method",5)]);
  const p=h.pane([4,5]);await p.binding.ready;
  const action=inlineActions(p)[0];p.binding.reset();
  assert.equal(inlineActions(p)[0],action);
  await p.toolbar.children[2].fire("click").done;
  assert.equal(inlineActions(p).length,2);await action.fire("click").done;
  assert.equal(h.selected[0],method);
});

test("source metadata snapshots hydrate admitted pages synchronously with exact identities and no fetch",async()=>{
  const h=harness(),first=declaration("first","method",4),second=declaration("second","method",5),third=declaration("third","method",6);
  h.symbolsRequest=async url=>{
    const offset=Number(new URL(url,"http://localhost").searchParams.get("offset"));
    return offset===0 ? outline([first],{nextOffset:1,truncated:true}) :
      offset===1 ? outline([second],{nextOffset:2,truncated:true,warnings:["Partial input"]}) : outline([third]);
  };
  const p=h.pane([4,5,6]);await p.binding.ready;
  const pageOne=p.binding.snapshot();assert.equal(pageOne.items.length,1);assert.equal(pageOne.items[0],first);
  await p.toolbar.children[2].fire("click").done;
  const saved=p.binding.snapshot();assert.equal(saved.items.length,2);assert.equal(saved.items[1],second);
  assert.equal(saved.nextOffset,2);assert.equal(saved.metadataCount,2);assert.equal(saved.metadataPartial,true);
  p.binding.dispose();assert.equal(p.binding.snapshot().items[1],second);const before=h.metadataCalls.length;
  const restored=h.pane([4,5,6],{metadataSnapshot:saved,startLine:5});
  const actions=inlineActions(restored);assert.equal(actions.length,2);assert.equal(h.metadataCalls.length,before);
  assert.equal(actions[1].getAttribute("data-symbol-id"),second.id);
  assert.equal(restored.pre.dataset.navigationLine,"5");assert.match(restored.toolbar.children[3].textContent,/2 declarations/);
  await actions[1].fire("click").done;assert.equal(h.selected[0],second);
  await restored.toolbar.children[2].fire("click").done;
  assert.equal(h.metadataCalls.length,before+1);assert.match(h.metadataCalls.at(-1).url,/offset=2&limit=200/);
  assert.equal(inlineActions(restored).length,3);
});

test("pending initial metadata has no snapshot, while admitted empty metadata restores without fetching",async()=>{
  const h=harness(),gate=deferred();h.symbolsRequest=()=>gate.promise;
  const p=h.pane([4]);assert.equal(p.binding.snapshot(),undefined);
  gate.resolve(outline());await p.binding.ready;
  const saved=p.binding.snapshot();assert.equal(saved.items.length,0);assert.equal(saved.nextOffset,null);
  p.binding.dispose();const before=h.metadataCalls.length;
  const restored=h.pane([4],{metadataSnapshot:saved});
  assert.equal(h.metadataCalls.length,before);assert.equal(inlineActions(restored).length,0);
  assert.equal(restored.toolbar.children[2].hidden,true);assert.equal(restored.binding.snapshot().metadataCount,0);
});

test("resolved choices distinguish native callsite ranges and retain exact same-callee symbols",async()=>{
  const h=harness(),first=declaration("callee"),second={...first};
  const targets=[
    {symbol:first,reason:"call",matchKind:"resolved",action:"sequence",callId:"call-1",range:{start:100,end:110}},
    {symbol:second,reason:"call",matchKind:"resolved",action:"sequence",callId:"call-2",range:{start:120,end:130}}];
  h.request=async()=>response(targets);
  for(const index of [0,1]) {
    await h.nav.open(h.event(),{path:"src/A.java",line:20});
    const actions=h.latest.actions.filter(a=>a.label.startsWith("Sequence of callee"));
    assert.equal(actions.length,2);assert.notEqual(actions[0].label,actions[1].label);
    assert.match(actions[0].label,/call bytes 100–110/);assert.match(actions[1].label,/call bytes 120–130/);
    actions[index].run();assert.equal(h.selected[index],targets[index].symbol);
  }
  targets[1].range=targets[0].range;
  await h.nav.open(h.event(),{path:"src/A.java",line:20});
  const duplicate=h.latest.actions.filter(a=>a.label.startsWith("Sequence of callee"));
  assert.match(duplicate[0].label,/call call-1/);assert.match(duplicate[1].label,/call call-2/);
});

test("all source lookups inject solePublished while closed member selectors remain syntax-only",async()=>{
  const h=harness();await h.nav.open(h.event(),{path:"src/A.java",line:4});
  assert.deepEqual(JSON.parse(JSON.stringify(h.calls[0].options.body)),{
    expectedRevision:response().revision,path:"src/A.java",line:4,semanticProducerPolicy:"solePublished"});
  const p=h.pane([4]);await p.button.fire("click").done;
  assert.equal(h.calls[1].options.body.semanticProducerPolicy,"solePublished");
  await h.nav.open(h.event(),{classId:"A",memberName:"run",startByte:10,endByte:60});
  assert.equal(Object.hasOwn(h.calls[2].options.body,"semanticProducerPolicy"),false);
  const before=h.calls.length;
  await h.nav.open(h.event(),{path:"src/A.java",line:4,semanticProducerPolicy:"solePublished"});
  await h.nav.open(h.event(),{path:"src/A.java",line:4,semanticProducerId:"producer"});
  assert.equal(h.calls.length,before);
});

test("source retries retain solePublished without accepting policy from caller",async()=>{
  const h=harness();h.request=async()=>{throw Error("offline");};
  await h.nav.open(h.event(),{path:"src/A.java",line:4});
  h.request=async()=>response([]);await h.latest.actions.find(a=>a.label==="Try navigation again").run();
  assert.equal(h.calls.length,2);assert.equal(h.calls[1].options.body.semanticProducerPolicy,"solePublished");
});

test("multiple producer warning leaves only explicitly labelled syntax choices and no callee action",async()=>{
  const h=harness(),method=declaration();
  const warning="multiple semantic producers for java; showing syntax";
  h.request=async()=>response([
    {symbol:method,reason:"enclosing",action:"sequence",matchKind:"measured"},
    {symbol:declaration("guess"),reason:"call",action:"sequence",matchKind:"syntaxCandidate"}],{warnings:[warning]});
  await h.nav.open(h.event(),{path:"src/A.java",line:5});
  assert.equal(h.latest.actions.some(a=>a.label.startsWith("Sequence of callee")),false);
  const syntax=h.latest.actions.find(a=>a.label.startsWith("Sequence · enclosing · syntax"));assert.ok(syntax);
  assert.equal(h.latest.actions.find(a=>a.label===warning).disabled,true);
  syntax.run();assert.equal(h.selected[0],method);
});
