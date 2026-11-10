"use strict";
// #852: the app switcher of Treemap, Classes and the Files tree, and the one choice they share with
// Architecture, in a real browser on a real index. Every test runs under classic 15px scrollbars, as CI draws them.
const test = require("node:test");
const assert = require("node:assert/strict");
const {mkdirSync, writeFileSync, rmSync} = require("node:fs");
const {dirname, join} = require("node:path");
const {chromium} = require("playwright");
const H = require("./sequence-harness.cjs");
const {classicScrollbars} = require("./architecture-geometry.cjs");

test.before(async () => H.setup(), {timeout:960000});
test.after(async () => H.cleanup());

// A Rust server `srv` authored in server/ and a JavaScript client `web` detected in web/. The browser test of the web app
// is web's through its hidden layer, though no web root holds it; web/vendor/lib.js has no layer and
// is web's by its root; tools/build.py is in no app.
const files = new Map([
  ["server/src/main.rs", "pub struct Server { port: u32 }\nfn main() { let _ = Server { port: 1 }; }\n"],
  ["web/package.json", '{"name":"web"}\n'],
  ["web/app.js", "export class Widget { draw() { return 1; } }\n"],
  ["web/vendor/lib.js", "export function vendored() { return 2; }\n"],
  ["tests/browser/web.test.cjs", "function checksWidget() { return 3; }\n"],
  ["tools/build.py", "def build():\n    return 4\n"],
]);
const rules = `version=1
[apps.srv]
roots=['server']
[layers.server]
app='srv'
order=1
paths=['server/**']
[layers.client]
app='web'
order=1
paths=['web/app.js']
[layers.web-tests]
app='web'
order=0
display='hidden'
paths=['tests/browser/**']
`;
const oneApp = new Map([["Cargo.toml", '[package]\nname = "srv"\n'], ["src/main.rs", "pub struct Server { port: u32 }\nfn main() {}\n"], ["tools/build.py", "def build():\n    return 4\n"]]);
const oneAppRules = "version=1\n[layers.server]\norder=1\npaths=['src/**']\n";

async function published(fixture, rulesText, callback, viewport = {width:1600, height:1000}) {
  const paths = H.fixture("", "placeholder.txt");
  rmSync(paths.source);
  for (const [path, text] of fixture) {
    const target = join(paths.workspace, path);
    mkdirSync(dirname(target), {recursive:true}); writeFileSync(target, text);
  }
  mkdirSync(join(paths.workspace, ".trellis"));
  writeFileSync(join(paths.workspace, ".trellis/rules.toml"), rulesText);
  let server, browser, context;
  try {
    const indexed = await H.run(H.binary, ["index", "--workspace", paths.workspace], {env:paths.env, timeout:120000});
    server = await H.startDaemon(paths, indexed.json().status.revision.indexRevision);
    browser = await chromium.launch();
    context = await browser.newContext({viewport});
    const page = await context.newPage(), errors = [];
    page.on("pageerror", error => errors.push(error.message));
    await H.connect(page, server, paths.token);
    await page.locator("#view-architecture").click();
    await page.locator("#architecture-state[data-state=ready], #architecture-state[data-state=empty]").waitFor({state:"attached"});
    await classicScrollbars(page);
    // The explorer and Treemap scroll too: classic scrollbars there as well.
    await page.evaluate(() => {
      const sheet = [...document.styleSheets].find(item => item.href?.endsWith("/architecture.css"));
      for (const selector of [".file-browser", "#treemap-diagram", "#classes-diagram", ".view-panel"]) sheet.insertRule(`${selector}::-webkit-scrollbar { width: 15px; height: 15px; }`, sheet.cssRules.length);
    });
    await callback({page});
    assert.deepEqual(errors, [], "no page errors");
  } finally {
    await context?.close(); await browser?.close(); await H.stopDaemon(server);
    rmSync(paths.temp, {recursive:true, force:true, maxRetries:10, retryDelay:100});
  }
}
const nextFrame = page => page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
async function treemapReady(page) {
  await nextFrame(page);
  await page.waitForFunction(() => document.querySelector("#treemap-diagram")?.dataset.layout === "settled" &&
    ["ready", "empty"].includes(document.querySelector("#treemap-state")?.dataset.state));
}
const choices = (page, id) => page.locator(`#${id} button`).allTextContents();
const pressed = (page, id) => page.locator(`#${id} button[aria-pressed=true]`).allTextContents();
const treemapFiles = page => page.locator("#treemap-files [data-trellis-file-id]").evaluateAll(nodes => nodes.map(node => JSON.parse(node.getAttribute("data-trellis-file-id"))[2]).sort());
const treeEntries = page => page.locator("#file-tree .file-pick").evaluateAll(nodes => nodes.map(node => node.title));
async function pick(page, id, label) {
  await page.locator(`#${id} button`, {hasText:label}).first().click();
  await page.waitForFunction(([target, text]) => [...document.querySelectorAll(`#${target} button[aria-pressed=true]`)].some(node => node.textContent.startsWith(text)), [id, label]);
}

