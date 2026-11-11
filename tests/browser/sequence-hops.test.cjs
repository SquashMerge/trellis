"use strict";
// #853: Sequence crosses apps at a matched HTTP interface as a labelled boundary hop. Real Pele,
// real CSS and real geometry in Chromium, over a DTO shaped like the daemon's: the web app's
// `load` calls its `api` wrapper (same column) five times. Three calls match routes of the
// `daemon` app (exact, template, wrapperInferred) and carry an `interface` hop to a handler
// participant in the column `app:daemon`; two stay unresolved boundaries.
const test = require("node:test");
const assert = require("node:assert/strict");
const {readFileSync} = require("node:fs");
const {createServer} = require("node:http");
const {resolve, join} = require("node:path");
const {chromium} = require("playwright");
const ROOT = resolve(__dirname, "../..");
const files = {"/vendor/pele-1.10.0-trellis.2.js":"web/vendor/pele-1.10.0-trellis.2.js", "/sequence-pele.js":"web/sequence-pele.js", "/sequence.js":"web/sequence.js", "/style.css":"web/style.css"};
const html = '<!doctype html><html><head><link rel="stylesheet" href="/style.css"><script src="/vendor/pele-1.10.0-trellis.2.js" defer></script><script src="/sequence.js" defer></script><script src="/sequence-pele.js" defer></script></head><body><div id="sequence-diagram" class="sequence-scroll" style="flex:none;width:900px;height:240px"></div></body></html>';
let server, browser, page;

test.before(async () => {
  server = createServer((req, res) => {
    const file = files[req.url];
    if (file) { res.setHeader("Content-Type", file.endsWith("css") ? "text/css; charset=utf-8" : "text/javascript; charset=utf-8"); res.end(readFileSync(join(ROOT, file))); return; }
    if (req.url === "/") { res.setHeader("Content-Type", "text/html"); res.end(html); return; }
    res.writeHead(404); res.end();
  });
  await new Promise(done => server.listen(0, "127.0.0.1", done));
  browser = await chromium.launch(); page = await browser.newPage({viewport:{width:1200, height:800}});
  await page.goto(`http://127.0.0.1:${server.address().port}`);
  await page.waitForFunction(() => !!window.TrellisPeleSequence && !!window.pele);
  // #860: CI draws classic 15px scrollbars; local Macs overlay them. Emulate the classic one in
  // the canvas so every host measures as CI does, as architecture-geometry.cjs does: the rules go
  // in through the CSSOM. An overlay host reserves the width only in a stable gutter; the
  // drawing overflows the canvas below, so a classic scrollbar takes the same 15px on CI.
  await page.evaluate(() => {
    const sheet = [...document.styleSheets].find(item => item.href?.endsWith("/style.css"));
    sheet.insertRule("#sequence-diagram { scrollbar-gutter: stable; }", sheet.cssRules.length);
    sheet.insertRule("#sequence-diagram::-webkit-scrollbar { width: 15px; height: 15px; }", sheet.cssRules.length);
  });
});
test.after(async () => { await browser?.close(); await new Promise(done => server?.close(done)); });

