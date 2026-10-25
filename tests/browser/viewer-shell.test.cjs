"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const {mkdirSync, writeFileSync, rmSync, realpathSync} = require("node:fs");
const {join} = require("node:path");
const {chromium} = require("playwright");
const H = require("./sequence-harness.cjs");

const maliciousName = '<img src=x onerror="globalThis.filenameExecuted=true">.js';
const flowSource = `function measured(items, enabled) {
  // Nested decisions make this callable distinct from the small helpers.
  let total = 0;
  for (const item of items) {
    if (enabled) {
      if (item > 0) total += transform(item);
      else total -= transform(-item);
    }
  }
  if (total < 0) throw new Error("negative");
  return total;
}
function transform(value) { return value + 1; }
`;
const otherSource = `function moderate(value) {
  if (value > 3) return value * 2;
  return value;
}
function tiny() { return 1; }
function smaller(value) { return value; }
`;
test.before(async () => H.setup(), {timeout:960000});
test.after(async () => H.cleanup());

async function published(callback) {
  const paths = H.fixture("function anchor() { return 0; }\n", "anchor.js");
  mkdirSync(join(paths.workspace, "src", "nested"), {recursive:true});
  writeFileSync(join(paths.workspace, "src", "flow.js"), flowSource);
  writeFileSync(join(paths.workspace, "src", "nested", "other.js"), otherSource);
  writeFileSync(join(paths.workspace, maliciousName), "function safeName() { return 2; }\n");
  let server, browser, context;
  try {
    const indexed = await H.run(H.binary, ["index", "--workspace", paths.workspace], {env:paths.env, timeout:120000});
    const pin = JSON.parse(indexed.stdout).status.revision;
    server = await H.startDaemon(paths, pin.indexRevision);
    browser = await chromium.launch();
    context = await browser.newContext({viewport:{width:1440, height:900}});
    const page = await context.newPage(), requests = [], errors = [];
    page.on("pageerror", error => errors.push(error.message));
    page.on("request", request => {
      const url = new URL(request.url());
      if (url.pathname.startsWith("/api/")) requests.push({url, method:request.method()});
    });
    await page.goto(`${server.base}/`, {waitUntil:"networkidle"});
    assert.deepEqual(requests, [], "opening the viewer does not authenticate or select a checkout");
    await page.locator("#token").fill(paths.token);
    await page.locator("#connect-form button[type=submit]").click();
    await page.locator("#checkout-select").waitFor({state:"visible"});
    await page.waitForFunction(() => !document.querySelector("#checkout-select").disabled);
    assert.equal(await page.locator("#checkout-select").inputValue(), "", "one checkout still requires explicit selection");
    assert.equal(await page.locator("#workspace").isVisible(), false);
    assert.ok(requests.every(row => ["/api/checkouts", "/api/daemon/status"].includes(row.url.pathname)), "Connect loads discovery only");
    await page.locator("#checkout-select").selectOption(server.rootKey);
    await page.locator("#workspace").waitFor({state:"visible"});
    await page.waitForFunction(() => document.querySelector("#metrics-state")?.textContent.includes("Indexed snapshot · max"));
    await page.locator('.directory-pick[title="src/"]').waitFor();
    await callback({page, paths, server, pin, requests});
    assert.deepEqual(errors, []);
    const metricsRequests = requests.filter(row => row.url.pathname.endsWith("/metrics"));
    assert.ok(metricsRequests.length > 0);
    for (const {url, method} of metricsRequests) {
      assert.equal(method, "GET");
      assert.equal(url.pathname, H.scoped(server.rootKey, "metrics"));
      assert.equal(url.searchParams.get("indexGeneration"), pin.indexGeneration);
      assert.equal(url.searchParams.get("indexRevision"), String(pin.indexRevision));
    }
  } finally {
    await context?.close();
    await browser?.close();
    await H.stopDaemon(server);
    rmSync(paths.temp, {recursive:true, force:true, maxRetries:10, retryDelay:100});
  }
}
async function metrics(server, paths, pin, aggregate, scope = "revision", row) {
  const query = new URLSearchParams({...pin, aggregate, scope});
  if (row) {
    query.set("sourceSetId", row.sourceSetId); query.set("path", row.path); query.set("topCognitive", "3");
    if (scope === "file") query.set("language", row.language);
  }
  const response = await H.api(server.base, paths.token, "GET", H.scoped(server.rootKey, `metrics?${query}`));
  assert.equal(response.status, 200, JSON.stringify(response.data));
  return response.data.payload;
}
const labels = {loc:"LOC (callable ranges)", sloc:"SLOC (callable ranges)", commentLines:"Comment lines (callable ranges)",
  cognitive:"Cognitive", cyclomatic:"Cyclomatic", maxNesting:"Max nesting", maintainabilityIndex:"Maintainability (higher is better)",
  halstead:"Halstead", fanIn:"Fan-in", fanOut:"Fan-out (call sites)", parameters:"Parameters", exits:"Exits"};
