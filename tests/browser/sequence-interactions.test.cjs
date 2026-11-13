"use strict";
// #636: the Interactions view. Real Pele, real CSS and real geometry in Chromium, over a DTO
// shaped like `coordinate` (src/mcp/mod.rs): most steps stay in the seed's own column and only a
// few calls cross to another type, one of them through a nested chain (Inbox → FrameWindow).
// Owner review on #638: the view shows only interactions between entities. Internal steps draw
// nothing (one count in the status line); a loop around arrows is a UML loop frame.
const test = require("node:test");
const assert = require("node:assert/strict");
const {readFileSync} = require("node:fs");
const {createServer} = require("node:http");
const {resolve, join} = require("node:path");
const {chromium} = require("playwright");
const ROOT = resolve(__dirname, "../..");
const files = {"/vendor/pele-1.10.0-trellis.2.js":"web/vendor/pele-1.10.0-trellis.2.js", "/sequence-pele.js":"web/sequence-pele.js", "/source-witness.js":"web/source-witness.js", "/sequence.js":"web/sequence.js", "/style.css":"web/style.css"};
const html = '<!doctype html><html><head><link rel="stylesheet" href="/style.css"><script src="/vendor/pele-1.10.0-trellis.2.js" defer></script><script src="/source-witness.js" defer></script><script src="/sequence.js" defer></script><script src="/sequence-pele.js" defer></script></head><body><div id="sequence-diagram" class="sequence-scroll" style="flex:none;width:1200px;height:700px"></div></body></html>';
let server, browser, page;

test.before(async () => {
  server = createServer((req, res) => {
    const file = files[req.url];
    if (file) { res.setHeader("Content-Type", file.endsWith("css") ? "text/css; charset=utf-8" : "text/javascript; charset=utf-8"); res.end(readFileSync(join(ROOT, file))); return; }
    if (req.url === "/") { res.setHeader("Content-Type", "text/html"); res.end(html); return; }
    res.writeHead(404); res.end();
  });
  await new Promise(done => server.listen(0, "127.0.0.1", done));
  browser = await chromium.launch(); page = await browser.newPage({viewport:{width:1440, height:900}});
  await page.goto(`http://127.0.0.1:${server.address().port}`);
  await page.waitForFunction(() => !!window.TrellisPeleSequence && !!window.pele);
});
test.after(async () => { await browser?.close(); await new Promise(done => server?.close(done)); });

