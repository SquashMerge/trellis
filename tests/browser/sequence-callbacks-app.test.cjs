"use strict";
// #888 end to end: a published two-app checkout (a Rust axum daemon, `alpha`, and a JavaScript web
// app, `web-app`), the real daemon, its owner-built interface evidence and the real web shell. The
// web app's `load` makes its client call inside the callback it passes to `browseRequest`, as
// Trellis's own `loadSequence` does: the callback is shown under that call, with its hop. `quiet`
// passes a callback with no hop: it stays the boundary it always was.
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
  ["src/main.rs", 'async fn sequence() {}\nfn router() -> Router {\n    Router::new()\n        .route("/api/sequence", post(sequence))\n}\nfn main() {}\n'],
  ["web/package.json", '{"name":"web-app"}\n'],
  ["web/app.js", 'function api(path, method) { return fetch(path, {method}); }\nasync function browseRequest(action) { await action(); }\nasync function load() {\n  await browseRequest(async () => {\n    await api("/api/sequence", "POST");\n  });\n}\nasync function quiet() {\n  await browseRequest(async () => {\n    local();\n  });\n}\nfunction local() {}\n'],
]);
const BOUNDARY = "Nested function/callback/class boundary: bodies are not executed here; class definition effects (base, computed keys, static initialization) are not expanded.";
const delay = ms => new Promise(done => setTimeout(done, ms));
const every = (steps, test) => steps.every(function all(step) { return test(step) && step.children.every(all) && step.alternate.every(all); });
const find = (steps, test) => { for (const step of steps) { if (test(step)) return step; const found = find([...step.children, ...step.alternate], test); if (found) return found; } return null; };

test("a hop inside a callback is shown under the call it is passed to; a callback without one is not", {timeout:300000}, async () => {
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
    const sequence = async (name, showAll = false) => {
      const seed = await H.seedId(server, paths.token, name);
      const dto = await H.api(server.base, paths.token, "POST", H.scoped(server.rootKey, "sequence"), {seed, expectedRevision:pin, showAll, interactions:!showAll});
      assert.equal(dto.status, 200, JSON.stringify(dto.data));
      return {seed, data:dto.data};
    };
    // The owner builds the head's interface evidence in the background; the bound only detects a hang.
    let load;
    for (const deadline = Date.now() + 120000; ;) {
      load = await sequence("load");
      if (load.data.interfaces?.state === "ready" || Date.now() > deadline) break;
      assert.equal(load.data.interfaces?.state, "notMeasured", JSON.stringify(load.data.interfaces));
      await delay(100);
    }
    assert.deepEqual(load.data.interfaces, {state:"ready", hops:1, unresolved:0});
    const outer = load.data.steps.find(step => step.kind === "call" && step.label === "browseRequest");
    const block = outer.children.find(step => step.kind === "callback");
    assert.equal(block.label, "callback passed to browseRequest · when and how often it runs is unknown");
    const hop = find(block.children, step => step.interface);
    assert.deepEqual([hop.interface.method, hop.interface.pathTemplate, hop.interface.serverApp], ["POST", "/api/sequence", "alpha"]);
    assert.ok(every(load.data.steps, step => step.label !== BOUNDARY), "the boundary became the block");
    // Show all draws the same block.
    const all = await sequence("load", true);
    assert.ok(find(all.data.steps, step => step.kind === "callback" && find(step.children, item => item.interface)));
    // Negative control: no hop in the callback, so its boundary stays and nothing is expanded.
    const quiet = await sequence("quiet");
    assert.deepEqual(quiet.data.interfaces, {state:"ready", hops:0, unresolved:0});
    assert.ok(every(quiet.data.steps, step => step.kind !== "callback"));
    assert.equal(quiet.data.steps.filter(step => step.label === BOUNDARY).length, 1);

    browser = await chromium.launch();
    context = await browser.newContext({viewport:{width:1440, height:900}});
    const page = await context.newPage(), errors = [];
    page.on("pageerror", error => errors.push(error.message));
    await H.connect(page, server, paths.token);
    await page.evaluate(id => { location.hash = `#sequence?sid=${encodeURIComponent(id)}`; }, load.seed);
    await page.waitForFunction(() => /Pele rendered|fell back/.test(document.querySelector("#sequence-state")?.textContent || ""));
    const state = await page.locator("#sequence-state").innerText();
    assert.match(state, /Pele rendered/);
    assert.match(state, /1 HTTP interface hop \(structural matches, not proof\)/);
    const drawn = await page.locator("#sequence-diagram .pele-edge-label").allTextContents();
    assert.deepEqual(drawn.map(text => text.replace(/^\[\d{3}\] /, "")), ["POST /api/sequence · exact · structural match, not proof"]);
    const headers = await page.locator("#sequence-diagram .pele-actor:not(.pele-actor-footer)").evaluateAll(nodes => nodes.map(node => node.textContent));
    assert.ok(headers.some(text => text.includes("«app»") && text.includes("alpha")), JSON.stringify(headers));
    const notes = await page.locator("#sequence-diagram .pele-note").allTextContents();
    assert.ok(notes.some(text => text.includes("callback passed to browseRequest")), JSON.stringify(notes));
    // Selecting the hop inspects it as #853 does.
    await page.locator(`#sequence-diagram .pele-edge-label[data-trellis-call-id="${hop.callId}"]`).click();
    assert.match(await page.locator("#call-inspector").innerText(), /HTTP interface hop · POST \/api\/sequence · exact · web-app → alpha/);
    // The negative control in the web shell: no callback frame, no hop.
    await page.evaluate(id => { location.hash = `#sequence?sid=${encodeURIComponent(id)}`; }, quiet.seed);
    await page.waitForFunction(() => /^quiet/.test(document.querySelector("#sequence-state")?.textContent || "") && /Pele rendered|fell back/.test(document.querySelector("#sequence-state")?.textContent || ""));
    assert.equal(await page.locator("#sequence-diagram .pele-edge-label").filter({hasText:"structural match"}).count(), 0);
    assert.equal(await page.locator("#sequence-diagram .pele-note").filter({hasText:"callback passed to"}).count(), 0);
    assert.deepEqual(errors, []);
  } finally {
    await context?.close(); await browser?.close(); await H.stopDaemon(server);
    rmSync(paths.temp, {recursive:true, force:true, maxRetries:10, retryDelay:100});
  }
});