function displayedMetrics(values) {
  const pairs = [];
  function visit(value, keys) {
    const label = keys.map(key => labels[key] || key).join(" · ");
    if (value && typeof value === "object") {
      if (Object.hasOwn(value, "unavailable")) pairs.push([label, `— — ${value.unavailable}`]);
      else for (const [key, child] of Object.entries(value)) visit(child, [...keys, key]);
    } else pairs.push([label, typeof value === "number" ? String(value) : "—"]);
  }
  for (const [key, value] of Object.entries(values)) visit(value, [key]);
  return pairs;
}
async function assertInspection(page, payload, scope, path) {
  const row = payload[scope === "file" ? "files" : "directories"].find(item => item.path === path);
  assert.ok(row, `${scope} ${path} is indexed`);
  await page.waitForFunction(expected => {
    const target = document.querySelector("#metric-inspector-values");
    const pairs = [...(target?.querySelectorAll("dt") || [])].map(node => [node.textContent, node.nextElementSibling.textContent]);
    return JSON.stringify(pairs) === JSON.stringify(expected);
  }, displayedMetrics(row.metrics));
  assert.equal(await page.locator("#metric-inspector-title").textContent(), path);
  assert.match(await page.locator("#metric-inspector-scope").textContent(), new RegExp(`${scope} · ${payload.aggregate} · indexed snapshot · all indexed callables`));
  assert.match(await page.locator("#metric-inspector-state").textContent(), new RegExp(`${row.declarationCount} indexed callables`));
  const labelWidths = await page.locator("#metric-inspector-values dt").evaluateAll(nodes => nodes.map(node => node.getBoundingClientRect().width));
  assert.ok(labelWidths.every(width => width >= 80), "metric labels retain readable columns beside long unavailable reasons");
  const top = await page.locator("#metric-inspector-top li").allTextContents();
  assert.equal(top.length, payload.topCognitive.length + 1);
  for (const [index, callable] of payload.topCognitive.entries()) {
    for (const value of [callable.name, callable.kind, callable.sourceSetId, callable.language, callable.path,
      `UTF-8 bytes [${callable.range.start}, ${callable.range.end})`, `Cognitive ${callable.metrics.cognitive}`,
      `Cyclomatic ${callable.metrics.cyclomatic}`, `Max nesting ${callable.metrics.maxNesting}`]) assert.ok(top[index + 1].includes(value), value);
  }
  return row;
}
async function expandSources(page) {
  await page.locator('.directory-pick[title="src/"]').click();
  await page.locator('.directory-pick[title="src/nested/"]').waitFor();
  await page.locator('.directory-pick[title="src/nested/"]').click();
  await page.locator('.file-pick[title="src/nested/other.js"]').waitFor();
}
async function assertLegend(page, payload, metric) {
  const rows = await page.locator("#file-tree .file-pick").evaluateAll(nodes => nodes.map(node => ({
    path:node.title, directory:node.classList.contains("directory-pick"),
    value:node.querySelector(".metric-value > span:last-child")?.textContent,
    band:node.querySelector(".metric-swatch")?.className,
    background:node.querySelector(".metric-swatch") && getComputedStyle(node.querySelector(".metric-swatch")).backgroundColor,
  })));
  const valueAt = values => metric.split(".").reduce((value, key) => value[key], values);
  const values = rows.map(row => {
    const path = row.directory ? row.path.replace(/\/$/, "") : row.path;
    const indexed = payload[row.directory ? "directories" : "files"].find(item => item.path === path);
    assert.ok(indexed, path);
    const value = valueAt(indexed.metrics);
    assert.equal(row.value, String(value), `${metric} for ${path} is from the selected snapshot`);
    return value;
  });
  const sorted = [...values].sort((a, b) => a - b);
  const cuts = [.25, .5, .75].map(p => sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * p))]);
  const legend = await page.locator("#metric-legend .metric-legend-entry").evaluateAll(nodes => nodes.map(node => ({
    text:node.lastElementChild.textContent, band:node.firstElementChild.className,
    background:getComputedStyle(node.firstElementChild).backgroundColor,
  })));
  assert.deepEqual(legend.map(row => row.text), [`≤${cuts[0]}`, `>${cuts[0]}–${cuts[1]}`, `>${cuts[1]}–${cuts[2]}`, `>${cuts[2]}`]);
  assert.equal(new Set(legend.map(row => row.background)).size, 4, "legend shows four actual, distinct heat colors");
  assert.ok(legend.every(row => row.background !== "rgba(0, 0, 0, 0)"), "heat is painted, not only class names");
  for (let index = 0; index < rows.length; index++) {
    const rank = cuts.filter(cut => values[index] > cut).length;
    const severity = metric === "maintainabilityIndex" ? 3 - rank : rank;
    assert.ok(rows[index].band.includes(`heat-${severity}`), `${metric}: ${values[index]} severity ${severity}`);
    assert.ok(legend[rank].band.includes(`heat-${severity}`));
    assert.equal(rows[index].background, legend[rank].background, "tree heat matches the numeric legend interval");
  }
  return values;
}
async function noOverflow(page, width) {
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true, `${width}px page has no horizontal overflow`);
  for (const selector of [".app-header", ".canvas-area", ".metric-controls", ".view-tabs", "footer"]) {
    const box = await page.locator(selector).boundingBox();
    assert.ok(box && box.width > 0 && box.x >= 0 && box.x + box.width <= width + 1, `${selector} fits ${width}px viewport`);
  }
}