// The seed `coordinate` in column `mcp`. Cross-column calls: open; open and start again inside a
// loop that may exit early; start; refresh inside a branch; deliver, whose expanded body calls
// FrameWindow.show; and, inside the same-column helper's body, tools.run.
function coordinateView() {
  const view = JSON.parse(readFileSync(join(ROOT, "tests/fixtures/sequence-pele/rust.json")));
  const template = view.steps[0], seed = view.seed;
  seed.name = "coordinate"; seed.path = "src/mcp/mod.rs";
  view.semantic = {tier:"semantic", producerId:"rust-analyzer"};
  view.participants = [{id:seed.id, label:"coordinate", kind:"method", identification:"Selected source symbol.", column:{id:"module:mcp", label:"mcp"}, declaration:seed}];
  const method = (id, name, column) => {
    view.participants.push({id, label:name, kind:"method", identification:"Fresh exact target",
      declaration:{...structuredClone(seed), id, name, path:`src/${column.toLowerCase()}.rs`}, column:{id:`type:${column}`, label:column}});
    return id;
  };
  method("open", "open", "OpenedWorkspace"); method("refresh", "refresh", "OpenedWorkspace");
  method("start", "start", "Session"); method("deliver", "deliver", "Inbox"); method("show", "show", "FrameWindow");
  method("run", "run", "tools");
  view.participants.push({id:"helper", label:"helper", kind:"method", identification:"Fresh exact target",
    declaration:{...structuredClone(seed), id:"helper", name:"helper"}, column:{id:"module:mcp", label:"mcp"}});
  const step = (id, kind, label, extra = {}) => ({...structuredClone(template), id, kind, label, callId:null, children:[], alternate:[], hidden:false, ...extra});
  const evidence = id => ({boundaryReason:"none", resolution:"resolved", dispatch:"direct", freshness:"fresh", provenanceId:`proof:${id}`});
  const call = (id, target, extra = {}) => step(id, "call", `self.${target}_spelling`, {callId:`callid:${id}`, target, callEvidence:evidence(id), ...extra});
  const unresolved = (id, extra = {}) => step(id, "call", `unknown_${id}`, {callId:`callid:${id}`, callEvidence:{boundaryReason:"unresolved"}, ...extra});
  const effect = (id, extra = {}) => step(id, "effect", `bind name: v_${id}`, extra);
  view.steps = [
    effect("e1"),
    step("if2", "branch", "if ready", {children:[unresolved("u1"), step("ret2", "return", "return Err(x)")]}),
    call("c-open", "open"),
    step("loop4", "loop", "for_expression: possible iterations, not unrolled; exit/termination unknown", {children:[
      unresolved("u2"), call("c-loop-open", "open"), step("if-loop", "branch", "if done", {children:[step("ret-loop", "return", "return Ok(())")]}),
      call("c-loop-start", "start"), call("c-helper-in-loop", "helper")]}),
    call("c-start", "start"),
    step("if6", "branch", "if refresh", {children:[call("c-refresh", "refresh"), effect("e6")], alternate:[unresolved("u3")]}),
    call("c-deliver", "deliver", {children:[effect("d1", {lane:"deliver"}), call("c-deliver>show", "show", {lane:"deliver"}), unresolved("d3", {lane:"deliver"})]}),
    call("c-helper", "helper", {children:[call("c-helper>run", "run", {lane:"helper"})]}),
    effect("e9"), unresolved("u5"),
    step("hidden", "call", "std::mem::take", {callId:"callid:hidden", hidden:true, incidental:"external", callEvidence:{boundaryReason:"external"}})
  ];
  view.warnings = []; view.hiddenSteps = 1; view.incidentalSteps = 1;
  return view;
}
const INTERACTIONS = {interactions:true};
// Source order. The branch and the same-column helper are frames around their arrows (#907), not
// rows, so their arrows keep their numbers.
const ARROWS = ["c-open", "c-loop-open", "c-loop-start", "c-start", "c-refresh", "c-deliver", "c-deliver>show", "c-helper>run"];
const LABELS = ["[001] open", "[002] open", "[003] start", "[004] start", "[005] refresh", "[006] deliver", "[007] show", "[008] run"];
const RETURNS = ["c-open", "c-loop-open", "c-loop-start", "c-start", "c-refresh", "c-deliver>show", "c-deliver", "c-helper>run"];
// e1 if2 u1 ret2 · u2 if-loop ret-loop c-helper-in-loop · e6 u3 · d1 d3 · e9 u5. The branch if6
// and the helper c-helper are drawn as frames (#907).
const HIDDEN = 14;

async function render(view, options = INTERACTIONS) {
  return page.evaluate(async ({view, options}) => {
    const container = document.getElementById("sequence-diagram");
    window.view = view; window.picks = [];
    const result = await TrellisPeleSequence.render(container, view, null, new Set(), {...options, onSelect:step => window.picks.push(step)});
    return {status:result.status, reason:result.reason};
  }, {view, options});
}
const ids = (selector, attribute) => page.locator(selector).evaluateAll((nodes, attribute) => nodes.map(node => node.getAttribute(attribute)), attribute);
const box = locator => locator.evaluate(node => { const box = node.getBoundingClientRect(); return {x:box.left + box.width / 2, y:box.top + box.height / 2, top:box.top, bottom:box.bottom, left:box.left, right:box.right}; });
// Each lane's header, its lines joined by a space (#716): the kind, then the wrapped name.
const headers = selector => page.locator(selector).evaluateAll(nodes => nodes.map(node => {
  const lines = [...node.querySelectorAll("tspan")].map(line => line.textContent);
  return lines.length ? lines.join(" ") : node.matches(".pele-actor") ? node.textContent : node.firstChild.textContent;
}));
const actor = name => page.locator("#sequence-diagram .pele-actor:not(.pele-actor-footer)").filter({hasText:new RegExp(`»${name}$`)});
const label = id => page.locator(`#sequence-diagram .pele-edge-label[data-trellis-step-id="${id}"]`);

