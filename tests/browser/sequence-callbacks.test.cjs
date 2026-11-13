"use strict";
// #888 (option C): a callback's body is shown as a `callback` block under the call it is passed
// to, only when it holds an interaction or an HTTP interface hop (#853). Real Pele, real CSS and
// real geometry in Chromium, over a DTO shaped like the daemon's: `loadSequence` passes a
// callback to `browseRequest` (same column) whose `api("/api/sequence")` call hops into the
// `trellis` app. Any other callback stays the boundary it always was.
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
  // #860: emulate CI's classic 15px scrollbar in the canvas, as architecture-geometry.cjs does.
  await page.evaluate(() => {
    const sheet = [...document.styleSheets].find(item => item.href?.endsWith("/style.css"));
    sheet.insertRule("#sequence-diagram { scrollbar-gutter: stable; }", sheet.cssRules.length);
    sheet.insertRule("#sequence-diagram::-webkit-scrollbar { width: 15px; height: 15px; }", sheet.cssRules.length);
  });
});
test.after(async () => { await browser?.close(); await new Promise(done => server?.close(done)); });

const BOUNDARY = "Nested function/callback/class boundary: bodies are not executed here; class definition effects (base, computed keys, static initialization) are not expanded.";
const BLOCK = "callback passed to browseRequest · when and how often it runs is unknown";
const HOP = "[001] POST /api/sequence · exact · structural match, not proof";

/// `loadSequence`'s view. `shown`: the daemon found the hop in the callback and shows its block
/// under `browseRequest`; otherwise the boundary stays as it always was.
function callbackView({shown = true, tier = "semantic"} = {}) {
  const view = JSON.parse(readFileSync(join(ROOT, "tests/fixtures/sequence-pele/javascript.json")));
  const template = view.steps[0], seed = view.seed;
  seed.name = "loadSequence"; seed.path = "web/app.js";
  view.semantic = tier === "semantic" ? {tier, producerId:"scip-typescript"} : {tier, reason:"no producer"};
  const declaration = (id, name, path) => ({...structuredClone(seed), id, name, path, kind:"function"});
  const column = {id:"module:app", label:"app"};
  view.participants = [{id:seed.id, label:"loadSequence", kind:"method", identification:"Selected source symbol.", column, declaration:seed}];
  const evidence = id => tier === "semantic" ? {callEvidence:{boundaryReason:"none", resolution:"resolved", dispatch:"direct", freshness:"fresh", provenanceId:`proof:${id}`}} : {};
  const step = (id, kind, label, extra = {}) => ({...structuredClone(template), id, kind, label, callId:null, children:[], alternate:[], hidden:false, ...extra});
  if (tier === "semantic") {
    for (const name of ["browseRequest", "api"]) view.participants.push({id:name, label:name, kind:"function", identification:"Fresh exact target", column, declaration:declaration(name, name, "web/app.js")});
  }
  const target = name => tier === "semantic" ? {target:name, ...evidence(name)} : {};
  const outer = step("browse", "call", "browseRequest", {callId:"callid:browse", ...target("browseRequest")});
  const boundary = step("callback", "boundary", BOUNDARY);
  if (shown) {
    view.participants.push({id:"interface:trellis:sequence", label:"sequence", kind:"interfaceHandler", identification:"HTTP interface handler: a structural method and path match, not proof.",
      column:{id:"app:trellis", label:"trellis"}, declaration:declaration("syntax:sequence", "sequence", "src/http.rs")});
    const api = step("callback>api", "call", "api", {callId:"callid:api", ...target("api"),
      interface:{kind:"http", method:"POST", pathTemplate:"/api/sequence", confidence:"exact", clientApp:"web", serverApp:"trellis", handlerKind:"declaration", participant:"interface:trellis:sequence"}});
    outer.children = [step("callback", "callback", BLOCK, {children:[api, step("callback>await", "await", "await: suspension/resumption boundary, timing unknown")]})];
    view.steps = [outer];
    view.interfaces = {state:"ready", hops:1, unresolved:0};
  } else {
    view.steps = [boundary, outer];
    view.interfaces = {state:"ready", hops:0, unresolved:0};
  }
  view.steps.push(step("await", "await", "await: suspension/resumption boundary, timing unknown"));
  view.warnings = []; view.hiddenSteps = 0; view.incidentalSteps = 0;
  return view;
}

