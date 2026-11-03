"use strict";
// #636: the Interactions view. Real Pele, real CSS and real geometry in Chromium, over a DTO
// shaped like `coordinate` (src/mcp/mod.rs): most steps stay in the seed's own column and only a
// few calls cross to another type, one of them through a nested chain (Inbox → FrameWindow).
const test = require("node:test");
const assert = require("node:assert/strict");
const {readFileSync} = require("node:fs");
const {createServer} = require("node:http");
const {resolve, join} = require("node:path");
const {chromium} = require("playwright");
const ROOT = resolve(__dirname, "../..");
const files = {"/vendor/pele-1.10.0-trellis.2.js":"web/vendor/pele-1.10.0-trellis.2.js", "/sequence-pele.js":"web/sequence-pele.js", "/sequence.js":"web/sequence.js", "/style.css":"web/style.css"};
const html = '<!doctype html><html><head><link rel="stylesheet" href="/style.css"><script src="/vendor/pele-1.10.0-trellis.2.js" defer></script><script src="/sequence.js" defer></script><script src="/sequence-pele.js" defer></script></head><body><div id="sequence-diagram" class="sequence-scroll" style="flex:none;width:1200px;height:700px"></div></body></html>';
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

// The seed `coordinate` in column `mcp`. Cross-column calls: open, start, refresh, deliver (whose
// expanded body calls FrameWindow.show) and, inside the same-column helper's body, tools.run.
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
    step("loop4", "loop", "for_expression: possible iterations, not unrolled; exit/termination unknown", {children:[unresolved("u2"), call("c-helper-in-loop", "helper")]}),
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
// The same-column helper folds; its body's call to tools still leaves mcp. The branch around
// `refresh` draws no frame: its arrow says it is conditional, and its run spans both arms.
const ARROWS = ["c-open", "c-start", "c-refresh", "c-deliver", "c-deliver>show", "c-helper>run"];
const LABELS = ["[001] open", "[002] start", "[003] refresh · conditional", "[004] deliver", "[005] show", "[006] run"];
const RETURNS = ["c-open", "c-start", "c-refresh", "c-deliver>show", "c-deliver", "c-helper>run"];
const BADGES = [["e1", "▸ +4 internal steps · 1 unresolved"], ["loop4", "▸ +3 internal steps · 1 unresolved"], ["e6", "▸ +2 internal steps · 1 unresolved"],
  ["d1", "▸ +1 internal step"], ["d3", "▸ +1 internal step · 1 unresolved"], ["e9", "▸ +2 internal steps · 1 unresolved"]];

async function render(view, options = INTERACTIONS) {
  return page.evaluate(async ({view, options}) => {
    const container = document.getElementById("sequence-diagram");
    window.view = view; window.picks = []; window.expandedBadges = new Set();
    const result = await TrellisPeleSequence.render(container, view, null, new Set(), {...options, expandedBadges:window.expandedBadges, onSelect:step => window.picks.push(step)});
    return {status:result.status, reason:result.reason};
  }, {view, options});
}
function legacy(view, options = INTERACTIONS) {
  return page.evaluate(({view, options}) => {
    const container = document.getElementById("sequence-diagram");
    window.view = view; window.picks = [];
    TrellisSequence.render(container, view, null, new Set(), {...options, onSelect:step => window.picks.push(step)});
  }, {view, options});
}
const ids = (selector, attribute) => page.locator(selector).evaluateAll((nodes, attribute) => nodes.map(node => node.getAttribute(attribute)), attribute);
const badgeTexts = () => page.locator("#sequence-diagram [data-trellis-badge-id]").evaluateAll(nodes => nodes.map(node => [node.getAttribute("data-trellis-badge-id"), node.textContent]));
const center = locator => locator.evaluate(node => { const box = node.getBoundingClientRect(); return {x:box.left + box.width / 2, y:box.top + box.height / 2, top:box.top, bottom:box.bottom, left:box.left, right:box.right}; });
const actor = name => page.locator("#sequence-diagram .pele-actor:not(.pele-actor-footer)").filter({hasText:new RegExp(`^${name}$`)});