test("real indexed viewer exposes snapshot metrics, numeric heat legend, raw top callables and aggregates", {timeout:240000}, async () => published(async ({page, paths, server, pin, requests}) => {
  const listing = await H.api(server.base, paths.token, "GET", "/api/checkouts");
  assert.equal(listing.status, 200);
  const checkout = listing.data.checkouts.find(row => row.rootKey === server.rootKey);
  assert.equal(checkout.workspaceRoot, realpathSync(paths.workspace));
  assert.deepEqual(checkout.revision, pin);
  const checkoutPick = page.locator("#indexed-checkouts .checkout-pick");
  assert.equal(await checkoutPick.count(), 1);
  assert.equal(await checkoutPick.getAttribute("aria-pressed"), "true");
  assert.equal(await checkoutPick.getAttribute("title"), checkout.workspaceRoot);
  assert.ok((await checkoutPick.textContent()).includes(`rev ${pin.indexGeneration.slice(0, 8)}:${pin.indexRevision}`));
  const status = await H.api(server.base, paths.token, "GET", H.scoped(server.rootKey, "status"));
  assert.equal(status.status, 200, JSON.stringify(status.data));
  assert.deepEqual(status.data.revision, pin);
  const freshness = status.data.catchingUp ? "catching-up" : "fresh";
  assert.equal(await checkoutPick.locator(".checkout-freshness").getAttribute("data-state"), freshness);
  assert.equal(await page.locator("#workspace-index-state").getAttribute("data-state"), freshness);
  assert.match(await page.locator("#workspace-index-label").textContent(), new RegExp(`revision ${pin.indexGeneration.slice(0, 8)}:${pin.indexRevision}`));
  await expandSources(page);
  let payload = await metrics(server, paths, pin, "max");
  await assertLegend(page, payload, "cognitive");
  const file = payload.files.find(row => row.path === "src/flow.js");
  await page.locator('.file-pick[title="src/flow.js"]').click();
  const selectedFile = await metrics(server, paths, pin, "max", "file", file);
  const inspected = await assertInspection(page, selectedFile, "file", file.path);
  assert.deepEqual(Object.keys(inspected.metrics).sort(), Object.keys(labels).sort(), "every metric family is shown, including nested Halstead and exits");
  assert.equal(inspected.metrics.fanIn.value, null);
  assert.equal(inspected.metrics.fanIn.unavailable, "call resolution not yet produced (#40/#68)");
  assert.ok(selectedFile.topCognitive.some(row => row.name === "measured"));
  assert.equal(requests.some(row => row.url.pathname.endsWith("/source")), false, "metric inspection never fetches source");
  for (const metric of ["cyclomatic", "maxNesting", "halstead.volume", "fanOut", "maintainabilityIndex"]) {
    await page.locator("#metric-color").selectOption(metric);
    const values = await assertLegend(page, payload, metric);
    if (metric === "maintainabilityIndex") {
      assert.ok(new Set(values).size > 1, "real Maintainability values differ so reversed severity is exercised");
      assert.match(await page.locator("#metric-legend").textContent(), /higher is better/);
    }
  }
  await page.locator("#metric-color").selectOption("none");
  assert.equal(await page.locator("#file-tree .metric-swatch").count(), 0);
  assert.equal(await page.locator("#metric-legend").textContent(), "");
  await page.locator("#metric-color").selectOption("cognitive");
  const directory = payload.directories.find(row => row.path === "src");
  await page.locator('.directory-pick[title="src/"]').click();
  const directoryMax = await metrics(server, paths, pin, "max", "directory", directory);
  await assertInspection(page, directoryMax, "directory", "src");
  const descendants = payload.files.filter(row => row.path.startsWith("src/"));
  const callableRows = (await Promise.all(descendants.map(row => metrics(server, paths, pin, "max", "file", row)))).flatMap(file => file.declarations);
  for (const aggregate of ["sum", "mean", "max"]) {
    await page.locator("#metric-aggregate").selectOption(aggregate);
    payload = await metrics(server, paths, pin, aggregate);
    const selected = await metrics(server, paths, pin, aggregate, "directory", directory);
    const row = await assertInspection(page, selected, "directory", "src");
    if (aggregate === "mean") {
      const callableSum = callableRows.reduce((sum, item) => sum + item.metrics.cognitive, 0);
      assert.equal(row.metrics.cognitive, callableSum / callableRows.length, "directory mean is callable-weighted, not a mean of file means");
      assert.equal(row.declarationCount, 5);
      assert.equal(row.declarationCount, callableRows.length);
    }
    assert.deepEqual(selected.topCognitive, directoryMax.topCognitive, "top callable scores and order remain raw under every aggregate");
    await assertLegend(page, payload, "cognitive");
  }
  await noOverflow(page, 1440);
  const explorer = await page.locator("#explorer-panel").boundingBox();
  const canvas = await page.locator(".canvas-area").boundingBox();
  const inspector = await page.locator("#call-inspector").boundingBox();
  assert.ok(explorer.x + explorer.width <= canvas.x && canvas.x + canvas.width <= inspector.x, "desktop keeps separate explorer, canvas and inspector columns");
}));

