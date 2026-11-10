"use strict";
// #853 end to end: a published two-app checkout (a Rust axum daemon, `alpha`, and a JavaScript web
// app, `web-app`), the real daemon, its owner-built interface evidence and the real web shell. The
// web app's `load` calls its `api` wrapper five times: three match daemon routes (exact, template,
// wrapper-inferred) and hop into the daemon's handlers; two stay unresolved boundaries.
const test = require("node:test");
const assert = require("node:assert/strict");
const {mkdirSync, writeFileSync, rmSync} = require("node:fs");
const {dirname, join} = require("node:path");
const {chromium} = require("playwright");
const H = require("./sequence-harness.cjs");

test.before(async () => H.setup(), {timeout:960000});
test.after(async () => H.cleanup());

const files = new Map([
  ["Cargo.toml", '[package]\nname = "alpha"\n'],
  ["src/main.rs", 'async fn sequence() {}\nasync fn item() {}\nasync fn head() {}\nfn router() -> Router {\n    Router::new()\n        .route("/api/sequence", post(sequence))\n        .route("/api/items/{id}", get(item))\n        .route("/api/checkouts/{root}/status/head", get(head))\n}\nfn main() {}\n'],
  ["web/package.json", '{"name":"web-app"}\n'],
  ["web/app.js", 'function api(path, method) { return fetch(path, {method}); }\nasync function load(id, dynamic) {\n  await api("/api/sequence", "POST");\n  await api(`/api/items/${id}`, "GET");\n  await api("/api/status/head", "GET");\n  await api(dynamic, "GET");\n  await api("/api/nowhere", "GET");\n}\n'],
]);
const LABELS = [
  "[001] POST /api/sequence · exact · structural match, not proof",
  "[002] GET /api/items/{id} · template · structural match, not proof",
  "[003] GET /api/checkouts/{root}/status/head · wrapper-inferred · structural match, not proof",
];
const delay = ms => new Promise(done => setTimeout(done, ms));