test("Pele draws only cross-column calls, with returns, in source order", async () => {
  const result = await render(coordinateView());
  assert.equal(result.status, "rendered", result.reason);
  assert.deepEqual(await page.locator("#sequence-diagram .pele-actor:not(.pele-actor-footer)").allTextContents(), ["mcp", "OpenedWorkspace", "Session", "Inbox", "FrameWindow", "tools"]);
  assert.deepEqual(await ids("#sequence-diagram .pele-edge-label", "data-trellis-step-id"), ARROWS);
  assert.deepEqual(await page.locator("#sequence-diagram .pele-edge-label").allTextContents(), LABELS);
  assert.deepEqual(await ids("#sequence-diagram .pele-message[data-trellis-return-step-id]", "data-trellis-return-step-id"), RETURNS);
  assert.equal(await page.locator("#sequence-diagram .pele-message").count(), ARROWS.length + RETURNS.length);
  assert.equal(await page.locator("#sequence-diagram .pele-activation").count(), RETURNS.length);
  // No other row: everything else is folded into a badge.
  assert.deepEqual(await ids("#sequence-diagram .pele-note[data-trellis-step-id]", "data-trellis-step-id"), []);
  assert.equal(await page.locator("#sequence-diagram .pele-frame, #sequence-diagram .pele-rect").count(), 0);
  assert.deepEqual(await badgeTexts(), BADGES);
  // Arrows run top to bottom in source order.
  const tops = [];
  for (const id of ARROWS) tops.push((await center(page.locator(`#sequence-diagram .pele-edge-label[data-trellis-step-id="${id}"]`))).y);
  assert.deepEqual(tops, [...tops].sort((a, b) => a - b), JSON.stringify(tops));
});

test("a nested chain starts on its callee's column and returns before its caller does", async () => {
  assert.equal((await render(coordinateView())).status, "rendered");
  const inbox = await center(actor("Inbox")), frame = await center(actor("FrameWindow")), mcp = await center(actor("mcp"));
  const show = await center(page.locator('#sequence-diagram .pele-message[data-trellis-step-id="c-deliver>show"] path').first());
  // An arrow leaves the edge of its caller's activation bar (10 px wide) and its head (7 px)
  // touches the callee's.
  assert.ok(Math.abs(show.left - inbox.x) <= 6 && Math.abs(show.right - frame.x) <= 13, `Inbox → FrameWindow: ${JSON.stringify({show, inbox, frame})}`);
  const back = await center(page.locator('#sequence-diagram .pele-message[data-trellis-return-step-id="c-deliver"] path').first());
  assert.ok(Math.abs(back.right - inbox.x) <= 6 && Math.abs(back.left - mcp.x) <= 8, `deliver returns to mcp: ${JSON.stringify({back, inbox, mcp})}`);
  const showBack = await center(page.locator('#sequence-diagram .pele-message[data-trellis-return-step-id="c-deliver>show"] path').first());
  assert.ok(show.y < showBack.y && showBack.y < back.y);
  // Badges sit on their own column: Inbox's two runs on Inbox's lifeline, around the nested call.
  const first = await center(page.locator('#sequence-diagram [data-trellis-badge-id="d1"] rect'));
  const last = await center(page.locator('#sequence-diagram [data-trellis-badge-id="d3"] rect'));
  assert.ok(Math.abs(first.x - inbox.x) < 2 && Math.abs(last.x - inbox.x) < 2, JSON.stringify({first, last, inbox}));
  assert.ok(first.y < show.y && show.y < last.y);
  assert.ok(Math.abs((await center(page.locator('#sequence-diagram [data-trellis-badge-id="e1"] rect'))).x - mcp.x) < 2);
});