test("switcher_routing: one choice filters Treemap, Classes and the Files tree, and Architecture draws it", {timeout:240000}, async () => published(files, rules, async ({page}) => {
  // The Files tree switcher appears once the summary is read, beside Architecture's own.
  await page.locator("#files-apps button").first().waitFor();
  assert.deepEqual(await choices(page, "files-apps"), ["All apps", "srv · rust", "web · javascript"]);
  assert.deepEqual(await choices(page, "architecture-apps"), await choices(page, "files-apps"), "the same choices as Architecture");
  const allEntries = await treeEntries(page);
  for (const entry of ["server/", "tests/", "tools/", "web/"]) assert.ok(allEntries.includes(entry), `All apps lists ${entry}: ${allEntries}`);

  await page.locator("#view-treemap").click(); await treemapReady(page);
  assert.deepEqual(await pressed(page, "treemap-apps"), ["All apps"]);
  const everything = await treemapFiles(page);
  assert.ok(everything.includes("tools/build.py") && everything.includes("server/src/main.rs"), `every file under All apps: ${everything}`);
  await pick(page, "treemap-apps", "web"); await treemapReady(page);
  assert.deepEqual(await treemapFiles(page), ["web/app.js", "web/vendor/lib.js"], "web's files; Hide test files keeps its browser test out");
  await page.locator("#treemap-hide-tests").evaluate(node => { node.checked = false; node.dispatchEvent(new Event("change")); }); await treemapReady(page);
  assert.deepEqual(await treemapFiles(page), ["tests/browser/web.test.cjs", "web/app.js", "web/vendor/lib.js"], "the layer's file and the unassigned file are web's");
  assert.match(await page.locator("#treemap-summary").textContent(), /3 of \d+ indexed files in this app/);

  // The Files tree follows: web's folders only, and the rest counted.
  assert.deepEqual(await pressed(page, "files-apps"), ["web · javascript"]);
  assert.deepEqual(await treeEntries(page), ["tests/", "web/"]);
  assert.match(await page.locator("#files-state").textContent(), /outside web · All apps shows them/);

  // Architecture draws web's stack.
  await page.locator("#view-architecture").click();
  await page.waitForFunction(() => [...document.querySelectorAll("#architecture-apps button[aria-pressed=true]")].some(node => node.textContent.startsWith("web")));
  await page.locator("#architecture-source-line", {hasText:"web · roots web"}).waitFor();

  // Classes lists web's class and passes over srv's struct.
  await page.locator("#view-classes").click();
  assert.deepEqual(await pressed(page, "classes-apps"), ["web · javascript"]);
  await page.locator("#classes-search").click();
  await page.waitForFunction(() => document.querySelector("#classes-state")?.dataset.state === "ready");
  assert.deepEqual((await page.locator(".classes-result > span:first-child").allTextContents()).map(name => name.split(".").at(-1)), ["Widget"]);
  // All apps from Classes' switcher lists both, and the others follow.
  await pick(page, "classes-apps", "All apps");
  await page.waitForFunction(() => document.querySelectorAll(".classes-result").length === 2);
  assert.deepEqual(await pressed(page, "files-apps"), ["All apps"]);
  assert.deepEqual((await treeEntries(page)).filter(entry => entry.endsWith("/")), allEntries.filter(entry => entry.endsWith("/")));
}));

