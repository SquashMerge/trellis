"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const {readFileSync} = require("node:fs");
const {createServer} = require("node:http");
const {resolve, join} = require("node:path");
const {chromium} = require("playwright");

const ROOT = resolve(__dirname, "../..");
const vendorPath = "/vendor/pele-1.10.0-trellis.2.js";
const vendor = readFileSync(join(ROOT, "web/vendor/pele-1.10.0-trellis.2.js"));
const html = `<!doctype html><html><head><script src="${vendorPath}" defer></script></head><body><div id="diagram"></div></body></html>`;
let server, base, browser;

// This isolates upstream SVG layout and native browser hit testing. Raw Pele
// owns inline SVG styles; this host has no CSP and makes no product CSP claim.
// Trellis's adapter and its strict-CSP acceptance remain separate tests.
test.before(async () => {
  server = createServer((req, res) => {
    if (req.url === vendorPath) {
      res.setHeader("Content-Type", "text/javascript; charset=utf-8");
      res.end(vendor);
    } else if (req.url === "/") {
      res.setHeader("Content-Type", "text/html; charset=utf-8");
      res.end(html);
    } else {
      res.writeHead(404);
      res.end();
    }
  });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  base = `http://127.0.0.1:${server.address().port}`;
  browser = await chromium.launch();
});
test.after(async () => {
  await browser?.close();
  if (server) await new Promise(resolve => server.close(resolve));
});

const cases = [
  {name: "narrow", nodes: [["A", "a"], ["B", "b"]], edges: [
    ["144 production conforming", "1-a-b"], ["144 production deviation", "1-b-a"],
  ]},
  {name: "composite", nodes: [["a", "a"], ["g", "g"]], edges: [
    ["12 production conforming", "1-upper-left"], ["12 production conforming", "1-left-lower"],
    ["12 production deviation", "1-lower-left"], ["7 test exception", "1-right-upper"],
    ["7 production policy", "1-right-lower"],
  ]},
  {name: "vertical", nodes: [["Upper layer", "a"], ["Lower layer", "b"]], edges: [
    ["12 production conforming", "1-a-b"], ["12 production deviation", "1-b-a"],
  ]},
  {name: "horizontal", nodes: [["Left layer", "a"], ["Right layer", "b"]], edges: [
    ["12 productionconforming", "1-a-b"], ["12 productiondeviation", "1-b-a"],
  ]},
  {name: "repeated", nodes: [["A", "a"], ["B", "b"]], edges: [
    ["7 production policy", "1-a-b"], ["7 test policy", "2-a-b"],
    ["7 production exception", "3-a-b"], ["7 production deviation", "1-b-a"],
  ]},
];

for (const {name, nodes, edges} of cases) {
  test(`exact vendored Pele keeps ${name} block labels separate and natively clickable`, async () => {
    const source = readFileSync(join(ROOT, `tests/fixtures/pele-1.10.0-trellis.2/block-labels-${name}.mmd`), "utf8");
    const page = await browser.newPage();
    try {
      await page.goto(base);
      await page.waitForFunction(() => typeof window.pele?.render === "function");
      const identities = await page.evaluate(source => {
        const result = pele.render(source, {responsive: false});
        document.querySelector("#diagram").innerHTML = result.svg;
        window.nativeClicks = [];
        // Observe the browser's real target; do not route, modify, or redispatch it.
        document.addEventListener("click", event => {
          const native = event.target.closest("[data-id]");
          window.nativeClicks.push({id: native?.getAttribute("data-id"), trusted: event.isTrusted});
        }, {passive: true});
        return [...document.querySelectorAll(".pele-edge-label")].map(label => label.getAttribute("data-id"));
      }, source);
      assert.deepEqual(identities, edges.map(([, id]) => id));
      const boxes = await page.locator(".pele-edge-label").evaluateAll(labels => labels.map(label => {
        const box = label.getBoundingClientRect();
        return {x: box.x, y: box.y, width: box.width, height: box.height};
      }));
      for (let i = 0; i < boxes.length; i++) {
        assert.ok(boxes[i].width > 0 && boxes[i].height > 0, `${name}: visible label ${i}`);
        for (let j = i + 1; j < boxes.length; j++) {
          const a = boxes[i], b = boxes[j];
          const overlaps = a.x < b.x + b.width && b.x < a.x + a.width &&
            a.y < b.y + b.height && b.y < a.y + a.height;
          assert.equal(overlaps, false, `${name}: labels ${i} and ${j} overlap: ${JSON.stringify([a, b])}`);
        }
      }
      const expected = [];
      for (const [label, id] of [...nodes, ...edges]) {
        const element = page.locator(`.pele-node[data-id="${id}"], .pele-cluster[data-id="${id}"], .pele-edge-label[data-id="${id}"]`);
        assert.equal(await element.textContent(), label);
        await element.click();
        expected.push({id, trusted: true});
        assert.deepEqual(await page.evaluate(() => window.nativeClicks), expected, `${name}: native click on ${label}`);
      }
    } finally {
      await page.close();
    }
  });
}