test("a Pele badge expands in place to its exact steps and folds again", async () => {
  const view = coordinateView();
  assert.equal((await render(view)).status, "rendered");
  const badge = page.locator('#sequence-diagram [data-trellis-badge-id="e1"]');
  assert.equal(await badge.getAttribute("aria-expanded"), "false");
  assert.equal(await badge.getAttribute("aria-label"), "Show 4 internal steps, 1 unresolved");
  await badge.click();
  await page.waitForFunction(() => document.querySelector('[data-trellis-badge-id="e1"]')?.getAttribute("aria-expanded") === "true");
  const opened = page.locator('#sequence-diagram [data-trellis-badge-id="e1"]');
  assert.equal(await opened.textContent(), "▾ 4 internal steps · 1 unresolved");
  assert.equal(await page.evaluate(() => document.activeElement?.getAttribute("data-trellis-badge-id")), "e1", "focus stays on the badge");
  assert.deepEqual(await ids("#sequence-diagram .pele-note[data-trellis-step-id]", "data-trellis-step-id"), ["e1", "if2", "u1", "ret2"]);
  assert.deepEqual(await ids("#sequence-diagram .pele-edge-label", "data-trellis-step-id"), ARROWS, "the arrows do not change");
  // In place: the run's rows sit between the badge and the first arrow.
  const below = (await center(opened.locator("rect"))).y, row = (await center(page.locator('#sequence-diagram .pele-note[data-trellis-step-id="u1"]'))).y;
  const open = (await center(page.locator('#sequence-diagram .pele-edge-label[data-trellis-step-id="c-open"]'))).y;
  assert.ok(below < row && row < open, JSON.stringify({below, row, open}));
  // A revealed step selects the original DTO step.
  await page.locator('#sequence-diagram .pele-note[data-trellis-step-id="u1"]').click();
  assert.deepEqual(await page.evaluate(() => window.picks.at(-1)), view.steps[1].children[0]);
  await opened.focus(); await page.keyboard.press("Enter");
  await page.waitForFunction(() => document.querySelector('[data-trellis-badge-id="e1"]')?.getAttribute("aria-expanded") === "false");
  assert.deepEqual(await ids("#sequence-diagram .pele-note[data-trellis-step-id]", "data-trellis-step-id"), []);
  assert.deepEqual(await badgeTexts(), BADGES);
});

test("selecting an arrow hands the inspector the original step and evidence", async () => {
  const view = coordinateView();
  assert.equal((await render(view)).status, "rendered");
  const label = page.locator('#sequence-diagram .pele-edge-label[data-trellis-step-id="c-deliver>show"]');
  assert.equal(await page.locator('#sequence-diagram .pele-message[data-trellis-step-id="c-deliver>show"]').getAttribute("data-trellis-target-id"), "show");
  await label.click();
  assert.deepEqual(await page.evaluate(() => window.picks[0]), view.steps[6].children[1]);
  assert.match(await label.getAttribute("aria-label"), /self\.show_spelling/);
  assert.equal(await page.evaluate(() => TrellisSequence.evidenceLabel(window.view.steps[6].children[1], window.view)), "Resolved declaration");
});

test("the legacy renderer draws the same arrows, returns and badges, and expands in place", async () => {
  const view = coordinateView();
  assert.equal((await render(view)).status, "rendered");
  const pele = await page.locator("#sequence-diagram .pele-edge-label").allTextContents();
  await legacy(view);
  assert.deepEqual(await page.locator("#sequence-diagram .sequence-participant-label").evaluateAll(nodes => nodes.map(node => node.firstChild.textContent)), ["mcp", "OpenedWorkspace", "Session", "Inbox", "FrameWindow", "tools"]);
  assert.deepEqual(await ids("#sequence-diagram .sequence-arrow[data-trellis-call-id]", "data-trellis-call-id"), ARROWS.map(id => `callid:${id}`));
  const rows = await page.locator("#sequence-diagram .sequence-source").evaluateAll((nodes, arrows) => nodes.filter(node => arrows.includes(node.dataset.sourceStepId)).map(node => node.querySelector("text:not(.sequence-fragment-operator)").firstChild.textContent), ARROWS);
  assert.deepEqual(rows, pele, "the same arrow labels in both renderers");
  assert.deepEqual(await ids("#sequence-diagram .sequence-return[data-trellis-return-step-id]", "data-trellis-return-step-id"), RETURNS);
  assert.equal(await page.locator("#sequence-diagram .sequence-activation").count(), RETURNS.length);
  assert.deepEqual(await ids("#sequence-diagram .sequence-source", "data-source-step-id"), ARROWS);
  assert.deepEqual(rows, LABELS);
  assert.deepEqual(await badgeTexts(), BADGES);
  // Geometry: the nested call leaves Inbox's lifeline and the badge sits on it.
  const inbox = await page.locator("#sequence-diagram .sequence-participant-label").evaluateAll(nodes => nodes.find(node => node.firstChild.textContent === "Inbox").getAttribute("x"));
  assert.match(await page.locator('#sequence-diagram .sequence-arrow[data-trellis-call-id="callid:c-deliver>show"]').getAttribute("d"), new RegExp(`^M ${inbox} `));
  assert.equal(await page.locator('#sequence-diagram [data-trellis-badge-id="d1"] text').getAttribute("x"), inbox);
  const badge = page.locator('#sequence-diagram [data-trellis-badge-id="loop4"]');
  await badge.click();
  assert.equal(await page.locator('#sequence-diagram [data-trellis-badge-id="loop4"]').getAttribute("aria-expanded"), "true");
  assert.deepEqual(await ids("#sequence-diagram .sequence-source", "data-source-step-id"), ["c-open", "loop4", "u2", "c-helper-in-loop", "c-start", "c-refresh", "c-deliver", "c-deliver>show", "c-helper>run"]);
  assert.equal(await page.evaluate(() => document.activeElement?.getAttribute("data-trellis-badge-id")), "loop4");
  await page.locator('#sequence-diagram [data-source-step-id="u2"]').click();
  assert.deepEqual(await page.evaluate(() => window.picks.at(-1)), view.steps[3].children[0]);
  await page.keyboard.press("Tab"); // Away and back by keyboard: the badge is a real control.
  await page.locator('#sequence-diagram [data-trellis-badge-id="loop4"]').focus(); await page.keyboard.press("Enter");
  assert.equal(await page.locator('#sequence-diagram [data-trellis-badge-id="loop4"]').getAttribute("aria-expanded"), "false");
  assert.deepEqual(await badgeTexts(), BADGES);
});

