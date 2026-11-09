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
const hostileFiles = new Map([
  ["a/package.json", JSON.stringify({name:hostileName}) + "\n"], ["b/package.json", '{"name":"beta"}\n'],
  ["a/main.js", "export function run() { return 1; }\n"], ["b/lib.js", "export function lib() { return 2; }\n"],
]);
const hostileRules = 'version = 1\n[layers.core]\norder = 1\napp = "beta"\npaths = ["b/lib.js"]\n';
// A Rust server and a JavaScript client of 120 characters' name: `/health` is matched, `/unused` is
// server-only and `/missing` is client-only.
const longName = "a".repeat(120);
const httpFiles = new Map([
  ["Cargo.toml", '[package]\nname = "srv"\n'],
  ["src/main.rs", 'async fn health() {}\nasync fn unused() {}\nfn router() -> Router {\n    Router::new().route("/health", get(health)).route("/unused", get(unused))\n}\nfn main() {}\n'],
  ["web/package.json", JSON.stringify({name:longName}) + "\n"],
  ["web/app.js", 'async function load(api) {\n  await api("/health");\n  await api("/missing");\n}\n'],
]);
const httpRules = `version=1\n[layers.server]\napp='srv'\norder=1\npaths=['src/**']\n[layers.client]\napp='${longName}'\norder=1\npaths=['web/**']\n`;