const HOPS = {"c-exact":["POST", "/api/sequence", "exact", "sequence"], "c-template":["GET", "/api/items/{id}", "template", "item"], "c-wrapper":["GET", "/api/checkouts/{root}/status/head", "wrapperInferred", "head"]};
function hopView({hops = true} = {}) {
  const view = JSON.parse(readFileSync(join(ROOT, "tests/fixtures/sequence-pele/javascript.json")));
  const template = view.steps[0], seed = view.seed;
  seed.name = "load"; seed.path = "web/app.js";
  view.semantic = {tier:"semantic", producerId:"scip-typescript"};
  const declaration = (id, name, path) => ({...structuredClone(seed), id, name, path, kind:"function"});
  view.participants = [
    {id:seed.id, label:"load", kind:"method", identification:"Selected source symbol.", column:{id:"module:app", label:"app"}, declaration:seed},
    {id:"api", label:"api", kind:"method", identification:"Fresh exact target", column:{id:"module:app", label:"app"}, declaration:declaration("api", "api", "web/app.js")},
  ];
  const step = (id, extra = {}) => ({...structuredClone(template), id, kind:"call", label:`api(${id})`, callId:`callid:${id}`, children:[], alternate:[], hidden:false, target:"api",
    callEvidence:{boundaryReason:"none", resolution:"resolved", dispatch:"direct", freshness:"fresh", provenanceId:`proof:${id}`}, ...extra});
  view.steps = Object.keys(HOPS).map(id => step(id));
  view.steps.push(step("c-dynamic"), step("c-nowhere"));
  if (hops) {
    for (const [id, [method, pathTemplate, confidence, handler]] of Object.entries(HOPS)) {
      const participant = `interface:daemon:${handler}`;
      view.participants.push({id:participant, label:handler, kind:"interfaceHandler", identification:"HTTP interface handler: a structural method and path match, not proof.",
        column:{id:"app:daemon", label:"daemon"}, declaration:declaration(`syntax:${handler}`, handler, "src/http.rs")});
      view.steps.find(item => item.id === id).interface = {kind:"http", method, pathTemplate, confidence, clientApp:"web", serverApp:"daemon", handlerKind:"declaration", participant};
    }
    view.steps.find(item => item.id === "c-dynamic").interface = {kind:"http", method:"GET", pathTemplate:null, confidence:"unresolved", clientApp:"web", serverApp:null};
    view.steps.find(item => item.id === "c-nowhere").interface = {kind:"http", method:"GET", pathTemplate:"/api/nowhere", confidence:"unresolved", clientApp:"web", serverApp:null};
    view.interfaces = {state:"ready", hops:3, unresolved:2};
  }
  view.warnings = []; view.hiddenSteps = 0; view.incidentalSteps = 0;
  return view;
}
const LABELS = [
  "[001] POST /api/sequence · exact · structural match, not proof",
  "[002] GET /api/items/{id} · template · structural match, not proof",
  "[003] GET /api/checkouts/{root}/status/head · wrapper-inferred · structural match, not proof",
];

async function render(view, options = {interactions:true}) {
  return page.evaluate(async ({view, options}) => {
    const container = document.getElementById("sequence-diagram");
    window.view = view; window.picks = []; window.opened = [];
    const result = await TrellisPeleSequence.render(container, view, null, new Set(), {...options, onSelect:step => window.picks.push(step), onOpenTarget:(declaration, step) => window.opened.push([declaration.id, step.id])});
    return {status:result.status, reason:result.reason};
  }, {view, options});
}
function legacy(view, options = {interactions:true}) {
  return page.evaluate(({view, options}) => {
    TrellisSequence.render(document.getElementById("sequence-diagram"), view, null, new Set(), options);
  }, {view, options});
}
const ids = (selector, attribute) => page.locator(selector).evaluateAll((nodes, attribute) => nodes.map(node => node.getAttribute(attribute)), attribute);
const box = locator => locator.evaluate(node => { const box = node.getBoundingClientRect(); return {x:box.left + box.width / 2, y:box.top + box.height / 2, left:box.left, right:box.right, top:box.top, bottom:box.bottom}; });
const headers = selector => page.locator(selector).evaluateAll(nodes => nodes.map(node => {
  const lines = [...node.querySelectorAll("tspan")].map(line => line.textContent);
  return lines.length ? lines.join(" ") : node.textContent;
}));

test("matched calls hop into the server app's lane, labelled with method, path, confidence and not proof", async () => {
  const result = await render(hopView());
  assert.equal(result.status, "rendered", result.reason);
  assert.deepEqual(await headers("#sequence-diagram .pele-actor:not(.pele-actor-footer)"), ["«module» app", "«app» daemon"]);
  assert.deepEqual(await ids("#sequence-diagram .pele-edge-label", "data-trellis-step-id"), Object.keys(HOPS));
  assert.deepEqual(await page.locator("#sequence-diagram .pele-edge-label").allTextContents(), LABELS);
  // Each hop has its response arrow back to the web app.
  assert.deepEqual(await ids("#sequence-diagram .pele-message[data-trellis-return-step-id]", "data-trellis-return-step-id"), Object.keys(HOPS));
  assert.deepEqual(await ids("#sequence-diagram .pele-message[data-trellis-step-id]:not([data-trellis-return-step-id])", "data-trellis-target-id"),
    ["interface:daemon:sequence", "interface:daemon:item", "interface:daemon:head"]);
  // Unresolved calls stay boundaries: no arrow, no lane, no guessed handler.
  for (const id of ["c-dynamic", "c-nowhere"]) assert.equal(await page.locator(`#sequence-diagram [data-trellis-call-id="callid:${id}"]`).count(), 0);
  const status = await page.evaluate(() => [TrellisSequence.interfaceSummary(window.view), TrellisSequence.display(window.view, {interactions:true}).hiddenSteps]);
  assert.deepEqual(status, ["3 HTTP interface hops (structural matches, not proof) · 2 unresolved HTTP calls", 2]);
});