test("Pele draws only cross-column calls, with returns, in source order, and nothing else", async () => {
  const view = coordinateView();
  const result = await render(view);
  assert.equal(result.status, "rendered", result.reason);
  assert.deepEqual(await headers("#sequence-diagram .pele-actor:not(.pele-actor-footer)"), ["«module» mcp", "«type» Opened Workspace", "«type» Session", "«type» Inbox", "«type» Frame Window", "«type» tools"]);
  assert.deepEqual(await ids("#sequence-diagram .pele-edge-label", "data-trellis-step-id"), ARROWS);
  assert.deepEqual(await page.locator("#sequence-diagram .pele-edge-label").allTextContents(), LABELS);
  assert.deepEqual(await ids("#sequence-diagram .pele-message[data-trellis-return-step-id]", "data-trellis-return-step-id"), RETURNS);
  assert.equal(await page.locator("#sequence-diagram .pele-message").count(), ARROWS.length + RETURNS.length);
  assert.equal(await page.locator("#sequence-diagram .pele-activation").count(), RETURNS.length);
  // No internal-step row, badge or note: only the intro note and the helper frame's name (#907).
  assert.deepEqual(await page.locator("#sequence-diagram .pele-note").allTextContents(), ["Static possible paths - not a runtime trace", "helper"]);
  assert.equal(await page.locator("#sequence-diagram [data-trellis-badge-id]").count(), 0);
  assert.equal(await page.evaluate(view => TrellisSequence.display(view, {interactions:true}).hiddenSteps, view), HIDDEN);
  // Arrows run top to bottom in source order.
  const tops = [];
  for (const id of ARROWS) tops.push((await box(label(id))).y);
  assert.deepEqual(tops, [...tops].sort((a, b) => a - b), JSON.stringify(tops));
});

test("a loop around arrows is one labelled UML frame around exactly them, with no row number", async () => {
  assert.equal((await render(coordinateView())).status, "rendered");
  const frames = page.locator("#sequence-diagram .pele-frame-loop");
  assert.equal(await frames.count(), 1);
  assert.equal(await frames.locator(".pele-frame-kind").textContent(), "loop");
  // The tag says "loop" once; the guard says only the qualifier.
  assert.deepEqual(await frames.locator(".pele-frame-label").allTextContents(), ["[may exit early]"]);
  const frame = await box(frames.locator("rect, path").first());
  for (const id of ["c-loop-open", "c-loop-start"]) {
    const arrow = await box(label(id));
    assert.ok(frame.top < arrow.y && arrow.y < frame.bottom, `${id} inside: ${JSON.stringify({frame, arrow})}`);
  }
  for (const id of ["c-open", "c-start"]) {
    const arrow = await box(label(id));
    assert.ok(arrow.y < frame.top || arrow.y > frame.bottom, `${id} outside: ${JSON.stringify({frame, arrow})}`);
  }
  // Without an exit in it, the frame has only its "loop" tag; with a folded `?`, it may exit early.
  const plain = coordinateView(), loop = plain.steps[3];
  loop.children = loop.children.filter(step => step.id !== "if-loop");
  assert.equal((await render(plain)).status, "rendered");
  assert.equal(await page.locator("#sequence-diagram .pele-frame-loop .pele-frame-kind").textContent(), "loop");
  assert.equal(await page.locator("#sequence-diagram .pele-frame-loop .pele-frame-label").count(), 0, "no [loop] guard");
  loop.children[1].folded = [{...structuredClone(loop.children[0]), id:"q", kind:"branch", label:"?: continue only on success; otherwise early return", callId:null}];
  assert.equal(await page.evaluate(view => { const d = TrellisSequence.display(view, {interactions:true}); return d.loopLabel(view.steps[3]); }, plain), "may exit early");
});