test("390px drawers and explicit Files section retain selected evidence without fetching source or parsing filenames as HTML", {timeout:240000}, async () => published(async ({page, requests}) => {
  await expandSources(page);
  await page.locator('.file-pick[title="src/flow.js"]').click();
  const method = page.locator(".method-pick").filter({hasText:"measured"});
  await method.waitFor(); await method.click();
  await page.waitForFunction(() => document.querySelector("#sequence-state")?.textContent.includes("Pele rendered"));
  await page.locator("#method-source").click();
  await page.waitForFunction(() => document.querySelector("#source-path")?.textContent.includes("cached snapshot"));
  const source = await page.locator("#source").textContent();
  const selectedMethod = await page.locator(".method-pick.selected").textContent();
  const diagram = await page.locator("#sequence-diagram").textContent();
  await page.locator("#view-classes").click();
  assert.equal(await page.locator("#classes-panel").isVisible(), true);
  await page.locator("#view-libraries").click();
  assert.equal(await page.locator("#dependency-library").isVisible(), true);
  await page.locator("#view-sequence").click();
  assert.equal(await page.locator(".method-pick.selected").textContent(), selectedMethod);
  assert.equal(await page.locator("#sequence-diagram").textContent(), diagram);
  assert.equal(await page.locator("#source").textContent(), source);
  const sourceCount = () => requests.filter(row => row.url.pathname.endsWith("/source") || row.url.pathname.endsWith("/external/source")).length;
  const before = sourceCount();
  for (const width of [1440, 390]) {
    await page.setViewportSize({width, height:width === 390 ? 844 : 900});
    await page.locator("#section-files").focus(); await page.keyboard.press("Enter");
    assert.equal(await page.locator("body").evaluate(node => node.classList.contains("files-section")), true, "Files is a source section, not merely Explorer focus");
    assert.equal(await page.locator("#source").isVisible(), true);
    assert.equal(await page.locator("#source").textContent(), source);
    assert.equal(sourceCount(), before, "switching to Files does not select or read a file");
    assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true, `${width}px Files has no page overflow`);
    await page.locator("#section-diagrams").click();
    assert.equal(await page.locator("#sequence-diagram").textContent(), diagram);
    assert.equal(await page.locator("#source").textContent(), source);
    await noOverflow(page, width);
  }
  await page.locator("#explorer-toggle").click();
  assert.equal(await page.locator("#explorer-toggle").getAttribute("aria-expanded"), "true");
  const explorer = await page.locator("#explorer-panel").boundingBox();
  assert.ok(explorer && explorer.x >= 0 && explorer.x + explorer.width <= 390);
  await page.locator('.file-pick[title="src/flow.js"]').click();
  await page.locator("#metric-inspector").waitFor({state:"visible"});
  assert.equal(await page.locator("#inspector-toggle").getAttribute("aria-expanded"), "true");
  assert.equal(await page.locator("#explorer-toggle").getAttribute("aria-expanded"), "false");
  assert.equal(await page.locator("#explorer-panel").isVisible(), false, "mobile drawers are exclusive");
  const inspector = await page.locator("#call-inspector").boundingBox();
  assert.ok(inspector && inspector.x >= 0 && inspector.x + inspector.width <= 390);
  await page.keyboard.press("Escape");
  assert.equal(await page.locator("#inspector-toggle").evaluate(node => document.activeElement === node), true);
  assert.equal(await page.locator("#call-inspector").isVisible(), false);
  await page.locator("#explorer-toggle").click();
  const malicious = page.locator("#file-tree .file-pick").filter({hasText:maliciousName});
  assert.equal(await malicious.count(), 1);
  assert.equal(await malicious.getAttribute("title"), maliciousName);
  assert.equal(await page.locator("#file-tree img").count(), 0, "repository filename is text, not a created image element");
  await malicious.click();
  await page.waitForFunction(name => document.querySelector("#metric-inspector-title")?.textContent === name, maliciousName);
  assert.equal(await page.locator("#metric-inspector-title img").count(), 0);
  assert.equal(await page.evaluate(() => globalThis.filenameExecuted), undefined);
  assert.equal(await page.locator("#source").textContent(), source, "metric selection keeps the explicitly opened snapshot");
  assert.equal(sourceCount(), before, "drawer and filename interactions never read source");
  await noOverflow(page, 390);
}));
