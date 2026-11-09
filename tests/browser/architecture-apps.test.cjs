"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const {mkdirSync, writeFileSync, rmSync} = require("node:fs");
const {dirname, join} = require("node:path");
const {chromium} = require("playwright");
const H = require("./sequence-harness.cjs");

test.before(async () => H.setup(), {timeout:960000});
test.after(async () => H.cleanup());

const hostileName = '<img src=x onerror="window.__pwned=1">';
const files = new Map([
  ["a/package.json", JSON.stringify({name:hostileName}) + "\n"], ["b/package.json", '{"name":"beta"}\n'],
  ["a/main.js", "export function run() { return 1; }\n"], ["b/lib.js", "export function lib() { return 2; }\n"],
]);

async function published(callback) {
  const paths = H.fixture("", "placeholder.txt");
  rmSync(paths.source);
  for (const [path, text] of files) {
    const target = join(paths.workspace, path);
    mkdirSync(dirname(target), {recursive:true}); writeFileSync(target, text);
  }
  mkdirSync(join(paths.workspace, ".trellis"));
  writeFileSync(join(paths.workspace, ".trellis/rules.toml"), 'version = 1\n[layers.core]\norder = 1\napp = "beta"\npaths = ["b/lib.js"]\n');
  let server, browser, context;
  try {
    const indexed = await H.run(H.binary, ["index", "--workspace", paths.workspace], {env:paths.env, timeout:120000});
    server = await H.startDaemon(paths, indexed.json().status.revision.indexRevision);
    browser = await chromium.launch();
    context = await browser.newContext({viewport:{width:1440, height:1000}});
    const page = await context.newPage(), errors = [];
    page.on("pageerror", error => errors.push(error.message));
    await H.connect(page, server, paths.token);
    await page.locator("#view-architecture").click();
    await callback({page});
    assert.deepEqual(errors, [], "repository text obeys the unchanged CSP");
  } finally {
    await context?.close(); await browser?.close(); await H.stopDaemon(server);
    rmSync(paths.temp, {recursive:true, force:true, maxRetries:10, retryDelay:100});
  }
}

test("markup_in_app_name_is_text", {timeout:240000}, async () => published(async ({page}) => {
  const button = page.locator("#architecture-apps button").nth(1);
  await button.waitFor({state:"visible"});
  assert.equal(await button.textContent(), `${hostileName} · javascript`, "the package name is literal text");
  await button.click();
  await page.locator("#architecture-state[data-state=empty]").waitFor({state:"visible"});
  assert.ok((await page.locator("#architecture-state").textContent()).includes(hostileName));
  assert.ok((await page.locator("#architecture-source-line").textContent()).includes(hostileName));
  assert.equal(await page.locator("#architecture-panel img").count(), 0, "no element is created from the name");
  assert.equal(await page.evaluate(() => window.__pwned), undefined);
}));