// #907: `enqueue_generated_semantic_import`. The same-column `admission_lease` expands a body
// whose `match` wakes the inbox in one arm; then the seed enqueues into the store, and wakes the
// inbox again in the `Err` arm of a `match` on that call's result.
function enqueueView() {
  const view = coordinateView(), template = view.steps[0], seed = view.seed;
  view.participants = view.participants.filter(p => p.id === seed.id);
  const method = (id, column) => view.participants.push({id, label:id, kind:"method", identification:"Fresh exact target",
    declaration:{...structuredClone(seed), id, name:id, path:`src/${column}.rs`}, column:{id:`type:${column}`, label:column}});
  method("admission_lease", "DaemonState"); method("enqueue_as", "Store"); method("wake", "Inbox");
  view.participants[0].column = {id:"type:DaemonState", label:"DaemonState"};
  const step = (id, kind, label, extra = {}) => ({...structuredClone(template), id, kind, label, callId:null, children:[], alternate:[], hidden:false, ...extra});
  const call = (id, target, extra = {}) => step(id, "call", `self.${target}()`, {callId:`callid:${id}`, target,
    callEvidence:{boundaryReason:"none", resolution:"resolved", dispatch:"direct", freshness:"fresh", provenanceId:`proof:${id}`}, ...extra});
  const arm = (id, pattern, children) => step(id, "branch", `only if no earlier arm selected and pattern matches: ${pattern}`, {children});
  const match = (id, arms, extra = {}) => step(id, "branch", "match: mutually exclusive arms; first matching pattern with true guard", {alternate:arms, ...extra});
  const lane = {lane:"admission_lease"};
  view.steps = [
    call("lease", "admission_lease", {children:[
      match("lease>match", [arm("lease>closing", "RuntimeState::Closing", [step("lease>ret", "return", "return Err(x)", lane)]),
        arm("lease>retiring", "RuntimeState::RetireRequested { session, .. }", [call("lease>wake", "wake", lane)]), arm("lease>other", "_", [])], lane)]}),
    call("enqueue", "enqueue_as"),
    match("match", [arm("ok", "Ok(row)", []), arm("err", "Err(error)", [call("err>wake", "wake"), step("err>ret", "return", "return Err(error)")])]),
    call("wake", "wake")
  ];
  return view;
}

test("#907: a same-column call's body around an arrow is a frame named for its callee, before the next call", async () => {
  const view = enqueueView();
  assert.equal((await render(view)).status, "rendered");
  assert.deepEqual(await page.locator("#sequence-diagram .pele-edge-label").allTextContents(), ["[001] wake", "[002] enqueue_as", "[003] wake", "[004] wake"]);
  // The callee frame: a shaded region opened by a note naming the callee, around its arrow only.
  assert.deepEqual(await page.locator("#sequence-diagram .pele-note").allTextContents(), ["Static possible paths - not a runtime trace", "admission_lease"]);
  assert.equal(await page.locator("#sequence-diagram .pele-rect").count(), 1);
  const region = await box(page.locator("#sequence-diagram .pele-rect"));
  assert.ok(region.top < (await box(label("lease>wake"))).y && (await box(label("lease>wake"))).y < region.bottom, "the lease's wake is inside its frame");
  for (const id of ["enqueue", "err>wake", "wake"]) assert.ok((await box(label(id))).y > region.bottom, `${id} after the frame`);
  // A same-column call whose body draws no arrow is still nothing.
  const quiet = enqueueView(); quiet.steps[0].children = [];
  assert.equal(await page.evaluate(view => TrellisSequence.display(view, {interactions:true}).callees.size, quiet), 0);
});

test("#907: a branch around an arrow is an alt frame, an arm an opt frame, and arms without an arrow draw nothing", async () => {
  const view = enqueueView();
  assert.equal((await render(view)).status, "rendered");
  // Pele appends a frame after the frames nested in it: the arm, then its match.
  const kinds = await page.locator("#sequence-diagram .pele-frame .pele-frame-kind").allTextContents();
  assert.deepEqual(kinds, ["opt", "alt", "opt", "alt"]);
  // Pele wraps a long header over lines; compare without whitespace, as its validator does.
  const headers = await page.locator("#sequence-diagram .pele-frame").evaluateAll(nodes => nodes.map(n => [...n.querySelectorAll(".pele-frame-label")].map(l => l.textContent.replace(/\s+/g, ""))));
  const squeeze = rows => rows.map(row => row.map(text => text.replace(/\s+/g, "")));
  assert.deepEqual(headers, squeeze([
    ["[only if no earlier arm selected and pattern matches: RuntimeState::Reti…]"],
    ["[alternate path]", "[match · first matching arm]"],
    ["[only if no earlier arm selected and pattern matches: Err(error)]"],
    ["[alternate path]", "[match · first matching arm]"]]));
  const frames = page.locator("#sequence-diagram .pele-frame");
  const err = await box(frames.nth(2).locator("rect, path").first()), match = await box(frames.nth(3).locator("rect, path").first());
  const inside = async (frame, id) => { const y = (await box(label(id))).y; return frame.top < y && y < frame.bottom; };
  assert.ok(await inside(err, "err>wake") && await inside(match, "err>wake"), "the Err arm's wake is in its arm and its match");
  for (const id of ["enqueue", "wake"]) assert.ok(!(await inside(match, id)), `${id} outside the match`);
  assert.ok((await box(label("enqueue"))).y < match.top, "the scrutinee call comes before its match");
  // An if with an else around an arrow is alt; the coordinate view's if6 holds c-refresh.
  const display = await page.evaluate(view => { const d = TrellisSequence.display(view, {interactions:true}); return [...d.branches].map(s => [s.id, d.frameKind(s), d.frameLabel(s)]); }, coordinateView());
  assert.deepEqual(display, [["if6", "alt", "if refresh"]]);
  // A try groups with branches: alt with its handlers, opt without.
  const tried = coordinateView(), [open] = [tried.steps[2]];
  const body = {...structuredClone(tried.steps[0]), id:"try", kind:"try", label:"try; exception behavior unknown", children:[open], alternate:[]};
  tried.steps = [body, {...structuredClone(body), id:"try-catch", alternate:[{...structuredClone(open), id:"catch>open", callId:"callid:catch>open"}], children:[]}];
  assert.deepEqual(await page.evaluate(view => { const d = TrellisSequence.display(view, {interactions:true}); return [...d.branches].map(s => d.frameKind(s)); }, tried), ["opt", "alt"]);
  assert.equal((await render(tried)).status, "rendered");
  assert.deepEqual(await page.locator("#sequence-diagram .pele-frame .pele-frame-kind").allTextContents(), ["opt", "alt"]);
});

