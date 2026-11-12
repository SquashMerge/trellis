"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const {mkdirSync, writeFileSync, rmSync} = require("node:fs");
const {dirname, join} = require("node:path");
const {chromium} = require("playwright");
const H = require("./sequence-harness.cjs");
const {arrowGeometry, classicScrollbars} = require("./architecture-geometry.cjs");

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
    // Two columns need a canvas of 2·352 + 176 = 880px plus its 24px padding. At 1440 the canvas
    // was 912px with overlay scrollbars but 897px where Chromium draws a classic 15px scrollbar in
    // the stable gutter (Linux, and CI Macs without a trackpad), which stacks the columns. 1600
    // leaves a margin no scrollbar or font takes away; the stacked layout is asserted at 760.
    context = await browser.newContext({viewport:{width:1600, height:1000}});
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

// What decides the All apps layout, for a failure message.
const layoutInputs = page => page.evaluate(() => {
  const canvas = document.getElementById("architecture-diagram"), width = node => node ? Math.round(node.getBoundingClientRect().width) : null;
  const name = document.querySelector(".architecture-app-name");
  return {viewport:innerWidth, devicePixelRatio, canvasClientWidth:canvas.clientWidth, scrollbar:canvas.offsetWidth - canvas.clientWidth,
    layout:document.querySelector(".architecture-apps-grid")?.dataset.layout, columns:[...document.querySelectorAll(".architecture-app-column")].map(width),
    rail:width(document.querySelector(".workspace-frame > aside")), inspector:width(document.getElementById("architecture-inspector")), font:name && getComputedStyle(name).font};
});
const columnsDrawn = async page => {
  const inputs = await layoutInputs(page);
  assert.equal(inputs.layout, "columns", `the columns fit side by side: ${JSON.stringify(inputs)}`);
};
const drawn = async page => {
  await page.locator("#architecture-state[data-state=ready]").waitFor({state:"attached"});
  await page.locator(".architecture-apps-grid .architecture-interface-card").waitFor({state:"attached"});
};