test("the hop arrow ends on the app lane's lifeline under a classic scrollbar, and its label is not clipped", async () => {
  assert.equal((await render(hopView())).status, "rendered");
  // The drawing is taller than the canvas, so the canvas scrolls and draws its scrollbar.
  const gutter = await page.evaluate(() => { const canvas = document.querySelector("#sequence-diagram"); return canvas.offsetWidth - canvas.clientWidth; });
  assert.equal(gutter, 15, "the canvas draws a classic 15px scrollbar, as on CI");
  const app = await box(page.locator('#sequence-diagram .pele-lifeline[data-id="S"]')), daemon = await box(page.locator('#sequence-diagram .pele-lifeline[data-id="P1"]'));
  for (const id of Object.keys(HOPS)) {
    const arrow = await box(page.locator(`#sequence-diagram .pele-message[data-trellis-step-id="${id}"]:not([data-trellis-return-step-id]) path`).first());
    assert.ok(Math.abs(arrow.left - app.x) <= 8 && Math.abs(arrow.right - daemon.x) <= 13, `${id} web → daemon: ${JSON.stringify({arrow, app, daemon})}`);
    const back = await box(page.locator(`#sequence-diagram .pele-message[data-trellis-return-step-id="${id}"] path`).first());
    assert.ok(Math.abs(back.right - daemon.x) <= 8 && Math.abs(back.left - app.x) <= 13, `${id} returns: ${JSON.stringify({back, app, daemon})}`);
  }
  // The longest label is whole in the scrollable drawing: its text was not cut.
  const svg = await box(page.locator("#sequence-diagram svg")), longest = await box(page.locator('#sequence-diagram .pele-edge-label[data-trellis-step-id="c-wrapper"]'));
  assert.ok(longest.left >= svg.left - 1 && longest.right <= svg.right + 1, JSON.stringify({longest, svg}));
});

test("the handler lane and the inspector continue into the handler's own Sequence on demand", async () => {
  const view = hopView();
  assert.equal((await render(view)).status, "rendered");
  const header = page.locator('#sequence-diagram .pele-actor:not(.pele-actor-footer)[data-id="P1"]');
  // Three handlers share the daemon's column, so the lane names no single target to open.
  assert.equal(await header.getAttribute("role"), null);
  await page.locator('#sequence-diagram .pele-edge-label[data-trellis-step-id="c-exact"]').click();
  const picked = await page.evaluate(() => {
    const step = window.picks[0], target = TrellisSequence.target(window.view, step);
    return {id:step.id, target:target.id, action:TrellisSequence.targetActionLabel(target.declaration, target), evidence:TrellisSequence.evidenceLabel(step, window.view), text:TrellisSequence.interfaceText(step, window.view)};
  });
  assert.deepEqual(picked, {id:"c-exact", target:"interface:daemon:sequence", action:"Open handler sequence", evidence:"HTTP interface hop · POST /api/sequence · exact · structural match, not proof",
    text:"HTTP interface hop · POST /api/sequence · exact · web → daemon · structural match by method and path, not proof the request is sent"});
  // One hop: the lane names one handler, so its header opens that handler's Sequence.
  const single = hopView();
  single.steps = single.steps.filter(step => step.id === "c-exact");
  assert.equal((await render(single)).status, "rendered");
  const one = page.locator('#sequence-diagram .pele-actor:not(.pele-actor-footer)[data-id="P1"]');
  assert.equal(await one.getAttribute("aria-label"), "Open handler sequence: sequence");
  await one.click();
  assert.deepEqual(await page.evaluate(() => window.opened), [["syntax:sequence", "c-exact"]]);
  const unresolved = await page.evaluate(view => TrellisSequence.interfaceText(view.steps[4], view), view);
  assert.equal(unresolved, "HTTP client call · GET /api/nowhere · unresolved: no route matched · a missing match proves nothing");
  // A match the participant limit left undrawn is told in the inspector, never drawn.
  const limited = hopView();
  delete limited.steps[0].interface.participant;
  assert.equal(await page.evaluate(view => [TrellisSequence.hop(view, view.steps[0]), TrellisSequence.interfaceText(view.steps[0], view)], limited).then(([hop, text]) => hop === null && text),
    "HTTP interface · POST /api/sequence · exact · web → daemon · not drawn: sequence participant limit · structural match, not proof");
});