async function render(view, options = {interactions:true}) {
  return page.evaluate(async ({view, options}) => {
    const container = document.getElementById("sequence-diagram");
    window.view = view;
    const result = await TrellisPeleSequence.render(container, view, null, new Set(), options);
    return {status:result.status, reason:result.reason};
  }, {view, options});
}
const box = locator => locator.evaluate(node => { const box = node.getBoundingClientRect(); return {x:box.left + box.width / 2, left:box.left, right:box.right, top:box.top, bottom:box.bottom}; });
const headers = selector => page.locator(selector).evaluateAll(nodes => nodes.map(node => {
  const lines = [...node.querySelectorAll("tspan")].map(line => line.textContent);
  return lines.length ? lines.join(" ") : node.textContent;
}));
const PROJECTIONS = [["Interactions", {interactions:true}], ["calls-first", {}], ["Show all", {showDetails:true}]];

test("a hop inside a callback is drawn from the seed's lane into the app's, in every projection, in Pele", async () => {
  for (const [name, options] of PROJECTIONS) {
    const result = await render(callbackView(), options);
    assert.equal(result.status, "rendered", `${name}: ${result.reason}`);
    assert.deepEqual(await headers("#sequence-diagram .pele-actor:not(.pele-actor-footer)"), ["«module» app", "«app» trellis"], name);
    const labels = await page.locator("#sequence-diagram .pele-edge-label").allTextContents();
    assert.equal(labels.filter(text => text.includes("POST /api/sequence") && text.includes("structural match, not proof")).length, 1, `${name}: ${JSON.stringify(labels)}`);
    if (name === "Interactions") assert.deepEqual(labels, [HOP]);
  }
  // Calls-first and Show all frame the callback's body, with its own numbered row.
  for (const [name, options] of PROJECTIONS.slice(1)) {
    await render(callbackView(), options);
    const notes = await page.locator("#sequence-diagram .pele-note").allTextContents();
    assert.ok(notes.some(text => text.includes("callback passed to browseRequest")), `${name}: ${JSON.stringify(notes)}`);
    assert.ok(await page.locator("#sequence-diagram .pele-rect").count() >= 1, name);
  }
});

test("the hop arrow inside a callback ends on the app lane's lifeline under a classic scrollbar", async () => {
  assert.equal((await render(callbackView())).status, "rendered");
  const app = await box(page.locator('#sequence-diagram .pele-lifeline[data-id="S"]')), trellis = await box(page.locator('#sequence-diagram .pele-lifeline[data-id="P1"]'));
  const arrow = await box(page.locator('#sequence-diagram .pele-message[data-trellis-step-id="callback>api"]:not([data-trellis-return-step-id]) path').first());
  assert.ok(Math.abs(arrow.left - app.x) <= 8 && Math.abs(arrow.right - trellis.x) <= 13, JSON.stringify({arrow, app, trellis}));
  const back = await box(page.locator('#sequence-diagram .pele-message[data-trellis-return-step-id="callback>api"] path').first());
  assert.ok(Math.abs(back.right - trellis.x) <= 8 && Math.abs(back.left - app.x) <= 13, JSON.stringify({back, app, trellis}));
  const svg = await box(page.locator("#sequence-diagram svg")), label = await box(page.locator('#sequence-diagram .pele-edge-label[data-trellis-step-id="callback>api"]'));
  assert.ok(label.left >= svg.left - 1 && label.right <= svg.right + 1, JSON.stringify({label, svg}));
});

test("a syntax-only view (calls-first) draws the hop inside the callback too", async () => {
  const view = callbackView({tier:"syntax"});
  assert.equal((await render(view, {})).status, "rendered");
  // Calls-first numbers every row: `browseRequest` and the callback's own come first.
  assert.deepEqual(await page.locator("#sequence-diagram .pele-edge-label").allTextContents(), [HOP.replace("[001]", "[003]")]);
  assert.equal(await page.evaluate(view => TrellisSequence.interfaceSummary(view), view), "1 HTTP interface hop (structural matches, not proof)");
});

test("negative control: a callback the daemon did not show stays the boundary it was", async () => {
  const view = callbackView({shown:false});
  for (const [name, options] of PROJECTIONS) {
    assert.equal((await render(view, options)).status, "rendered", name);
    assert.deepEqual(await headers("#sequence-diagram .pele-actor:not(.pele-actor-footer)"), ["«module» app"], name);
    assert.equal(await page.locator("#sequence-diagram .pele-edge-label").filter({hasText:"structural match"}).count(), 0, name);
    assert.equal(await page.locator("#sequence-diagram .pele-note").filter({hasText:"callback passed to"}).count(), 0, name);
    assert.equal(await page.locator('#sequence-diagram [data-trellis-target-id^="interface:"]').count(), 0, name);
    assert.equal(await page.locator('#sequence-diagram [data-trellis-call-id="callid:api"]').count(), 0, name);
  }
  // Calls-first shows the boundary row exactly as before #888.
  assert.equal((await render(view, {})).status, "rendered");
  assert.equal(await page.locator('#sequence-diagram .pele-note[data-trellis-step-id="callback"]').count(), 1);
});