test("a nested chain starts on its callee's column and returns before its caller does", async () => {
  assert.equal((await render(coordinateView())).status, "rendered");
  const inbox = await box(actor("Inbox")), frame = await box(actor("FrameWindow")), mcp = await box(actor("mcp"));
  const show = await box(page.locator('#sequence-diagram .pele-message[data-trellis-step-id="c-deliver>show"] path').first());
  // An arrow leaves the edge of its caller's activation bar (10 px wide) and its head (7 px)
  // touches the callee's.
  assert.ok(Math.abs(show.left - inbox.x) <= 6 && Math.abs(show.right - frame.x) <= 13, `Inbox → FrameWindow: ${JSON.stringify({show, inbox, frame})}`);
  const back = await box(page.locator('#sequence-diagram .pele-message[data-trellis-return-step-id="c-deliver"] path').first());
  assert.ok(Math.abs(back.right - inbox.x) <= 6 && Math.abs(back.left - mcp.x) <= 8, `deliver returns to mcp: ${JSON.stringify({back, inbox, mcp})}`);
  const showBack = await box(page.locator('#sequence-diagram .pele-message[data-trellis-return-step-id="c-deliver>show"] path').first());
  assert.ok(show.y < showBack.y && showBack.y < back.y);
});

test("a nested callable draws no row; a cross-column call in its body is an arrow from its column", async () => {
  const view = coordinateView(), [open] = [view.steps[2]];
  const closure = (id, children) => ({...structuredClone(view.steps[0]), id, kind:"boundary", label:"Nested callable boundary: body does not execute at definition; captures are not expanded.", children});
  view.steps = [closure("closure-empty", []), closure("closure-call", [{...structuredClone(open), id:"closure>open", callId:"callid:closure>open"}])];
  assert.equal((await render(view)).status, "rendered");
  assert.deepEqual(await page.locator("#sequence-diagram .pele-edge-label").allTextContents(), ["[001] open"]);
  assert.equal(await page.locator("#sequence-diagram .pele-note").count(), 1, "only the intro note");
  assert.equal(await page.evaluate(view => TrellisSequence.display(view, {interactions:true}).hiddenSteps, view), 2);
});

test("selecting an arrow hands the inspector the original step and evidence", async () => {
  const view = coordinateView();
  assert.equal((await render(view)).status, "rendered");
  assert.equal(await page.locator('#sequence-diagram .pele-message[data-trellis-step-id="c-deliver>show"]').getAttribute("data-trellis-target-id"), "show");
  await label("c-deliver>show").click();
  assert.deepEqual(await page.evaluate(() => window.picks[0]), view.steps[6].children[1]);
  assert.match(await label("c-deliver>show").getAttribute("aria-label"), /self\.show_spelling/);
  assert.equal(await page.evaluate(() => TrellisSequence.evidenceLabel(window.view.steps[6].children[1], window.view)), "Resolved declaration");
  await label("c-loop-start").click();
  assert.deepEqual(await page.evaluate(() => window.picks[1]), view.steps[3].children[3]);
});