async function published(files, rules, callback) {
  const paths = H.fixture("", "placeholder.txt");
  rmSync(paths.source);
  for (const [path, text] of files) {
    const target = join(paths.workspace, path);
    mkdirSync(dirname(target), {recursive:true}); writeFileSync(target, text);
  }
  mkdirSync(join(paths.workspace, ".trellis"));
  writeFileSync(join(paths.workspace, ".trellis/rules.toml"), rules);
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

test("markup_in_app_name_is_text", {timeout:240000}, async () => published(hostileFiles, hostileRules, async ({page}) => {
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

const drawn = async page => {
  await page.locator("#architecture-state[data-state=ready]").waitFor({state:"attached"});
  await page.locator(".architecture-apps-grid .architecture-interface-card").waitFor({state:"attached"});
};

test("all_apps_layout: columns side by side with a gutter card and a hidden overlay line, stacked when narrow", {timeout:240000}, async () => published(httpFiles, httpRules, async ({page}) => {
  await drawn(page);
  const grid = page.locator("#architecture-diagram .architecture-apps-grid");
  assert.equal(await grid.getAttribute("data-layout"), "columns");
  const heads = page.locator(".architecture-app-header");
  assert.equal(await heads.count(), 2);
  assert.match(await heads.nth(0).textContent(), /srv|aaaa/);
  const card = page.locator(".architecture-interface-card");
  assert.equal(await card.count(), 1);
  const lines = await card.locator(".architecture-card-line").allTextContents();
  assert.equal(lines[0], "HTTP · 1 route"); assert.match(lines[1], /^\d+ call sites?$/);
  assert.ok(lines.includes("◆ 1 server-only")); assert.ok(lines.includes("◆ 1 client-only"));
  const boxes = await page.evaluate(() => {
    const rect = node => { const r = node.getBoundingClientRect(); return {left:r.left, right:r.right}; };
    const [first, second] = [...document.querySelectorAll(".architecture-app-column")].map(rect);
    return {first, second, card:rect(document.querySelector(".architecture-interface-card"))};
  });
  assert.ok(boxes.card.left >= boxes.first.right - 1 && boxes.card.right <= boxes.second.left + 1, "the card sits in the gutter between the columns");
  assert.ok(Math.round(boxes.second.left - boxes.first.right) === 176, "176px gutter");
  const overlay = page.locator(".architecture-apps-overlay");
  assert.equal(await overlay.getAttribute("aria-hidden"), "true");
  assert.ok(await overlay.locator(".architecture-apps-line").count() >= 1, "the matched ends run as lines");
  assert.equal(await page.locator("#architecture-key-interface").isVisible(), true);
  assert.match(await page.locator("#architecture-footnote").textContent(), /^Apps are detected from build manifests/);
  await page.setViewportSize({width:760, height:1000});
  await page.waitForFunction(() => document.querySelector(".architecture-apps-grid")?.dataset.layout === "stacked");
  await page.locator(".architecture-interface-card .architecture-card-ends").first().waitFor({state:"attached"});
  assert.equal(await page.locator(".architecture-apps-line").count(), 0, "no line is drawn when the columns stack");
  assert.match(await page.locator(".architecture-interface-card").textContent(), /→/, "the card names both ends in words");
}));

test("all_apps_selected_word: a selected header or card says Selected, and is pressed", {timeout:240000}, async () => published(httpFiles, httpRules, async ({page}) => {
  await drawn(page);
  const card = page.locator(".architecture-interface-card"), tag = card.locator(".architecture-selected-tag");
  assert.equal(await card.getAttribute("aria-pressed"), "false");
  assert.equal(await tag.isVisible(), false, "no word before a selection");
  await card.focus(); await page.keyboard.press("Enter");
  assert.equal(await card.getAttribute("aria-pressed"), "true");
  assert.equal(await tag.isVisible(), true); assert.equal(await tag.textContent(), "Selected");
  assert.match(await card.getAttribute("aria-label"), /^HTTP interface from .* layer client to srv layer server: 1 matched route, 1 client call site, 1 server-only route, 1 client-only call.*\. Selected$/);
  assert.match(await page.locator("#architecture-inspector-title").textContent(), /→ srv$/);
  assert.equal(await page.locator(".architecture-apps-line[data-selected=true]").count() > 0, true, "the lines turn to the link tone");
  const head = page.locator(".architecture-app-header").first();
  await head.click();
  assert.equal(await head.getAttribute("aria-pressed"), "true"); assert.equal(await card.getAttribute("aria-pressed"), "false", "one selection at a time");
  assert.equal(await head.locator(".architecture-selected-tag").isVisible(), true);
  assert.equal(await card.locator(".architecture-selected-tag").isVisible(), false);
  assert.doesNotMatch(await card.getAttribute("aria-label"), /Selected/);
}));

test("all_apps_long_names_wrap: a 120-character name is not clipped and keeps its full accessible name", {timeout:240000}, async () => published(httpFiles, httpRules, async ({page}) => {
  await drawn(page);
  const head = page.locator(".architecture-app-header", {hasText:longName});
  assert.equal((await head.locator(".architecture-app-name").textContent()), longName, "the whole name is on the page");
  const measure = await page.evaluate(() => {
    const out = {};
    for (const [key, node] of [["head", document.querySelector(".architecture-app-header:has(.architecture-app-name)")], ["card", document.querySelector(".architecture-interface-card")]]) {
      const style = getComputedStyle(node);
      out[key] = {overflow:node.scrollWidth <= node.clientWidth + 1, whiteSpace:style.whiteSpace, wrap:style.overflowWrap, ellipsis:style.textOverflow};
    }
    const name = [...document.querySelectorAll(".architecture-app-name")].find(node => node.textContent.length === 120);
    out.lines = Math.round(name.getBoundingClientRect().height / parseFloat(getComputedStyle(name).lineHeight));
    return out;
  });
  for (const key of ["head", "card"]) {
    assert.equal(measure[key].overflow, true, `${key} does not overflow its box`);
    assert.equal(measure[key].whiteSpace, "normal"); assert.equal(measure[key].wrap, "anywhere"); assert.notEqual(measure[key].ellipsis, "ellipsis");
  }
  assert.ok(measure.lines >= 2, "the 120 characters wrap onto more than one line");
  assert.ok((await page.locator(".architecture-interface-card").getAttribute("aria-label")).includes(longName), "the card's name carries the whole app name");
  assert.ok((await page.locator(`#architecture-apps button`).nth(1).textContent()).includes(longName) || (await page.locator(`#architecture-apps button`).nth(2).textContent()).includes(longName));
}));