test("negative control: without interface evidence the same calls draw exactly as before", async () => {
  const plain = hopView({hops:false});
  assert.equal((await render(plain)).status, "rendered");
  // Every call targets `api` in the seed's own column: Interactions draws no arrow and no lane.
  assert.deepEqual(await headers("#sequence-diagram .pele-actor:not(.pele-actor-footer)"), ["«module» app"]);
  assert.equal(await page.locator("#sequence-diagram .pele-edge-label").count(), 0);
  assert.equal(await page.evaluate(view => TrellisSequence.interfaceSummary(view), plain), "");
  // Forged or inconsistent hops are ignored, never drawn: an unresolved hop naming a handler, a
  // column of another app, and an unknown confidence.
  for (const forge of [
    hop => { hop.confidence = "unresolved"; },
    hop => { hop.serverApp = "other"; },
    hop => { hop.confidence = "certain"; },
  ]) {
    const view = hopView();
    for (const step of view.steps) if (step.interface?.participant) forge(step.interface);
    assert.equal((await render(view)).status, "rendered");
    assert.equal(await page.locator("#sequence-diagram .pele-edge-label").count(), 0, JSON.stringify(view.steps[0].interface));
  }
});

test("the legacy renderer draws the same hops, labels and returns", async () => {
  const view = hopView();
  assert.equal((await render(view)).status, "rendered");
  const pele = await page.locator("#sequence-diagram .pele-edge-label").allTextContents();
  await legacy(view);
  assert.deepEqual(await headers("#sequence-diagram .sequence-participant-label"), ["«module» app", "«app» daemon"]);
  assert.deepEqual(await ids("#sequence-diagram .sequence-arrow[data-trellis-call-id]", "data-trellis-call-id"), Object.keys(HOPS).map(id => `callid:${id}`));
  const rows = await page.locator("#sequence-diagram .sequence-source").evaluateAll(nodes => nodes.map(node => node.querySelector("text").firstChild.textContent));
  assert.deepEqual(rows, pele, "the same arrow labels in both renderers");
  assert.deepEqual(await ids("#sequence-diagram .sequence-return[data-trellis-return-step-id]", "data-trellis-return-step-id").then(list => [...new Set(list)]), Object.keys(HOPS));
});

test("calls-first (a syntax-only view) draws the same hops as arrows and keeps unresolved calls as rows", async () => {
  const view = hopView();
  view.semantic = {tier:"syntax", reason:"no producer"};
  for (const step of view.steps) { delete step.callEvidence; delete step.target; }
  view.participants = view.participants.filter(participant => participant.id !== "api");
  assert.equal((await render(view, {})).status, "rendered");
  assert.deepEqual(await page.locator("#sequence-diagram .pele-edge-label").allTextContents(), LABELS);
  for (const id of ["c-dynamic", "c-nowhere"]) assert.equal(await page.locator(`#sequence-diagram .pele-note[data-trellis-call-id="callid:${id}"]`).count(), 1);
});

// Prime2 review on #885: Show all steps keeps the matched route on the hop, in both renderers.
test("Show all keeps the matched method and path template on every hop, in both renderers", async () => {
  const view = hopView();
  view.semantic = {tier:"syntax", reason:"no producer"};
  for (const step of view.steps) { delete step.callEvidence; delete step.target; }
  view.participants = view.participants.filter(participant => participant.id !== "api");
  const routes = Object.values(HOPS).map(([method, path]) => `${method} ${path}`);
  assert.equal((await render(view, {showDetails:true})).status, "rendered");
  const pele = await page.locator("#sequence-diagram .pele-edge-label").allTextContents();
  assert.equal(pele.length, 3, JSON.stringify(pele));
  pele.forEach((text, at) => assert.ok(text.includes(routes[at]) && /structural match, not proof/.test(text), text));
  await legacy(view, {showDetails:true});
  const rows = await page.locator("#sequence-diagram .sequence-source[data-trellis-call-id]").evaluateAll(nodes => nodes.map(node => [node.getAttribute("data-trellis-call-id"), node.querySelector("text").textContent]));
  for (const [at, id] of Object.keys(HOPS).entries()) {
    const row = rows.find(([callId]) => callId === `callid:${id}`);
    assert.ok(row && row[1].includes(routes[at]), JSON.stringify(rows));
  }
});