test("a web app Sequence hops into the daemon at matched routes and opens the handler on demand", {timeout:300000}, async () => {
  const paths = H.fixture("", "placeholder.txt");
  rmSync(paths.source);
  for (const [path, text] of files) {
    const target = join(paths.workspace, path);
    mkdirSync(dirname(target), {recursive:true}); writeFileSync(target, text);
  }
  let server, browser, context;
  try {
    const indexed = await H.run(H.binary, ["index", "--workspace", paths.workspace], {env:paths.env, timeout:120000});
    const pin = indexed.json().status.revision;
    server = await H.startDaemon(paths, pin.indexRevision);
    const seed = await H.seedId(server, paths.token, "load");
    // The owner builds the head's interface evidence in the background; a Sequence never waits for
    // it and says `notMeasured` until then. The bound only detects a hang.
    let dto;
    for (const deadline = Date.now() + 120000; ;) {
      dto = await H.api(server.base, paths.token, "POST", H.scoped(server.rootKey, "sequence"), {seed, expectedRevision:pin, showAll:false, interactions:true});
      assert.equal(dto.status, 200, JSON.stringify(dto.data));
      if (dto.data.interfaces?.state === "ready" || Date.now() > deadline) break;
      assert.equal(dto.data.interfaces?.state, "notMeasured", JSON.stringify(dto.data.interfaces));
      await delay(100);
    }
    assert.deepEqual(dto.data.interfaces, {state:"ready", hops:3, unresolved:2});
    const steps = dto.data.steps.filter(step => step.interface);
    assert.deepEqual(steps.map(step => [step.range.startLine, step.interface.confidence, step.interface.method, step.interface.pathTemplate, step.interface.serverApp]), [
      [3, "exact", "POST", "/api/sequence", "alpha"],
      [4, "template", "GET", "/api/items/{id}", "alpha"],
      [5, "wrapperInferred", "GET", "/api/checkouts/{root}/status/head", "alpha"],
      [6, "unresolved", "GET", null, null],
      [7, "unresolved", "GET", "/api/nowhere", null],
    ]);
    const handlers = dto.data.participants.filter(participant => participant.kind === "interfaceHandler");
    assert.deepEqual(handlers.map(participant => [participant.label, participant.column.id, participant.declaration.path]).sort(),
      [["head", "app:alpha", "src/main.rs"], ["item", "app:alpha", "src/main.rs"], ["sequence", "app:alpha", "src/main.rs"]]);
    const handler = handlers.find(participant => participant.id === steps[0].interface.participant);
    // Negative control: the daemon's own router makes no client call, so nothing crosses.
    const router = await H.api(server.base, paths.token, "POST", H.scoped(server.rootKey, "sequence"), {seed:await H.seedId(server, paths.token, "router"), expectedRevision:pin, showAll:false, interactions:true});
    assert.equal(router.status, 200, JSON.stringify(router.data));
    assert.deepEqual(router.data.interfaces, {state:"ready", hops:0, unresolved:0});
    assert.ok(router.data.steps.every(function none(step) { return !step.interface && step.children.every(none) && step.alternate.every(none); }));

    browser = await chromium.launch();
    context = await browser.newContext({viewport:{width:1440, height:900}});
    const page = await context.newPage(), errors = [];
    page.on("pageerror", error => errors.push(error.message));
    await H.connect(page, server, paths.token);
    // A Sequence link opens `load` in the web app.
    await page.evaluate(id => { location.hash = `#sequence?sid=${encodeURIComponent(id)}`; }, seed);
    await page.waitForFunction(() => /Pele rendered|fell back/.test(document.querySelector("#sequence-state")?.textContent || ""));
    const state = await page.locator("#sequence-state").innerText();
    assert.match(state, /Pele rendered/);
    assert.match(state, /3 HTTP interface hops \(structural matches, not proof\) · 2 unresolved HTTP calls/);
    // A syntax-only view is calls-first: every row is numbered, so the hops are not 1, 2 and 3.
    const drawn = await page.locator("#sequence-diagram .pele-edge-label").allTextContents();
    assert.deepEqual(drawn.map(text => text.replace(/^\[\d{3}\] /, "")), LABELS.map(text => text.replace(/^\[\d{3}\] /, "")));
    const headers = await page.locator("#sequence-diagram .pele-actor:not(.pele-actor-footer)").evaluateAll(nodes => nodes.map(node => node.textContent));
    assert.ok(headers.some(text => text.includes("«app»") && text.includes("alpha")), JSON.stringify(headers));
    // Unresolved calls stay rows of the web app: no arrow, no handler.
    for (const step of steps.slice(3)) {
      assert.equal(await page.locator(`#sequence-diagram .pele-message[data-trellis-call-id="${step.callId}"]`).count(), 0);
      assert.equal(await page.locator(`#sequence-diagram .pele-note[data-trellis-call-id="${step.callId}"]`).count(), 1);
    }
    // The hop's inspector keeps the confidence visible and continues into the handler on demand.
    await page.locator(`#sequence-diagram .pele-edge-label[data-trellis-call-id="${steps[0].callId}"]`).click();
    const inspector = await page.locator("#call-inspector").innerText();
    assert.match(inspector, /HTTP interface hop · POST \/api\/sequence · exact · web-app → alpha · structural match by method and path, not proof the request is sent/);
    assert.match(inspector, /HTTP interface handler · sequence in alpha/);
    const open = page.getByRole("button", {name:"Open handler sequence"});
    await open.click();
    await page.waitForFunction(id => new URLSearchParams(location.hash.slice("#sequence?".length)).get("sid") === id, handler.declaration.id);
    await page.waitForFunction(() => /^sequence · src\/main\.rs/.test(document.querySelector("#sequence-state")?.textContent || ""));
    assert.deepEqual(errors, []);
  } finally {
    await context?.close(); await browser?.close(); await H.stopDaemon(server);
    rmSync(paths.temp, {recursive:true, force:true, maxRetries:10, retryDelay:100});
  }
});