test("a seed whose calls all stay in its column says so; Show all steps and syntax views are unchanged", async () => {
  const view = coordinateView();
  for (const step of [view.steps[2], view.steps[4], view.steps[6], view.steps[5].children[0]]) { step.target = "helper"; step.children = []; }
  view.steps[7].children = [];
  const state = await page.evaluate(view => {
    const options = {interactions:true};
    return {summary:TrellisSequence.emptySummary(view, options), visible:TrellisSequence.display(view, options).visible.size,
      all:TrellisSequence.emptySummary(view, {...options, showDetails:true})};
  }, view);
  assert.deepEqual(state, {summary:"All calls stay within mcp.", visible:0, all:""});
  // No calls at all, and only hidden calls, keep their own words.
  const quiet = {...view, steps:[view.steps[0]]};
  assert.match(await page.evaluate(view => TrellisSequence.emptySummary(view, {interactions:true}), quiet), /^No measured calls in the admitted steps\. Non-call behavior is folded/);
  const hidden = {...view, steps:[view.steps.at(-1)]};
  assert.equal(await page.evaluate(view => TrellisSequence.emptySummary(view, {interactions:true}), hidden), "Only external calls (1 hidden).");
  // Show all steps draws exactly what it drew before Interactions, and so does a syntax-only view.
  const source = (view, options) => page.evaluate(({view, options}) => TrellisPeleSequence.project(view, new Set(), options).source, {view, options});
  const full = coordinateView();
  assert.equal(await source(full, {interactions:true, showDetails:true}), await source(full, {showDetails:true}));
  const syntax = {...full, semantic:{tier:"syntax", reason:"no semantic producer"}};
  assert.equal(await source(syntax, {interactions:true}), await source(syntax, {}));
  assert.equal(await page.evaluate(view => TrellisSequence.interactive(view, {interactions:true}), syntax), false);
});

test("a large interactions diagram keeps readable text and the panel scrolls", async () => {
  const view = coordinateView();
  // Many cross-column calls make a tall diagram; each must stay at its own size.
  const calls = view.steps.filter(step => step.id === "c-open");
  for (let n = 0; n < 40; n++) view.steps.push({...structuredClone(calls[0]), id:`c-open-${n}`, callId:`callid:c-open-${n}`});
  assert.equal((await render(view)).status, "rendered");
  const measured = await page.evaluate(() => {
    const box = document.getElementById("sequence-diagram"), svg = box.querySelector("svg"), scale = svg.getScreenCTM().a;
    const sizes = [...svg.querySelectorAll("text")].filter(node => node.textContent.trim()).map(node => parseFloat(getComputedStyle(node).fontSize) * scale);
    return {scale, smallest:Math.min(...sizes), clientHeight:box.clientHeight, scrollHeight:box.scrollHeight};
  });
  assert.ok(Math.abs(measured.scale - 1) < 0.001 && measured.smallest >= 12, JSON.stringify(measured));
  assert.ok(measured.scrollHeight > measured.clientHeight, JSON.stringify(measured));
});