test("a seed with no arrow says why; a view with no call, Show all steps and syntax views are unchanged", async () => {
  const view = coordinateView();
  for (const step of [view.steps[2], view.steps[4], view.steps[6], view.steps[5].children[0], view.steps[3].children[1], view.steps[3].children[3]]) { step.target = "helper"; step.children = []; }
  view.steps[7].children = [];
  const summary = view => page.evaluate(view => {
    const options = {interactions:true};
    return {summary:TrellisSequence.emptySummary(view, options), visible:TrellisSequence.display(view, options).visible.size,
      all:TrellisSequence.emptySummary(view, {...options, showDetails:true})};
  }, view);
  // A call without an exact target proves nothing about its column: it is counted, not placed.
  assert.deepEqual(await summary(view), {summary:"All resolved calls stay within mcp; 4 calls have no exact target.", visible:0, all:""});
  assert.equal((await summary({...view, steps:[view.steps[2], view.steps[4]]})).summary, "All calls stay within mcp.");
  assert.equal((await summary({...view, steps:[view.steps[9]]})).summary, "The one call has no exact target, so no arrow is drawn between columns.");
  assert.equal((await summary({...view, steps:[view.steps[1], view.steps[9]]})).summary, "None of the 2 calls has an exact target, so no arrow is drawn between columns.");
  // No call at all keeps calls-first, with its context and words; so does a view of hidden calls.
  const quiet = {...view, steps:[view.steps[0]]};
  assert.equal(await page.evaluate(view => TrellisSequence.display(view, {interactions:true}).interactions, quiet), undefined);
  assert.match(await page.evaluate(view => TrellisSequence.emptySummary(view, {interactions:true}), quiet), /^No measured calls in the admitted steps\. Non-call behavior may be hidden/);
  const hidden = {...view, steps:[view.steps.at(-1)]};
  assert.equal(await page.evaluate(view => TrellisSequence.emptySummary(view, {interactions:true}), hidden), "Only external calls (1 hidden).");
  // Show all steps draws what calls-first's detail view draws, and so does a syntax-only view.
  const source = (view, options) => page.evaluate(({view, options}) => TrellisPeleSequence.project(view, new Set(), options).source, {view, options});
  const full = coordinateView();
  assert.equal(await source(full, {interactions:true, showDetails:true}), await source(full, {showDetails:true}));
  const syntax = {...full, semantic:{tier:"syntax", reason:"no semantic producer"}};
  assert.equal(await source(syntax, {interactions:true}), await source(syntax, {}));
  assert.equal(await page.evaluate(view => TrellisSequence.display(view, {interactions:true}).interactions, syntax), undefined);
});

test("an arrow inside a call chain draws without the chain; its other members draw nothing", async () => {
  const view = coordinateView(), [open, start] = [view.steps[2], view.steps[4]];
  const chain = {...structuredClone(open), id:"chain", kind:"group", label:"a.open().start()", callId:null, target:undefined, callEvidence:undefined,
    children:[{...structuredClone(view.steps[9]), id:"chain:u", callId:"callid:chain:u"}, {...structuredClone(start), id:"chain:start", callId:"callid:chain:start"}]};
  view.steps = [chain];
  assert.equal((await render(view)).status, "rendered");
  assert.deepEqual(await page.locator("#sequence-diagram .pele-edge-label").allTextContents(), ["[001] start"]);
  assert.equal(await page.locator("#sequence-diagram .pele-note").count(), 1);
  assert.equal(await page.locator("#sequence-diagram .sequence-group-control").count(), 0, "the chain draws no row of its own");
});

test("a large interactions diagram keeps readable text and the panel scrolls", async () => {
  const view = coordinateView();
  // Many cross-column calls make a tall diagram; each must stay at its own size.
  for (let n = 0; n < 40; n++) view.steps.push({...structuredClone(view.steps[2]), id:`c-open-${n}`, callId:`callid:c-open-${n}`});
  assert.equal((await render(view)).status, "rendered");
  const measured = await page.evaluate(() => {
    const box = document.getElementById("sequence-diagram"), svg = box.querySelector("svg"), scale = svg.getScreenCTM().a;
    const sizes = [...svg.querySelectorAll("text")].filter(node => node.textContent.trim()).map(node => parseFloat(getComputedStyle(node).fontSize) * scale);
    return {scale, smallest:Math.min(...sizes), clientHeight:box.clientHeight, scrollHeight:box.scrollHeight};
  });
  assert.ok(Math.abs(measured.scale - 1) < 0.001 && measured.smallest >= 12, JSON.stringify(measured));
  assert.ok(measured.scrollHeight > measured.clientHeight, JSON.stringify(measured));
});