// Prime2 review on #885: a wrapper in its own column keeps its lane when its call hops, so its
// expanded body's arrows start from the wrapper, in both renderers.
function wrapperView() {
  const view = hopView();
  view.steps = view.steps.filter(step => step.id === "c-exact");
  const api = view.participants.find(participant => participant.id === "api");
  api.column = {id:"module:client", label:"client"};
  const trace = {...structuredClone(api), id:"trace", label:"trace", column:{id:"module:trace", label:"trace"}, declaration:{...structuredClone(api.declaration), id:"trace", name:"trace", path:"web/trace.js"}};
  view.participants.push(trace);
  const outer = view.steps[0];
  outer.children = [{...structuredClone(outer), id:"c-exact>trace", label:"trace()", callId:"callid:c-exact>trace", target:"trace", lane:"api", interface:undefined, children:[],
    callEvidence:{...outer.callEvidence, provenanceId:"proof:trace"}}];
  delete outer.children[0].interface;
  view.interfaces = {state:"ready", hops:1, unresolved:0};
  return view;
}
test("an expanded wrapper in its own column keeps its lane: its body's arrow starts there, in both renderers", async () => {
  const view = wrapperView();
  assert.equal((await render(view)).status, "rendered");
  const peleHeaders = await headers("#sequence-diagram .pele-actor:not(.pele-actor-footer)");
  assert.ok(peleHeaders.includes("«module» client"), JSON.stringify(peleHeaders));
  const client = await box(page.locator("#sequence-diagram .pele-actor:not(.pele-actor-footer)").filter({hasText:/client$/}));
  const peleChild = await box(page.locator('#sequence-diagram .pele-message[data-trellis-step-id="c-exact>trace"]:not([data-trellis-return-step-id]) path').first());
  // The child runs between the wrapper's and trace's lanes, never from the seed's.
  const seedX = (await box(page.locator('#sequence-diagram .pele-lifeline[data-id="S"]'))).x;
  const trace = await box(page.locator("#sequence-diagram .pele-actor:not(.pele-actor-footer)").filter({hasText:/trace$/}));
  const ends = [peleChild.left, peleChild.right].sort((a, b) => Math.abs(a - client.x) - Math.abs(b - client.x));
  assert.ok(Math.abs(ends[0] - client.x) <= 13 && Math.abs(ends[1] - trace.x) <= 13 && Math.abs(peleChild.left - seedX) > 20, `Pele child client → trace: ${JSON.stringify({peleChild, client, trace, seedX})}`);
  await legacy(view);
  const legacyHeaders = await headers("#sequence-diagram .sequence-participant-label");
  assert.ok(legacyHeaders.includes("«module» client"), JSON.stringify(legacyHeaders));
  const geometry = await page.evaluate(() => {
    const label = [...document.querySelectorAll("#sequence-diagram .sequence-participant-label")].find(node => node.textContent.includes("client"));
    const arrow = document.querySelector('#sequence-diagram .sequence-arrow[data-trellis-call-id="callid:c-exact>trace"]');
    const back = document.querySelector('#sequence-diagram .sequence-return[data-trellis-return-step-id="c-exact>trace"]');
    const start = d => Number(/^M ([\d.]+)/.exec(d)[1]);
    return {client:Number(label.getAttribute("x")), from:start(arrow.getAttribute("d")), backTo:back ? Number(/H ([\d.]+)/.exec(back.getAttribute("d"))[1]) : null};
  });
  assert.equal(geometry.from, geometry.client, JSON.stringify(geometry));
  assert.equal(geometry.backTo, geometry.client, JSON.stringify(geometry));
});