test("switcher_geometry: at 1200 and 760px every switcher button lies inside its panel and answers a pointer", {timeout:240000}, async () => published(files, rules, async ({page}) => {
  await page.locator("#files-apps button").first().waitFor();
  const inside = (id, panel) => page.evaluate(([target, owner]) => {
    const box = document.querySelector(owner).getBoundingClientRect();
    return {panel:{left:box.left, right:box.right}, page:{scrollWidth:document.documentElement.scrollWidth, clientWidth:document.documentElement.clientWidth},
      buttons:[...document.querySelectorAll(`#${target} button`)].map(node => { const r = node.getBoundingClientRect(); return {text:node.textContent, left:r.left, right:r.right, top:r.top, bottom:r.bottom, width:r.width}; })};
  }, [id, panel]);
  for (const width of [1200, 760]) {
    await page.setViewportSize({width, height:1000});
    for (const [view, id, panel] of [["treemap", "treemap-apps", "#treemap-panel"], ["classes", "classes-apps", "#classes-panel"]]) {
      await page.locator(`#view-${view}`).click();
      if (view === "treemap") await treemapReady(page);
      const measured = await inside(id, panel);
      assert.equal(measured.buttons.length, 3, `${id} at ${width}`);
      assert.ok(measured.page.scrollWidth <= measured.page.clientWidth + 1, `no sideways page scroll at ${width}: ${JSON.stringify(measured.page)}`);
      for (const button of measured.buttons) {
        assert.ok(button.width > 0 && button.left >= measured.panel.left - 0.5 && button.right <= measured.panel.right + 0.5, `${id} ${button.text} inside at ${width}: ${JSON.stringify(button)} ${JSON.stringify(measured.panel)}`);
      }
      for (let at = measured.buttons.length - 1; at >= 0; at -= 1) {
        const now = (await inside(id, panel)).buttons[at];
        await page.mouse.click((now.left + now.right) / 2, (now.top + now.bottom) / 2);
        await page.waitForFunction(([target, index]) => document.querySelectorAll(`#${target} button`)[index]?.getAttribute("aria-pressed") === "true", [id, at]);
        if (view === "treemap") await treemapReady(page);
      }
    }
  }
  // The explorer is a side rail at 1200px.
  await page.setViewportSize({width:1200, height:1000});
  const rail = await inside("files-apps", "#explorer-panel");
  for (const button of rail.buttons) assert.ok(button.left >= rail.panel.left - 0.5 && button.right <= rail.panel.right + 0.5, `files-apps ${button.text}: ${JSON.stringify(button)} ${JSON.stringify(rail.panel)}`);
}));

// Negative control: one app has no switcher anywhere, and every view lists every file.
test("negative control: a checkout with one app draws no switcher and filters nothing", {timeout:240000}, async () => published(oneApp, oneAppRules, async ({page}) => {
  await page.locator("#architecture-state[data-state=ready]").waitFor({state:"attached"});
  await page.locator("#view-treemap").click(); await treemapReady(page);
  for (const id of ["files-apps", "treemap-apps", "classes-apps", "architecture-apps"]) assert.equal(await page.locator(`#${id}`).isHidden(), true, `${id} is hidden`);
  assert.deepEqual(await treemapFiles(page), ["src/main.rs", "tools/build.py"]);
  const entries = await treeEntries(page);
  assert.ok(entries.includes("src/") && entries.includes("tools/"), `${entries}`);
}));