test("all_apps_layout: columns side by side with a lane-sized gutter, the card under the grid and a hidden overlay line, stacked when narrow", {timeout:240000}, async () => published(httpFiles, httpRules, async ({page}) => {
  await drawn(page);
  const grid = page.locator("#architecture-diagram .architecture-apps-grid");
  await columnsDrawn(page);
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
    const rect = node => { const r = node.getBoundingClientRect(); return {left:r.left, right:r.right, top:r.top, bottom:r.bottom}; };
    const [first, second] = [...document.querySelectorAll(".architecture-app-column")].map(rect);
    return {first, second, grid:rect(document.querySelector(".architecture-apps-grid")), card:rect(document.querySelector(".architecture-interface-card")),
      under:!!document.querySelector(".architecture-apps-cards .architecture-interface-card")};
  });
  // #949: one line, so the gutter is the least 48px, narrower than a card, which is listed under the grid.
  assert.equal(Math.round(boxes.second.left - boxes.first.right), 48, `48px gutter: ${JSON.stringify(boxes)}`);
  assert.ok(boxes.under && boxes.card.top >= boxes.grid.bottom - 1, `the card sits under the grid: ${JSON.stringify(boxes)}`);
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

// #949: a Rust server with two JavaScript clients, `one` calling one route and `two` three. The server comes
// first in manifest order, so only interface order puts it between its clients.
const serverFirstFiles = new Map([
  ["Cargo.toml", '[package]\nname = "srv"\n'],
  ["src/main.rs", 'async fn a() {}\nasync fn b() {}\nasync fn c() {}\nfn router() -> Router {\n    Router::new().route("/a", get(a)).route("/b", get(b)).route("/c", get(c))\n}\nfn main() {}\n'],
  ["one/package.json", '{"name":"one"}\n'], ["one/app.js", 'async function load(api) {\n  await api("/a");\n}\n'],
  ["two/package.json", '{"name":"two"}\n'], ["two/app.js", 'async function load(api) {\n  await api("/a");\n  await api("/b");\n  await api("/c");\n}\n'],
]);
const serverFirstRules = "version=1\n[layers.server]\napp='srv'\norder=1\npaths=['src/**']\n[layers.first]\napp='one'\norder=1\npaths=['one/**']\n[layers.second]\napp='two'\norder=1\npaths=['two/**']\n";

test("#949 three apps at a realistic width: the server between its two clients, side by side with both lines, the inspector open", {timeout:240000}, async () => published(serverFirstFiles, serverFirstRules, async ({page}) => {
  // A canvas of about 1100px beside the open inspector.
  await page.setViewportSize({width:1700, height:1000});
  await page.locator("#architecture-state[data-state=ready]").waitFor({state:"attached"});
  await page.waitForFunction(() => document.querySelectorAll(".architecture-app-column").length === 3 && document.querySelector(".architecture-apps-grid")?.dataset.layout === "columns"
    && !document.getElementById("architecture-diagram").dataset.layout).catch(() => {});
  const inputs = await layoutInputs(page);
  assert.ok(inputs.inspector > 0, `the inspector is open: ${JSON.stringify(inputs)}`);
  assert.ok(inputs.canvasClientWidth >= 1080 && inputs.canvasClientWidth <= 1180, `a canvas of about 1100px: ${JSON.stringify(inputs)}`);
  assert.equal(inputs.layout, "columns", `the three columns sit side by side: ${JSON.stringify(inputs)}`);
  assert.deepEqual(await page.locator(".architecture-app-column").evaluateAll(nodes => nodes.map(node => node.getAttribute("aria-label"))), ["two", "srv", "one"],
    "the client with more matched routes on the server's left, the other on its right");
  assert.equal(await page.locator(".architecture-apps-overlay .architecture-apps-line").count(), 2, "both interfaces run as lines");
  assert.equal(await page.locator(".architecture-interface-card .architecture-card-ends").count(), 0, "no end is left to words");
}));

test("all_apps_selected_word: a selected header or card says Selected, and is pressed", {timeout:240000}, async () => published(httpFiles, httpRules, async ({page}) => {
  await drawn(page);
  await columnsDrawn(page);
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
  await columnsDrawn(page);
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

test("app_switcher_narrow: at 760px every app button of a 120-character name stays inside the panel and answers a pointer", {timeout:240000}, async () => published(httpFiles, httpRules, async ({page}) => {
  await drawn(page);
  await page.setViewportSize({width:760, height:1000});
  await page.waitForFunction(() => document.querySelector(".architecture-apps-grid")?.dataset.layout === "stacked");
  const geometry = () => page.evaluate(() => {
    const panel = document.getElementById("architecture-panel"), box = panel.getBoundingClientRect();
    return {
      panel:{left:box.left, right:box.right, scrollLeft:panel.scrollLeft, scrollWidth:panel.scrollWidth, clientWidth:panel.clientWidth},
      buttons:[...document.querySelectorAll("#architecture-apps button")].map(node => {
        const r = node.getBoundingClientRect();
        return {text:node.textContent, left:r.left, right:r.right, top:r.top, bottom:r.bottom};
      }),
    };
  });
  const measured = await geometry();
  assert.equal(measured.buttons.length, 3, "All apps and both apps");
  assert.ok(measured.buttons.some(button => button.text.includes(longName)), "the long name is a choice");
  assert.ok(measured.panel.scrollWidth <= measured.panel.clientWidth + 1, `the toolbar fits the panel: ${JSON.stringify(measured.panel)}`);
  for (const button of measured.buttons) {
    assert.ok(button.left >= measured.panel.left - 0.5 && button.right <= measured.panel.right + 0.5, `${button.text.slice(0, 20)} lies within the panel: ${JSON.stringify(button)} in ${JSON.stringify(measured.panel)}`);
  }
  for (let at = measured.buttons.length - 1; at >= 0; at -= 1) {
    const now = (await geometry()).buttons[at];
    await page.mouse.click((now.left + now.right) / 2, (now.top + now.bottom) / 2);
    await page.waitForFunction(index => document.querySelectorAll("#architecture-apps button")[index]?.getAttribute("aria-pressed") === "true", at);
    assert.equal((await geometry()).panel.scrollLeft, 0, "no button was reached by scrolling the hidden overflow");
  }
}));

// #850: a Rust server `srv` (manifest first, so the left column) with a Shared layer `common`, and a
// JavaScript client `web`. The Shared layer's call back into `server` is a deviation, so its header has a chip. The interface runs between the ordered layers, so srv's Shared band goes
// on its left and no line crosses it. The control adds a second route whose handler is in the Shared
// layer, registered there: one end on each side is a tie, so the band keeps the right and the ordered end's line crosses it.
const sharedFiles = control => new Map([
  ["Cargo.toml", '[package]\nname = "srv"\n'],
  ["src/main.rs", `mod util;\nasync fn health() {}\nfn router() -> Router {\n    Router::new().route("/health", get(health))\n}\npub fn version() -> u32 { 2 }\nfn main() { util::helper(); }\n`],
  ["src/util.rs", `pub fn helper() -> u32 { crate::version() }\n${control ? 'pub async fn ping() {}\nfn routes() -> Router {\n    Router::new().route("/ping", get(ping))\n}\n' : ""}`],
  ["web/package.json", '{"name":"web"}\n'],
  ["web/app.js", 'async function load(api) {\n  await api("/health");\n  await api("/ping");\n}\n'],
]);
const sharedRules = "version=1\n[layers.server]\napp='srv'\norder=1\npaths=['src/main.rs']\n[layers.common]\napps=['srv']\nshared=true\npaths=['src/util.rs']\n" +
  "[layers.client]\napp='web'\norder=1\npaths=['web/**']\n";
// In page pixels: the Shared band, the overlay segments that enter it, its header text lines and chips, and their overlaps.
const sharedGeometry = page => page.evaluate(() => {
  const band = document.querySelector('.pele-cluster[data-trellis-layer-id="common"]'), frame = band.querySelector(":scope > rect").getBoundingClientRect();
  const stacks = [...document.querySelectorAll(".architecture-app-column")].find(column => column.getAttribute("aria-label") === "srv").querySelectorAll(".pele-cluster");
  const stack = [...stacks].find(cluster => cluster.querySelector(":scope > text")?.textContent.replace(/\s+/g, " ").includes("ordered")).querySelector(":scope > rect");
  const box = r => ({x:r.left, y:r.top, w:r.width, h:r.height});
  // A segment enters a box when it passes through the box shrunk by one pixel (Liang-Barsky).
  const enters = (a, b, r) => {
    const x0 = r.x + 1, x1 = r.x + r.w - 1, y0 = r.y + 1, y1 = r.y + r.h - 1;
    let t0 = 0, t1 = 1;
    const dx = b[0] - a[0], dy = b[1] - a[1];
    for (const [p, q] of [[-dx, a[0] - x0], [dx, x1 - a[0]], [-dy, a[1] - y0], [dy, y1 - a[1]]]) {
      if (p === 0) { if (q < 0) return false; continue; }
      const t = q / p;
      if (p < 0) { if (t > t1) return false; if (t > t0) t0 = t; } else { if (t < t0) return false; if (t < t1) t1 = t; }
    }
    return t1 - t0 > 1e-6;
  };
  const crossings = [];
  for (const path of document.querySelectorAll(".architecture-apps-line")) {
    const m = path.getScreenCTM(), pts = [...path.getAttribute("d").matchAll(/(-?[\d.]+),(-?[\d.]+)/g)].map(([, x, y]) => [m.a * x + m.c * y + m.e, m.b * x + m.d * y + m.f]);
    for (let i = 0; i + 1 < pts.length; i++) if (enters(pts[i], pts[i + 1], box(frame))) crossings.push(path.getAttribute("d"));
  }
  const lines = [...band.querySelectorAll(":scope > text tspan")].filter(span => span.textContent.trim()).map(span => ({text:span.textContent, ...box(span.getBoundingClientRect())}));
  const chips = [...band.querySelectorAll(":scope > rect")].slice(1).map(rect => ({text:"chip", ...box(rect.getBoundingClientRect())}));
  for (const action of document.querySelectorAll('.trellis-architecture-stat-action[data-trellis-layer-action="common"] > rect:first-child')) chips.push({text:"action", ...box(action.getBoundingClientRect())});
  const hit = (a, b) => a.x < b.x + b.w - 0.5 && b.x < a.x + a.w - 0.5 && a.y < b.y + b.h - 0.5 && b.y < a.y + a.h - 0.5;
  const overlaps = [], parts = [...lines, ...chips], f = box(frame);
  parts.forEach((a, i) => { for (const b of parts.slice(i + 1)) if (hit(a, b)) overlaps.push(`${a.text} × ${b.text}`); });
  const outside = parts.filter(part => part.x < f.x - 0.5 || part.x + part.w > f.x + f.w + 0.5).map(part => part.text);
  return {band:f, stack:box(stack.getBoundingClientRect()), crossings, lines:lines.map(line => line.text), chips:chips.length, overlaps, outside,
    lineCount:document.querySelectorAll(".architecture-apps-line").length};
});
async function sharedDrawn(page) {
  await drawn(page);
  await classicScrollbars(page);
  await columnsDrawn(page);
  await page.locator('.pele-cluster[data-trellis-layer-id="common"]').waitFor({state:"attached"});
  await page.locator(".architecture-apps-line").first().waitFor({state:"attached"});
  return sharedGeometry(page);
}

test("#850 shared_band_away_from_partner: srv's Shared band sits left of its stack, no interface line enters it, and its header text never overlaps", {timeout:240000}, async () => published(sharedFiles(false), sharedRules, async ({page}) => {
  const at = await sharedDrawn(page);
  const detail = JSON.stringify({...at, inputs:await layoutInputs(page)});
  assert.ok(at.band.x + at.band.w <= at.stack.x + 0.5, `the Shared band is left of srv's ordered stack: ${detail}`);
  assert.ok(at.lineCount >= 1, `the interface draws its line: ${detail}`);
  assert.deepEqual(at.crossings, [], `no interface line enters the Shared band: ${detail}`);
  assert.equal(at.lines[0].startsWith("Shared"), true, detail); assert.ok(at.lines.some(line => /^1 file/.test(line)), `the file count is drawn whole: ${detail}`);
  assert.ok(at.chips >= 1, `the Shared header draws its count chip: ${detail}`);
  assert.deepEqual(at.overlaps, [], `the Shared header's lines and chips do not overlap: ${detail}`);
  assert.deepEqual(at.outside, [], `the Shared header stays inside its band: ${detail}`);
  // The column's own arrows route around the moved band: the shared outbound arrow leaves the band's right side.
  const arrows = await page.evaluate(arrowGeometry, '.architecture-app-column[aria-label="srv"] svg.trellis-architecture-svg');
  assert.ok(arrows.arrows >= 1, `srv draws its shared arrow: ${JSON.stringify(arrows)}`);
  assert.deepEqual([arrows.crossings, arrows.tagHits, arrows.detached], [[], [], []], `srv's arrows cross no band, chip or tag: ${JSON.stringify(arrows)}`);
}));

test("#850 negative control: a Shared end and an ordered end toward the partner tie, the band keeps the right and the check sees the crossing", {timeout:240000}, async () => published(sharedFiles(true), sharedRules, async ({page}) => {
  const at = await sharedDrawn(page);
  const detail = JSON.stringify(at);
  assert.ok(at.band.x >= at.stack.x + at.stack.w - 0.5, `the tie keeps the Shared band right of the stack: ${detail}`);
  assert.ok(at.crossings.length >= 1, `the ordered end's line crosses the Shared band, and the check reports it: ${detail}`);
  assert.deepEqual(at.overlaps, [], `the header still never overlaps itself: ${detail}`);
  // The overlap check sees a chip moved up onto the header's last text line.
  await page.evaluate(() => { const chip = document.querySelectorAll('.pele-cluster[data-trellis-layer-id="common"] > rect')[1]; chip.setAttribute("y", String(Number(chip.getAttribute("y")) - 10)); });
  assert.notDeepEqual((await sharedGeometry(page)).overlaps, [], "a chip drawn over a header line is reported");
}));

// #913: srv has a hidden tooling layer at order 1, then entry and core; web has one layer. srv's
// entry → core call is a production reference from srv's layers; web's own layer has none.
const numberedFiles = new Map([
  ["Cargo.toml", '[package]\nname = "srv"\n'],
  ["src/main.rs", "mod core;\nmod tools;\nfn main() { core::work(); }\n"],
  ["src/core.rs", "pub fn work() -> u32 { 1 }\n"],
  ["src/tools.rs", "pub fn tool() -> u32 { crate::core::work() }\n"],
  ["web/package.json", '{"name":"web"}\n'],
  ["web/app.js", "export function load() { return 1; }\n"],
]);
const numberedRules = "version=1\n[layers.tooling]\napp='srv'\norder=1\ndisplay='hidden'\npaths=['src/tools.rs']\n" +
  "[layers.entry]\napp='srv'\norder=2\npaths=['src/main.rs']\n[layers.core]\napp='srv'\norder=3\npaths=['src/core.rs']\n" +
  "[layers.client]\napp='web'\norder=1\npaths=['web/**']\n";
const bandLabel = (page, layer) => page.locator(`#architecture-diagram [data-trellis-layer-id="${layer}"]`).first().evaluate(node => node.getAttribute("aria-label") || node.textContent);
const chooseApp = async (page, name) => {
  await page.locator("#architecture-apps button", {hasText:name}).click();
  await page.waitForFunction(text => [...document.querySelectorAll("#architecture-apps button")].some(button => button.textContent.startsWith(text) && button.getAttribute("aria-pressed") === "true"), name);
  await page.locator("#architecture-state[data-state=ready]").waitFor({state:"attached"});
};

test("#913 layer numbers count only the shown layers: a hidden first layer leaves entry at 1, in All apps, one app and the inspector", {timeout:240000}, async () => published(numberedFiles, numberedRules, async ({page}) => {
  await page.locator("#architecture-state[data-state=ready]").waitFor({state:"attached"});
  await page.locator('#architecture-diagram [data-trellis-layer-id="entry"]').first().waitFor({state:"attached"});
  assert.match(await bandLabel(page, "entry"), /^1 · entry/, "All apps: srv's first shown layer is 1");
  assert.match(await bandLabel(page, "core"), /^2 · core/);
  assert.match(await bandLabel(page, "client"), /^1 · client/, "each app counts from 1");
  await chooseApp(page, "srv");
  await page.locator('#architecture-diagram [data-trellis-layer-id="entry"]').first().waitFor({state:"attached"});
  assert.match(await bandLabel(page, "entry"), /^1 · entry/, "one app: the hidden tooling layer takes no number");
  assert.match(await bandLabel(page, "core"), /^2 · core/);
  await page.locator('#architecture-diagram [data-trellis-layer-id="entry"]').first().click();
  await page.waitForFunction(() => [...document.querySelectorAll("#architecture-inspector-content p")].some(p => /^1 · entry/.test(p.textContent)));
  // Shown on request, the hidden layer is labelled Hidden and the numbers do not move.
  await page.locator("#architecture-hidden-toggle").click();
  await page.locator('#architecture-diagram [data-trellis-layer-id="tooling"]').first().waitFor({state:"attached"});
  assert.match(await bandLabel(page, "tooling"), /^Hidden · tooling/);
  assert.match(await bandLabel(page, "entry"), /^1 · entry/);
}));

test("#913 one app's coverage footnote and About counts are that app's, the whole checkout's in All apps", {timeout:240000}, async () => published(numberedFiles, numberedRules, async ({page}) => {
  await page.locator("#architecture-state[data-state=ready]").waitFor({state:"attached"});
  const coverage = () => page.locator("#architecture-coverage").textContent();
  const about = () => page.locator("#architecture-notice-content p").nth(1).textContent();
  const sites = text => Number(text.match(/of ([\d,]+) reference sites/)[1].replace(/,/g, ""));
  const all = await coverage(), allAbout = await about();
  assert.match(all, /^\d+ of \d+ reference sites mapped to a module/, "All apps: the checkout's sites");
  assert.doesNotMatch(allAbout, /layers\./);
  await chooseApp(page, "web");
  await page.waitForFunction(() => /from web's layers/.test(document.getElementById("architecture-coverage").textContent));
  const web = await coverage();
  assert.match(web, /^0 of 0 reference sites from web's layers mapped to a module/, `web's layer has no reference sites: ${web} (checkout: ${all})`);
  assert.match(await about(), /^0 mapped reference sites · 0 bound by semantic evidence · 0 unresolved from web's layers\./);
  await chooseApp(page, "srv");
  await page.waitForFunction(() => /from srv's layers/.test(document.getElementById("architecture-coverage").textContent));
  const srv = await coverage();
  assert.ok(sites(srv) > 0 && sites(srv) <= sites(all), `srv counts its own sites: ${srv} (checkout: ${all})`);
  assert.match(await about(), /from srv's layers\./);
  await chooseApp(page, "All apps");
  await page.waitForFunction(text => document.getElementById("architecture-coverage").textContent === text, all);
}));
