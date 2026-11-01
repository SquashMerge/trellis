"use strict";
// Published class DTO through the production daemon and unmodified shell:
// Pele class nodes and members map back to the exact response records.
const test = require("node:test");
const assert = require("node:assert/strict");
const {rmSync} = require("node:fs");
const {chromium} = require("playwright");
const H = require("./sequence-harness.cjs");

const source = `class Watcher {
  private int count = 1;
  int measured(int value) {
    if (value > 0) return helper(value);
    return count;
  }
  int helper(int value) { return value + count; }
}
`;

test.before(async () => H.setup(), {timeout:960000});
test.after(async () => H.cleanup());

test("published Java class renders through Pele with exact class, member and method identities", {timeout:240000}, async () => {
  const paths = H.fixture(source, "Watcher.java");
  let server, browser;
  try {
    const indexed = await H.run(H.binary, ["index", "--workspace", paths.workspace], {env:paths.env, timeout:120000});
    const pin = indexed.json().status.revision;
    server = await H.startDaemon(paths, pin.indexRevision);
    const read = async (method, suffix, body) => {
      const response = await H.api(server.base, paths.token, method, H.scoped(server.rootKey, suffix), body);
      assert.equal(response.status, 200, JSON.stringify(response.data));
      return response.data;
    };
    const catalog = await read("GET", `classes?${new URLSearchParams({q:"Watcher", offset:"0", limit:"100", ...pin})}`);
    const seed = catalog.items.find(item => item.symbol.name === "Watcher").symbol.id;
    const dto = await read("POST", "class-diagram", {seed, expectedRevision:pin, expanded:[], includeHierarchy:false, includeUnmatched:false});
    const declaration = dto.nodes.find(node => node.id === seed).class;
    assert.ok(declaration.methods.some(method => method.name === "measured" && method.symbolId), "real Java producer emits a method symbol");

    browser = await chromium.launch();
    const page = await (await browser.newContext({viewport:{width:1440, height:900}})).newPage();
    const errors = [], requests = [];
    page.on("pageerror", error => errors.push(error.message));
    page.on("console", message => { if (message.type() === "error" && /Content Security Policy|CSP/i.test(message.text())) errors.push(message.text()); });
    page.on("request", request => { const url = new URL(request.url()); if (url.pathname.startsWith("/api/")) requests.push({url, body:request.method() === "POST" ? request.postDataJSON() : null}); });
    await H.connect(page, server, paths.token);

    await page.locator("#view-classes").click();
    await page.locator("#classes-query").fill("Watcher");
    await page.locator("#classes-search").click();
    await page.locator(".classes-result").filter({hasText:"Watcher"}).first().click();
    await page.waitForFunction(() => /Pele rendered/.test(document.querySelector("#classes-state")?.textContent || ""), null, {timeout:15000});
    const diagramRequest = requests.filter(row => row.url.pathname.endsWith("/class-diagram")).at(-1);
    assert.deepEqual(diagramRequest.body.expectedRevision, pin);

    const node = page.locator(`#classes-diagram .pele-node[data-trellis-class-id="${seed}"]`);
    assert.equal(await page.locator("#classes-diagram .pele-node").count(), 1);
    assert.equal(await node.locator(".pele-label").textContent(), declaration.symbol.name);
    assert.match(await node.getAttribute("aria-label"), new RegExp(`${declaration.symbol.path.replace(/\./g, "\\.")}`));
    const expected = [...declaration.fields.map(f => `${f.name}${f.typeHint ? ": " + f.typeHint : ""}`),
      ...declaration.methods.map(m => `${m.name}()${m.typeHint ? " : " + m.typeHint : ""}`)];
    assert.deepEqual(await node.locator(".pele-member").allTextContents(), expected);
    assert.deepEqual(await node.locator(".pele-member[data-trellis-symbol-id]").evaluateAll(lines => lines.map(line => line.dataset.trellisSymbolId)),
      declaration.methods.filter(m => m.symbolId).map(m => m.symbolId));

    // Read class source uses the declaration's range at the same pin.
    await node.locator("> rect").click({position:{x:10, y:10}});
    await page.getByRole("menuitem", {name:"Read class source"}).click();
    await page.waitForFunction(() => /Watcher\.java/.test(document.querySelector("#source-path")?.textContent || "") && document.querySelector("#source .source-line"));
    const sourceRead = requests.filter(row => row.url.pathname.endsWith("/source")).at(-1);
    assert.equal(sourceRead.url.searchParams.get("indexRevision"), String(pin.indexRevision));

    // A method member opens its exact measured sequence.
    await page.locator("#view-classes").click();
    const measured = declaration.methods.find(method => method.name === "measured");
    await page.locator(`.pele-member[data-trellis-symbol-id="${measured.symbolId}"]`).click();
    await page.waitForFunction(() => /Pele rendered/.test(document.querySelector("#sequence-state")?.textContent || ""), null, {timeout:15000});
    const sequence = requests.filter(row => row.url.pathname.endsWith("/sequence")).at(-1);
    assert.equal(sequence.body.seed, measured.symbolId);
    assert.deepEqual(sequence.body.expectedRevision, pin);
    assert.deepEqual(errors, [], "class Pele view runs under the real CSP");
  } finally {
    await browser?.close(); await H.stopDaemon(server);
    rmSync(paths.temp, {recursive:true, force:true, maxRetries:10, retryDelay:100});
  }
});
