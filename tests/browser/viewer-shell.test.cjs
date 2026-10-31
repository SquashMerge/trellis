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
const mixedSource = `pub fn production(value: i32) -> i32 {
    if value > 0 { return value; }
    0
}
#[cfg(feature = "test-support")]
pub fn support() -> i32 { 1 }
#[cfg(test)] mod checks {
    fn nested(value: i32) -> i32 {
        if value > 0 {
            if value > 1 { return value; }
        }
        0
    }
}
`;
const testOnlySource = `#![cfg(test)]
fn only(value: i32) -> i32 {
    if value > 0 { return value; }
    0
}
`;
async function capture(page, name) {
  const directory = process.env.TRELLIS_BROWSER_SCREENSHOTS;
  if (!directory) return;
  mkdirSync(directory, {recursive:true});
  await page.screenshot({path:join(directory, `${name}.png`), fullPage:true});
}
test.before(async () => H.setup(), {timeout:960000});
test.after(async () => H.cleanup());

async function published(callback, signal, {rules = null, git = false, additionalRootKeys = []} = {}) {
  const paths = H.fixture("function anchor() { return 0; }\n", "anchor.js");
  mkdirSync(join(paths.workspace, "src", "nested"), {recursive:true});
  writeFileSync(join(paths.workspace, "src", "flow.js"), flowSource);
  writeFileSync(join(paths.workspace, "src", "nested", "other.js"), otherSource);
  writeFileSync(join(paths.workspace, maliciousName), "function safeName() { return 2; }\n");
  mkdirSync(join(paths.workspace, "roles"));
  writeFileSync(join(paths.workspace, "roles", "mixed.rs"), mixedSource);
  writeFileSync(join(paths.workspace, "roles", "only.rs"), testOnlySource);
  for (const relative of ["tests/fixtures/input.js", "tools/synthetic-cohorts/templates/input.js", "web/vendor/pele-1.10.0-trellis.2.js"]) {
    const target = join(paths.workspace, relative); mkdirSync(require("node:path").dirname(target), {recursive:true});
    writeFileSync(target, "function capturedInput(value) { if (value) { return 1; } return 0; }\n");
  }
  // Noise the explorer hides by default: a dot-directory and a folder with no indexed files.
  mkdirSync(join(paths.workspace, ".tool-cache"));
  writeFileSync(join(paths.workspace, ".tool-cache", "state.txt"), "cache\n");
  mkdirSync(join(paths.workspace, "assets"));
  writeFileSync(join(paths.workspace, "assets", "notes.txt"), "not source\n");
  if (rules !== null) {
    mkdirSync(join(paths.workspace, ".trellis"), {recursive:true});
    writeFileSync(join(paths.workspace, ".trellis/rules.toml"), rules);
  }
  let server, browser, context;
  try {
    if (git) {
      for (const args of [["init", "-q", "-b", "measured-branch"], ["config", "user.name", "Chrome acceptance"],
        ["config", "user.email", "chrome@example.invalid"], ["add", "."], ["commit", "-qm", "Chrome baseline"]]) {
        await H.run("git", args, {cwd:paths.workspace, env:paths.env});
      }
    }
    const indexed = await H.run(H.binary, ["index", "--workspace", paths.workspace], {env:paths.env, timeout:120000});
    const pin = indexed.json().status.revision;
    server = await H.startDaemon(paths, pin.indexRevision);
    browser = await chromium.launch();
    context = await browser.newContext({viewport:{width:1440, height:900}});
    signal?.addEventListener("abort", () => { void context.close().catch(() => {}); }, {once:true});
    const page = await context.newPage(), requests = [], errors = [];
    page.on("pageerror", error => errors.push(error.message));
    page.on("console", message => { if (message.type() === "error" && /Content Security Policy|CSP/i.test(message.text())) errors.push(message.text()); });
    page.on("request", request => {
      const url = new URL(request.url());
      if (url.pathname.startsWith("/api/")) requests.push({url, method:request.method(), body:request.method() === "POST" ? request.postDataJSON() : null});
    });
    await page.goto(`${server.base}/`, {waitUntil:"networkidle"});
    assert.deepEqual(requests, [], "opening the viewer does not authenticate or select a checkout");
    await page.locator("#token").fill(paths.token);
    await page.locator("#connect-form button[type=submit]").click();
    await page.locator("#checkout-select").waitFor({state:"visible"});
    // Arrival (#447): the only checkout opens on Architecture without another click.
    await page.locator("#workspace").waitFor({state:"visible"});
    assert.equal(await page.locator("#checkout-select").inputValue(), server.rootKey, "the only checkout is selected on arrival");
    assert.equal(await page.locator("#view-architecture").getAttribute("aria-selected"), "true", "arrival lands on Architecture");
    assert.equal(requests.some(row => row.method === "POST"), false, "arrival never starts indexing");
    await page.waitForFunction(() => document.querySelector("#metrics-state")?.textContent.includes("Indexed snapshot · max"));
    await page.locator('.directory-pick[title="src/"]').waitFor();
    const reloadedPin = await callback({page, paths, server, pin, requests});
    const observedPins = reloadedPin ? [pin, reloadedPin] : [pin];
    assert.deepEqual(errors, []);
    const metricsRequests = requests.filter(row => row.url.pathname.endsWith("/metrics"));
    assert.ok(metricsRequests.length > 0);
    for (const {url, method} of metricsRequests) {
      assert.equal(method, "GET"); assert.equal(url.searchParams.get("workbench"), "true");
      assert.ok([server.rootKey, ...additionalRootKeys].some(root => url.pathname === H.scoped(root, "metrics")), "metrics remain scoped to an explicitly selected fixture checkout");
      assert.ok(observedPins.some(value => url.searchParams.get("indexGeneration") === value.indexGeneration &&
        url.searchParams.get("indexRevision") === String(value.indexRevision)), "metrics use a complete displayed pin");
      assert.ok(["production", "all"].includes(url.searchParams.get("role")), "browser explicitly selects its metric population");
    }
  } finally {
    await context?.close();
    await browser?.close();
    await H.stopDaemon(server);
    rmSync(paths.temp, {recursive:true, force:true, maxRetries:10, retryDelay:100});
  }
}
async function metrics(server, paths, pin, aggregate, scope = "revision", row, role = "production") {
  const query = new URLSearchParams({...pin, aggregate, scope, role, workbench:"true"});
  if (row) {
    query.set("sourceSetId", row.sourceSetId); query.set("path", row.path); query.set("topCognitive", "3");
    if (scope === "file") query.set("language", row.language);
  } else query.set("topCognitive", "8");
  const response = await H.api(server.base, paths.token, "GET", H.scoped(server.rootKey, `metrics?${query}`));
  assert.equal(response.status, 200, JSON.stringify(response.data));
  assert.equal(response.data.payload.workbench, true); assert.equal(response.data.payload.role, role);
  assert.equal(response.data.payload.indexGeneration, pin.indexGeneration);
  assert.equal(response.data.payload.indexRevision, pin.indexRevision);
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
  const population = payload.role === "production" ? "production callables" : "all indexed callables";
  assert.match(await page.locator("#metric-inspector-scope").textContent(), new RegExp(`${scope} · ${payload.aggregate} · indexed snapshot · ${population}`));
  assert.match(await page.locator("#metric-inspector-state").textContent(), new RegExp(`${row.declarationCount} ${payload.role === "all" ? "indexed callables" : population}`));
  assert.match(await page.locator("#metric-inspector-top li").first().textContent(), new RegExp(population));
  const labelWidths = await page.locator("#metric-inspector-values dt").evaluateAll(nodes => nodes.map(node => node.getBoundingClientRect().width));
  assert.ok(labelWidths.every(width => width >= 80), "metric labels retain readable columns beside long unavailable reasons");
  const top = await page.locator("#metric-inspector-top li").allTextContents();
  assert.equal(top.length, payload.topCognitive.length + 1);
  for (const [index, callable] of payload.topCognitive.entries()) {
    for (const value of [callable.name, callable.role, callable.kind, callable.sourceSetId, callable.language, callable.path,
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
  const {rows, legend} = await page.evaluate(() => ({
    rows:[...document.querySelectorAll("#file-tree .file-pick")].map(node => ({
      path:node.title, directory:node.classList.contains("directory-pick"),
      value:node.querySelector(".metric-value > span:last-child")?.textContent,
      band:node.querySelector(".metric-swatch")?.className,
      background:node.querySelector(".metric-swatch") && getComputedStyle(node.querySelector(".metric-swatch")).backgroundColor,
    })),
    legend:[...document.querySelectorAll("#metric-legend .metric-legend-entry")].map(node => ({
      text:node.lastElementChild.textContent, band:node.firstElementChild.className,
      background:getComputedStyle(node.firstElementChild).backgroundColor,
    })),
  }));
  const valueAt = values => metric.split(".").reduce((value, key) => value[key], values);
  const values = rows.map(row => {
    const path = row.directory ? row.path.replace(/\/$/, "") : row.path;
    const indexed = payload[row.directory ? "directories" : "files"].find(item => item.path === path);
    assert.ok(indexed, path);
    const value = valueAt(indexed.metrics);
    assert.equal(row.value, value === null ? "—" : String(value), `${metric} for ${path} is from the selected snapshot`);
    return value;
  });
  const relative = await page.locator("#metric-relative").isChecked();
  const neutral = payload.aggregate === "sum" && !relative;
  const fixed = {cognitive:[5,15,30],cyclomatic:[5,10,20],maxNesting:[2,3,4],"halstead.volume":[500,1000,2000],fanOut:[7,15,30]};
  const sorted = values.filter(value => typeof value === "number").sort((a, b) => a - b);
  const cuts = relative ? [.25,.5,.75].map(p => sorted[Math.min(sorted.length - 1, Math.floor(sorted.length * p))]) : fixed[metric];
  const ranges = metric === "maintainabilityIndex" && !relative ? ["≥40","≥20–<40","≥10–<20","<10"] :
    [`≤${cuts?.[0]}`, `>${cuts?.[0]}–${cuts?.[1]}`, `>${cuts?.[1]}–${cuts?.[2]}`, `>${cuts?.[2]}`];
  if (neutral) {
    assert.match(await page.locator("#metric-legend").textContent(), /relative totals; no callable risk bands/);
    assert.deepEqual(legend, []); assert.ok(rows.every(row => row.band.includes("neutral"))); return values;
  }
  assert.deepEqual(legend.map(row => row.text), ranges);
  assert.equal(new Set(legend.map(row => row.background)).size, 4, "four painted severity colors");
  for (let index = 0; index < rows.length; index++) {
    if (values[index] === null) { assert.ok(rows[index].band.includes("neutral")); continue; }
    const severity = metric === "maintainabilityIndex" && !relative ? [40,20,10].filter(cut => values[index] < cut).length :
      metric === "maintainabilityIndex" ? 3 - cuts.filter(cut => values[index] > cut).length : cuts.filter(cut => values[index] > cut).length;
    assert.ok(rows[index].band.includes(`heat-${severity}`), `${metric}: ${values[index]} severity ${severity}`);
    const entry = legend.find(row => row.band.includes(`heat-${severity}`));
    assert.equal(rows[index].background, entry.background, "tree heat matches the active legend interval");
  }
  return values;
}
async function noOverflow(page, width) {
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true, `${width}px page has no horizontal overflow`);
  for (const selector of [".app-header", ".canvas-area", ".view-tabs", "footer"]) {
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
  assert.equal(await page.locator("#indexed-checkouts").count(), 0, "the rail no longer repeats the header picker");
  const option = page.locator(`#checkout-select option[value="${server.rootKey}"]`);
  assert.equal(await option.getAttribute("title"), checkout.workspaceRoot);
  assert.ok((await option.textContent()).includes(`rev ${pin.indexGeneration.slice(0, 8)}:${pin.indexRevision}`));
  const status = await H.api(server.base, paths.token, "GET", H.scoped(server.rootKey, "status"));
  assert.equal(status.status, 200, JSON.stringify(status.data));
  assert.deepEqual(status.data.revision, pin);
  const freshness = status.data.catchingUp ? "catching-up" : "fresh";
  assert.equal(await page.locator("#workspace-index-state").getAttribute("data-state"), freshness);
  assert.match(await page.locator("#workspace-index-label").textContent(), new RegExp(`rev(?:ision)? ${pin.indexGeneration.slice(0, 8)}:${pin.indexRevision}`));
  await expandSources(page);
  let payload = await metrics(server, paths, pin, "max");
  await assertLegend(page, payload, "cognitive");
  assert.equal(await page.locator("#view-architecture").getAttribute("aria-selected"), "true", "Architecture shares the explorer heat surface");
  const swatchTip = await page.locator('.file-pick[title="src/flow.js"] .metric-value').getAttribute("title");
  assert.match(swatchTip, /^Cognitive · max · production callables · indexed snapshot: \d+ · Fixed per-callable bands · band /, "a swatch's tooltip carries the legend");
  assert.equal(await page.locator("#explorer-metric-legend").count(), 0, "no separate rail legend");
  assert.equal(await page.locator("#metric-relative").isChecked(), false);
  const heatBefore = await page.locator('.file-pick[title="src/flow.js"] .metric-swatch').getAttribute("class");
  await page.locator("#file-filter").fill("src/flow.js");
  assert.equal(await page.locator('.file-pick[title="src/flow.js"] .metric-swatch').getAttribute("class"), heatBefore, "same file keeps fixed color when explorer filtering changes");
  await page.locator("#file-filter").fill("");
  const readsBefore = requests.filter(row => row.url.pathname.endsWith("/metrics")).length;
  await H.reveal(page, "#metric-relative"); await page.locator("#metric-relative").check(); await assertLegend(page, payload, "cognitive");
  assert.match(await page.locator("#metric-legend").textContent(), /Relative to visible set/);
  assert.equal(requests.filter(row => row.url.pathname.endsWith("/metrics")).length, readsBefore);
  await H.reveal(page, "#metric-relative"); await page.locator("#metric-relative").uncheck(); await assertLegend(page, payload, "cognitive");
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
    await H.reveal(page, "#metric-color"); await page.locator("#metric-color").selectOption(metric);
    const values = await assertLegend(page, payload, metric);
    if (metric === "maintainabilityIndex") {
      assert.ok(new Set(values).size > 1, "real Maintainability values differ so reversed severity is exercised");
      assert.match(await page.locator("#metric-legend").textContent(), /higher is better/);
    }
  }
  await H.reveal(page, "#metric-color"); await page.locator("#metric-color").selectOption("none");
  assert.equal(await page.locator("#file-tree .metric-swatch").count(), 0);
  assert.equal(await page.locator("#metric-legend").textContent(), "");
  await H.reveal(page, "#metric-color"); await page.locator("#metric-color").selectOption("cognitive");
  const directory = payload.directories.find(row => row.path === "src");
  await page.locator('.directory-pick[title="src/"]').click();
  const directoryMax = await metrics(server, paths, pin, "max", "directory", directory);
  await assertInspection(page, directoryMax, "directory", "src");
  const descendants = payload.files.filter(row => row.path.startsWith("src/"));
  const callableRows = (await Promise.all(descendants.map(row => metrics(server, paths, pin, "max", "file", row)))).flatMap(file => file.declarations);
  for (const aggregate of ["sum", "mean", "max"]) {
    await H.reveal(page, "#metric-aggregate"); await page.locator("#metric-aggregate").selectOption(aggregate);
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

test("Viewer starts with production heat and includes stored tests at the same complete pin", {timeout:240000}, async () => published(async ({page, paths, server, pin, requests}) => {
  assert.equal(await page.locator("#metric-include-tests").isChecked(), false);
  assert.match(await page.locator("#metrics-state").textContent(), /production callables/);
  assert.ok(requests.filter(row => row.url.pathname.endsWith("/metrics")).every(row => row.url.searchParams.get("role") === "production"));
  await page.locator('.directory-pick[title="roles/"]').click();
  const initial = await metrics(server, paths, pin, "max");
  const mixed = initial.files.find(row => row.path === "roles/mixed.rs");
  const only = initial.files.find(row => row.path === "roles/only.rs");
  assert.deepEqual(mixed.roleCounts, {production:2, test:1});
  assert.deepEqual(only.roleCounts, {production:0, test:1});
  const fixtureScores = {production:[1, 0], all:[1, 0, 3]};
  const expectedCognitive = {production:{max:1, sum:1, mean:0.5}, all:{max:3, sum:4, mean:1.333333}};
  // Hand count: one production if, zero in test-support, and two nested test ifs (1 + 2).
  for (const role of ["production", "all"]) {
    if (role === "all") {
      await H.reveal(page, "#metric-include-tests"); await page.locator("#metric-include-tests").check();
      await page.waitForFunction(() => document.querySelector("#metrics-state")?.textContent.includes("all indexed callables"));
    }
    for (const aggregate of ["max", "sum", "mean"]) {
      await H.reveal(page, "#metric-aggregate"); await page.locator("#metric-aggregate").selectOption(aggregate);
      await page.waitForFunction(value => document.querySelector("#metrics-state")?.textContent.includes(`Indexed snapshot · ${value} ·`), aggregate);
      const payload = await metrics(server, paths, pin, aggregate, "file", mixed, role);
      const scores = fixtureScores[role], file = payload.files.find(row => row.path === mixed.path);
      assert.equal(file.metrics.cognitive, expectedCognitive[role][aggregate], `${role} ${aggregate} is hand-checked at the contract's six-decimal precision`);
      assert.equal(file.declarationCount, scores.length);
      assert.deepEqual(file.roleCounts, mixed.roleCounts, "population counts do not depend on selected role");
      assert.deepEqual(payload.topCognitive.map(row => [row.name, row.role, row.metrics.cognitive]).sort(),
        [["production", "production", 1], ["support", "production", 0], ...(role === "all" ? [["nested", "test", 3]] : [])].sort());
      assert.equal(payload.declarations.length, payload.topCognitive.length, "the three-row top list covers every fixture callable");
      for (const top of payload.topCognitive) {
        const declarations = payload.declarations.filter(row => row.sourceSetId === top.sourceSetId && row.language === top.language && row.path === top.path && row.sid === top.sid);
        assert.equal(declarations.length, 1, "named top row binds to exactly one full native declaration identity");
        assert.equal(declarations[0].role, top.role);
        assert.deepEqual(declarations[0].range, top.range);
        assert.equal(declarations[0].metrics.cognitive, top.metrics.cognitive);
      }
      await page.locator('.file-pick[title="roles/mixed.rs"]').click();
      await assertInspection(page, payload, "file", mixed.path);
      assert.match(await page.locator("#metric-legend").textContent(), new RegExp(role === "production" ? "production callables" : "all indexed callables"));
    }
    if (role === "all") {
      assert.equal(await page.locator('.file-pick[title="roles/only.rs"] .metric-value > span:last-child').textContent(), "1", "including tests makes the test-only callable measured");
      assert.doesNotMatch(await page.locator('.file-pick[title="roles/only.rs"] .metric-swatch').getAttribute("class"), /neutral/);
    }
    await capture(page, `viewer-${role}-desktop`);
    await page.setViewportSize({width:390, height:844});
    if (await page.locator("#inspector-toggle").getAttribute("aria-expanded") === "true") await page.locator("#inspector-toggle").click();
    await page.locator("#explorer-toggle").click();
    await H.reveal(page, "#metric-include-tests");
    assert.equal(await page.locator("#metric-include-tests").isVisible(), true, "heat options stay reachable in the mobile explorer menu");
    await capture(page, `viewer-${role}-mobile`);
    await page.keyboard.press("Escape");
    await page.locator("#explorer-toggle").click();
    await page.locator("#inspector-toggle").click();
    await capture(page, `viewer-${role}-mobile-inspector`);
    await page.setViewportSize({width:1440, height:900});
    if (role === "production") {
      await page.locator('.file-pick[title="roles/only.rs"]').click();
      await page.waitForFunction(() => document.querySelector("#metric-inspector-state")?.textContent.includes("No production callables"));
      assert.equal(await page.locator('.file-pick[title="roles/only.rs"] .metric-value > span:last-child').textContent(), "—");
      assert.match(await page.locator('.file-pick[title="roles/only.rs"] .metric-swatch').getAttribute("class"), /neutral/);
      assert.match(await page.locator("#metric-inspector-top").textContent(), /No production callables/);
      assert.equal((await metrics(server, paths, pin, "mean", "file", only)).files[0].metrics.cognitive, null);
    }
  }
  assert.equal(requests.some(row => row.url.pathname.endsWith("/source")), false, "population changes never read source");
}));

test("Viewer rejects a proven late production response after include-tests wins at the same pin", {timeout:240000}, async () => published(async ({page, paths, server, pin}) => {
  const production = await metrics(server, paths, pin, "max");
  let entered, release;
  const seen = new Promise(resolve => { entered = resolve; });
  const gate = new Promise(resolve => { release = resolve; });
  const pattern = "**/api/checkouts/*/metrics?*";
  const respond = async route => {
    const url = new URL(route.request().url());
    if (url.searchParams.get("scope") !== "revision" || url.searchParams.get("role") !== "production") return route.continue();
    entered(); await gate;
    await route.fulfill({status:200, contentType:"application/json", body:JSON.stringify({payload:production})});
  };
  await page.evaluate(() => {
    const original = window.fetch;
    window.__roleFetch = original;
    window.fetch = async (...args) => {
      const response = await original(...args), url = new URL(args[0], location.href);
      if (url.pathname.endsWith("/metrics") && url.searchParams.get("role") === "production" && url.searchParams.get("scope") === "revision") {
        const json = response.json.bind(response);
        response.json = async () => { const data = await json(); window.__lateProductionConsumed = true; return data; };
      }
      return response;
    };
  });
  try {
    await H.reveal(page, "#metric-include-tests"); await page.locator("#metric-include-tests").check();
    await page.waitForFunction(() => document.querySelector("#metrics-state")?.textContent.includes("all indexed callables"));
    await page.locator('.directory-pick[title="roles/"]').click();
    await page.locator('.file-pick[title="roles/mixed.rs"]').click();
    await page.waitForFunction(() => document.querySelector("#metric-inspector-values dt"));
    await page.route(pattern, respond);
    await H.reveal(page, "#metric-include-tests"); await page.locator("#metric-include-tests").uncheck();
    await seen;
    assert.equal(await page.locator("#metric-inspector-values dt").count(), 0, "changing role clears inspection before a response exists");
    assert.equal(await page.locator("#file-tree .heat-0, #file-tree .heat-1, #file-tree .heat-2, #file-tree .heat-3").count(), 0, "old population heat is cleared while production is blocked");
    await H.reveal(page, "#metric-include-tests"); await page.locator("#metric-include-tests").check();
    await page.waitForFunction(() => document.querySelector("#metrics-state")?.textContent.includes("all indexed callables"));
    const allHeat = await page.locator("#file-tree").textContent();
    release();
    await page.waitForFunction(() => window.__lateProductionConsumed === true);
    await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
    assert.match(await page.locator("#metrics-state").textContent(), /all indexed callables/);
    assert.equal(await page.locator("#metric-include-tests").isChecked(), true);
    assert.equal(await page.locator("#file-tree").textContent(), allHeat, "the consumed old response cannot repaint the chosen population");
  } finally {
    release(); await page.unroute(pattern, respond);
    await page.evaluate(() => { window.fetch = window.__roleFetch; delete window.__roleFetch; delete window.__lateProductionConsumed; });
  }
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



test("Sequence URLs wait for authentication and checkout, use the returned seed, and refuse invalid context", {timeout:240000}, async () => published(async ({page, paths, server, pin, requests}) => {
  const seed = await H.seedId(server, paths.token, "transform");
  const sequenceRequests = () => requests.filter(row => row.url.pathname.endsWith("/sequence"));
  const pinned = new URLSearchParams({sid:seed, rootKey:server.rootKey, ...pin});
  async function authenticateRoute(query, {valid = false, backend = false} = {}) {
    await page.goto("about:blank");
    const before = sequenceRequests().length;
    await page.goto(`${server.base}/#sequence?${query}`, {waitUntil:"domcontentloaded"});
    assert.equal(sequenceRequests().length, before, "a URL never reads evidence before Connect");
    const responsePromise = valid || backend ? page.waitForResponse(response => new URL(response.url()).pathname.endsWith("/sequence")) : null;
    await page.locator("#token").fill(paths.token);
    await page.locator("#connect-form button[type=submit]").click();
    // Arrival selects the only checkout; the URL itself never chooses one.
    await page.locator("#workspace").waitFor({state:"visible"});
    assert.equal(await page.locator("#checkout-select").inputValue(), server.rootKey);
    if (valid) {
      const response = await responsePromise, dto = await response.json();
      assert.equal(response.status(), 200, JSON.stringify(dto));
      assert.equal(response.request().postDataJSON().seed, seed); assert.deepEqual(response.request().postDataJSON().expectedRevision, pin);
      assert.equal(dto.seed.id, seed); assert.equal(dto.seed.name, "transform"); assert.deepEqual(dto.revision, pin);
      await page.waitForFunction(() => document.querySelector("#sequence-state")?.textContent.includes("Pele rendered"));
      assert.equal(await page.locator("#method-source").isEnabled(), true);
      await page.locator("#method-source").click();
      await assertExactSource(page, flowSource);
      assert.deepEqual(await page.locator("#source .highlight .line-number").allTextContents(), [String(dto.seed.range.startLine)]);
    } else {
      if (responsePromise) assert.notEqual((await responsePromise).status(), 200);
      await page.waitForFunction(() => !document.querySelector("#error").hidden && document.querySelector("#sequence-state")?.textContent.length > 0);
      assert.match(await page.locator("#sequence-state").textContent(), /invalid|unavailable|not found|select|snapshot|revision|match|stale|unknown|requires|symbol|seed/i);
      assert.equal(await page.locator("#sequence-diagram [data-trellis-step-id]").count(), 0, "refusal never displays a guessed successor sequence");
      assert.equal(await page.locator("#method-source").isEnabled(), false);
      assert.equal(sequenceRequests().length, before + (backend ? 1 : 0));
    }
  }
  await authenticateRoute(new URLSearchParams({sid:seed}), {valid:true});
  await authenticateRoute(pinned, {valid:true});
  for (const patch of [{rootKey:"f".repeat(64)}, {indexGeneration:"00000000-0000-4000-8000-000000000000"}, {indexRevision:String(pin.indexRevision + 1)}]) {
    const query = new URLSearchParams(pinned);
    for (const [key, value] of Object.entries(patch)) query.set(key, value);
    await authenticateRoute(query);
  }
  await authenticateRoute(new URLSearchParams({sid:"missing-explicit-callable"}), {backend:true});
  await authenticateRoute(new URLSearchParams());
}));

// #450: with no method selected, Sequence starts from the most complex methods of the
// metrics projection; those rows and the metric inspector's open the exact sid's sequence.
test("Sequence starts from the most complex named methods and the inspector opens a method's sequence", {timeout:240000}, async () => published(async ({page, paths, server, requests}) => {
  const pin = (await H.api(server.base, paths.token, "GET", H.scoped(server.rootKey, "status"))).data.revision;
  const projection = await metrics(server, paths, pin, "max", "revision");
  const named = projection.topCognitive.filter(row => row.name).slice(0, 5);
  assert.ok(named.length >= 2, JSON.stringify(projection.topCognitive));
  await page.locator("#view-sequence").click();
  const starts = page.locator("#sequence-start-list button");
  await starts.first().waitFor();
  const revisionRead = requests.find(row => row.url.pathname.endsWith("/metrics") && row.url.searchParams.get("scope") === "revision");
  assert.equal(revisionRead.url.searchParams.get("topCognitive"), "8");
  assert.deepEqual(await starts.evaluateAll(nodes => nodes.map(node => node.firstChild.textContent)), named.map(row => row.name));
  for (const [index, row] of named.entries()) assert.match(await starts.nth(index).textContent(), new RegExp(`${row.path.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")} · ${row.kind} · cognitive ${row.metrics.cognitive}`));
  assert.match(await page.locator("#sequence-start-note").textContent(), /Highest cognitive complexity · production callables/);
  assert.equal(await page.locator("#sequence-start img").count(), 0);
  await capture(page, "sequence-start");
  const sequenceRead = page.waitForRequest(request => new URL(request.url()).pathname.endsWith("/sequence"));
  await starts.first().click();
  const body = (await sequenceRead).postDataJSON();
  assert.equal(body.seed, named[0].sid); assert.deepEqual(body.expectedRevision, pin);
  await page.waitForFunction(() => document.querySelector("#sequence-state")?.textContent.includes("Pele rendered"));
  assert.match(await page.locator("#sequence-state").textContent(), new RegExp(`^${named[0].name} · ${named[0].path}`));
  assert.equal(await page.locator("#sequence-start").isVisible(), false, "the start list leaves once a method is open");
  // The metric inspector's highest-cognitive callables open the same way.
  await expandSources(page);
  await page.locator('.file-pick[title="src/flow.js"]').click();
  const measured = page.locator("#metric-inspector-top button.metric-top-open").filter({hasText:/^measured$/});
  await measured.waitFor();
  const fileRow = projection.files.find(row => row.path === "src/flow.js");
  const fileMetrics = await metrics(server, paths, pin, "max", "file", fileRow);
  const inspectorRead = page.waitForRequest(request => new URL(request.url()).pathname.endsWith("/sequence"));
  await measured.click();
  assert.equal((await inspectorRead).postDataJSON().seed, fileMetrics.topCognitive.find(row => row.name === "measured").sid);
  await page.waitForFunction(() => /^measured · src\/flow\.js .*Pele rendered/.test(document.querySelector("#sequence-state")?.textContent || ""));
  assert.equal(await page.locator("#sequence-panel").isVisible(), true);
}));

async function assertExactSource(page, text) {
  await page.waitForFunction(expected => [...document.querySelectorAll("#source .source-line")].map(line =>
    [...line.childNodes].filter(node => !node.classList?.contains("line-number")).map(node => node.textContent).join("")).join("\n") === expected, text);
  const actual = await page.locator("#source .source-line").evaluateAll(lines => lines.map(line =>
    [...line.childNodes].filter(node => !node.classList?.contains("line-number")).map(node => node.textContent).join("")).join("\n"));
  assert.equal(actual, text, "source keeps the exact indexed bytes, excluding line-number decoration");
}

test("retained Files and diagram evidence names the newer head until explicit keyboard reload", {timeout:240000}, async t => published(async ({page, paths, server, pin, requests}) => {
  const label = value => `${value.indexGeneration.slice(0, 8)}:${value.indexRevision}`;
  const atPin = (response, suffix, expected) => {
    const url = new URL(response.url());
    return url.pathname === H.scoped(server.rootKey, suffix) &&
      url.searchParams.get("indexGeneration") === expected.indexGeneration &&
      url.searchParams.get("indexRevision") === String(expected.indexRevision);
  };
  await expandSources(page);
  await page.locator('.file-pick[title="src/flow.js"]').click();
  await page.locator(".method-pick").filter({hasText:"measured"}).click();
  await page.waitForFunction(() => document.querySelector("#sequence-state")?.textContent.includes("Pele rendered"));
  const initialSource = page.waitForResponse(response => atPin(response, "source", pin));
  await page.locator("#method-source").click();
  const initial = await initialSource;
  assert.equal(initial.status(), 200);
  assert.deepEqual((await initial.json()).revision, pin);
  assert.equal((await initial.json()).file.text, flowSource);
  await assertExactSource(page, flowSource);
  assert.equal(await page.locator("#retained-pin-status").isVisible(), false, "equal full pins do not claim older evidence");
  await capture(page, "viewer-at-pin-desktop");

  const flowQ = flowSource.replace("value + 1", "value + 20");
  writeFileSync(join(paths.workspace, "src", "flow.js"), flowQ);
  const head = await H.indexThroughApi(server, paths.token, pin, "src/flow.js", flowQ);
  const sequenceRead = page.waitForResponse(response => response.url().endsWith(H.scoped(server.rootKey, "sequence")) &&
    response.request().postDataJSON()?.showAll === true);
  await H.reveal(page, "#all-steps"); await page.locator("#all-steps").check();
  const retainedSequence = await sequenceRead;
  assert.equal(retainedSequence.status(), 200, "the next actual pinned read still succeeds after head advances");
  assert.deepEqual(retainedSequence.request().postDataJSON().expectedRevision, pin);
  assert.deepEqual((await retainedSequence.json()).revision, pin);
  await page.waitForFunction(() => document.querySelector("#sequence-state")?.textContent.includes("Pele rendered"));
  await page.waitForFunction(() => document.querySelector("#retained-pin-status")?.hidden === false);
  const older = `Viewing revision ${label(pin)} · Older than head ${label(head)}`;
  assert.equal(await page.locator("#retained-pin-label").textContent(), older);
  assert.match(await page.locator("#sequence-state").textContent(), new RegExp(`revision ${label(pin)}`));
  await capture(page, "viewer-retained-diagram-desktop");
  await page.locator("#method-source").click();
  await assertExactSource(page, flowSource);
  assert.equal(await page.locator("#retained-pin-label").textContent(), older, "cache-hit source retains the same advisory comparison");

  await page.locator("#section-files").click();
  await page.waitForFunction(() => document.body.classList.contains("files-section"));
  const sourceRead = page.waitForResponse(response => atPin(response, "source", pin) &&
    new URL(response.url()).searchParams.get("path") === "src/nested/other.js");
  await page.locator('.file-pick[title="src/nested/other.js"]').click();
  const retainedSource = await sourceRead;
  assert.equal(retainedSource.status(), 200, "uncached Files source is read from the retained pin, not head");
  const retained = await retainedSource.json();
  assert.deepEqual(retained.revision, pin);
  assert.equal(retained.file.text, otherSource);
  await assertExactSource(page, otherSource);
  assert.equal(await page.locator("#retained-pin-label").textContent(), older, "Files and diagrams share the retained-pin status");
  assert.ok((await page.locator("#files-basis-note").textContent()).includes(label(pin)));
  await capture(page, "viewer-retained-pin-desktop");

  await page.setViewportSize({width:390, height:844});
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true, "390px Files has no page overflow");
  const banner = page.locator("#retained-pin-status");
  const box = await banner.boundingBox();
  assert.ok(box && box.x >= 0 && box.x + box.width <= 391, "the retained-pin banner fits a 390px viewport");
  assert.equal(await banner.getAttribute("role"), "status");
  assert.equal(await banner.getAttribute("aria-live"), "polite");
  const reload = page.locator("#reload-head");
  await reload.focus();
  assert.equal(await reload.evaluate(node => document.activeElement === node), true);
  assert.equal(await reload.textContent(), "Reload at head");
  await capture(page, "viewer-retained-pin-mobile");
  const reloadRequestStart = requests.length;
  const reloadGit = page.waitForResponse(response => new URL(response.url()).pathname === H.scoped(server.rootKey, "git"));
  await reload.press("Enter");
  assert.equal((await reloadGit).status(), 200);
  await page.waitForFunction(() => !document.querySelector("#reload-head").disabled);
  const actualHead = await H.api(server.base, paths.token, "GET", H.scoped(server.rootKey, "status"));
  assert.equal(actualHead.status, 200);
  assert.deepEqual(actualHead.data.revision, head, "explicit reload adopts the current head, independently verified from the daemon");
  await page.waitForFunction(expected => new RegExp(`rev(?:ision)? ${expected}`).test(document.querySelector("#workspace-index-label")?.textContent || ""), label(head));
  assert.equal(await page.locator(".method-pick.selected").count(), 0, "reload does not carry an old symbol selection into head");
  assert.equal(await page.locator("#source .source-line").count(), 0, "reload clears old source before a new file selection");
  assert.equal(await banner.isVisible(), false);
  if (await page.locator("#explorer-toggle").getAttribute("aria-expanded") !== "true") await page.locator("#explorer-toggle").click();
  const headSource = page.waitForResponse(response => atPin(response, "source", head) &&
    new URL(response.url()).searchParams.get("path") === "src/flow.js");
  await page.locator('.file-pick[title="src/flow.js"]').click();
  const selectedHead = await headSource;
  assert.equal(selectedHead.status(), 200);
  const current = await selectedHead.json();
  assert.deepEqual(current.revision, head);
  assert.equal(current.file.text, flowQ);
  await assertExactSource(page, flowQ);
  assert.equal(await banner.isVisible(), false, "an at-head source read has no older-than-head indication");
  assert.ok(requests.slice(reloadRequestStart).filter(row => row.url.pathname.endsWith("/source")).every(row =>
    row.url.searchParams.get("indexGeneration") === head.indexGeneration && row.url.searchParams.get("indexRevision") === String(head.indexRevision)));
  assert.equal(requests.some(row => row.method === "POST" && row.url.pathname.endsWith("/index")), false, "comparison and reload never index automatically");
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true, "390px Files has no page overflow");
  await capture(page, "viewer-reloaded-head-mobile");
  return head;
}, t.signal));

test("explorer hides dot and unindexed folders, status says what was indexed, and the inspector appears only for a selection", {timeout:240000}, async () => published(async ({page}) => {
  await page.waitForFunction(() => document.querySelector('.directory-pick[title="roles/"]') && !document.querySelector('.directory-pick[title="assets/"]'));
  assert.equal(await page.locator('.directory-pick[title=".tool-cache/"]').count(), 0, "dot-directories are hidden by default");
  assert.equal(await page.locator('.directory-pick[title="assets/"]').count(), 0, "a folder without indexed files is hidden by default");
  assert.match(await page.locator("#files-state").textContent(), /^2 hidden or unindexed entries · ⋯ to show$/);
  await page.locator("#rail-menu > summary").click();
  const toggle = page.getByRole("checkbox", {name:"Show hidden/ignored", exact:true});
  await toggle.focus(); await page.keyboard.press("Space");
  await page.keyboard.press("Escape");
  assert.equal(await page.locator("#rail-menu").evaluate(node => node.open), false, "Escape closes the rail menu");
  assert.equal(await page.locator("#rail-menu > summary").evaluate(node => document.activeElement === node), true, "focus returns to the menu button");
  await page.locator('.directory-pick[title=".tool-cache/"]').waitFor();
  await page.locator('.directory-pick[title="assets/"]').waitFor();
  assert.equal(await page.evaluate(key => localStorage.getItem(key), `trellis.files.showHidden.v1:${await page.locator("#checkout-select").inputValue()}`), "true", "the choice is remembered per checkout");
  assert.doesNotMatch(await page.locator("#job").textContent(), /0\/0|· ·/, "idle status never shows empty progress");

  // Desktop inspector: no placeholder column until something is selected.
  await page.setViewportSize({width:1440, height:900});
  assert.equal(await page.locator("#call-inspector").isVisible(), false, "no selection, no inspector column");
  assert.equal(await page.locator("#inspector-toggle").isVisible(), false);
  assert.match(await page.locator("footer details").textContent(), /Only measured source opens.*does not prove a path or runtime behavior/s, "standing caveats live in the evidence-limits footer");
  await page.locator('.directory-pick[title="src/"]').click();
  await page.locator("#call-inspector").waitFor({state:"visible"});
  await page.locator("#inspector-collapse").click();
  assert.equal(await page.locator("#call-inspector").isVisible(), false, "the inspector can be hidden without clearing the selection");
  assert.equal(await page.locator("#inspector-toggle").getAttribute("aria-expanded"), "false");
  assert.equal(await page.locator("#inspector-toggle").evaluate(node => document.activeElement === node), true);
  await page.keyboard.press("Enter");
  await page.locator("#call-inspector").waitFor({state:"visible"});
  assert.equal(await page.locator("#inspector-toggle").getAttribute("aria-expanded"), "true");
  await page.locator("#inspector-clear").click();
  await page.locator("#call-inspector").waitFor({state:"hidden"});
}));

test("Sequence explorer excludes fixtures and vendored inputs from heat while Files keeps them visible", {timeout:240000}, async () => published(async ({page, paths, server, pin}) => {
  const payload = await metrics(server, paths, pin, "max");
  const inputs = ["tests/fixtures/input.js", "tools/synthetic-cohorts/templates/input.js", "web/vendor/pele-1.10.0-trellis.2.js"];
  for (const path of inputs) {
    const row = payload.files.find(row => row.path === path); assert.ok(row, path);
    assert.equal(row.excludedReason, path.startsWith("web/") ? "vendored" : "fixture");
    assert.equal(row.declarationCount, 0); assert.equal(row.metrics.cognitive, null);
    assert.ok(row.roleCounts.production + row.roleCounts.test > 0);
  }
  assert.match(await page.locator("#metrics-state").textContent(), /callables excluded from heat.*2 fixture files, 1 vendored files/);
  await page.locator("#section-files").click();
  for (const path of inputs) {
    // Files uses indexed-catalog details, not the live explorer's directory buttons.
    await page.locator("#file-filter").fill(path);
    const pick = page.locator("#file-tree .file-pick").filter({hasText:path.split("/").at(-1)});
    await pick.waitFor(); assert.equal(await pick.count(), 1, path);
    assert.equal(await pick.getAttribute("title"), path);
    assert.equal(await pick.locator(".metric-swatch").count(), 0, "Files does not hide or heat excluded files");
  }
  await page.locator("#file-filter").fill("");
  await page.locator("#section-diagrams").click();
  await H.reveal(page, "#metric-include-tests"); await page.locator("#metric-include-tests").check();
  await page.waitForFunction(() => document.querySelector("#metrics-state")?.textContent.includes("all indexed callables"));
  const included = await metrics(server, paths, pin, "max", "revision", undefined, "all");
  for (const path of inputs) {
    const row = included.files.find(row => row.path === path);
    assert.equal(row.metrics.cognitive === null, path.startsWith("web/"), "Include tests restores fixtures, never vendor inputs");
  }
}));


test("invalid rules keep Explorer and Treemap heat with a warning while Architecture keeps its typed error", {timeout:240000}, async () => published(async ({page, paths, server, pin}) => {
  const warning = "Rules unavailable; layer-based heat exclusions were not applied.";
  const payload = await metrics(server, paths, pin, "max");
  assert.equal(payload.layerExclusionsWarning, warning);
  await H.reveal(page, "#metrics-state");
  assert.ok(await page.locator("#metrics-state").isVisible());
  assert.ok((await page.locator("#metrics-state").textContent()).includes(warning));
  await page.keyboard.press("Escape");
  const measured = payload.files.find(row => row.path === "src/flow.js");
  assert.ok(measured.metrics.cognitive > 0);
  const fixture = payload.files.find(row => row.path === "tests/fixtures/input.js");
  assert.equal(fixture.excludedReason, "fixture"); assert.equal(fixture.metrics.cognitive, null);
  const vendored = payload.files.find(row => row.path.startsWith("web/vendor/"));
  assert.equal(vendored.excludedReason, "vendored"); assert.equal(vendored.metrics.cognitive, null);
  await expandSources(page); await assertLegend(page, payload, "cognitive");
  assert.doesNotMatch(await page.locator('.file-pick[title="src/flow.js"] .metric-swatch').getAttribute("class"), /neutral/);

  await page.locator("#view-treemap").click();
  // #459: ready, and no render or resize relayout pending that could replace the
  // tile under the click below. The next frame observes any resize the switch caused.
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  await page.waitForFunction(() => document.querySelector("#treemap-state")?.dataset.state === "ready" &&
    document.querySelector("#treemap-diagram")?.dataset.layout === "settled");
  // #451: the ready summary sits behind the view's ⓘ; only the warning stays in view.
  assert.ok(await page.locator("#treemap-state").isVisible());
  assert.equal(await page.locator("#treemap-state").textContent(), warning);
  await H.reveal(page, "#treemap-summary");
  assert.ok(await page.locator("#treemap-summary").isVisible());
  assert.ok((await page.locator("#treemap-summary").textContent()).includes(warning));
  await page.keyboard.press("Escape");
  const tiles = page.locator("#treemap-diagram [data-trellis-file-id]");
  const key = JSON.stringify([measured.sourceSetId, measured.language, measured.path]);
  const keys = await tiles.evaluateAll(nodes => nodes.map(node => node.getAttribute("data-trellis-file-id")));
  assert.equal(keys.filter(value => value === key).length, 1);
  const tile = tiles.nth(keys.indexOf(key));
  assert.ok(await tile.locator("rect").first().isVisible());
  const band = [5, 15, 30].filter(cut => measured.metrics.cognitive > cut).length;
  assert.equal(await tile.locator("rect").first().getAttribute("fill"), ["#343644", "#FFC20A", "#FFAA10", "#FF7A00"][band]);
  await tile.click();
  assert.equal(await page.locator("#treemap-inspector-content dt").filter({hasText:/^cognitive$/}).evaluate(node => node.nextElementSibling.textContent), String(measured.metrics.cognitive));
  await page.locator("#treemap-path").fill("no-matching-files");
  await page.locator("#treemap-state[data-state=empty]").waitFor();
  assert.ok((await page.locator("#treemap-state").textContent()).includes(warning));

  const query = new URLSearchParams({...pin, workbench:"true",aggregate:"max",role:"production"});
  const response = await H.api(server.base, paths.token, "GET", H.scoped(server.rootKey, `architecture?${query}`));
  assert.equal(response.status, 422, JSON.stringify(response.data));
  assert.equal(response.data.error.code, "invalid_rules"); assert.equal(Object.hasOwn(response.data, "payload"), false);
  await page.locator("#view-architecture").click();
  await page.locator("#architecture-state[data-state=error]").waitFor();
  assert.match(await page.locator("#architecture-state").textContent(), /invalid_rules.*\.trellis\/rules\.toml/);
  assert.ok(!(await page.locator("#architecture-panel").textContent()).includes(warning));
  assert.equal(await page.locator("#architecture-diagram svg, #architecture-diagram [data-trellis-module-id]").count(), 0);
}, undefined, {rules:'version = 1\n[layers.broken]\norder = "not an integer"\npaths = ["src/**"]\n'}));


// Status responses are controlled at the existing API boundary; files, metrics
// and every displayed revision still come from the real published fixture.
async function chromeFixture(page, paths, server) {
  const observed = await H.api(server.base, paths.token, "GET", H.scoped(server.rootKey, "status"));
  assert.equal(observed.status, 200, JSON.stringify(observed.data));
  const fixture = {status:structuredClone(observed.data), job:null};
  const prefix = H.scoped(server.rootKey, "");
  const pattern = `${server.base}${prefix}**`;
  const respond = async route => {
    const suffix = new URL(route.request().url()).pathname.slice(prefix.length);
    let body;
    if (suffix === "status") body = fixture.status;
    else if (suffix === "jobs/current" || suffix.startsWith("jobs/")) body = fixture.job;
    else if (suffix === "index" && route.request().method() === "POST") body = fixture.job;
    else return route.continue();
    await route.fulfill({status:suffix === "index" ? 202 : 200, contentType:"application/json", body:JSON.stringify(body)});
  };
  await page.route(pattern, respond);
  fixture.select = async () => {
    await page.locator("#checkout-select").selectOption(server.rootKey);
    await page.waitForFunction(() => document.querySelector("#checkout-state")?.textContent === "");
  };
  fixture.close = () => page.unroute(pattern, respond);
  return fixture;
}
async function assertIndexState(page, state, words) {
  await page.waitForFunction(expected => document.querySelector("#workspace-index-state")?.dataset.state === expected, state);
  assert.match(await page.locator("#workspace-index-label").textContent(), words);
  const shape = await page.locator("#workspace-index-state .state-shape").boundingBox();
  assert.ok(shape && shape.width > 0 && shape.height > 0, `${state} has a visible shape as well as words`);
}

test("connected chrome is one header row: picker, status chip and a menu holding Refresh, Diagnostics and Disconnect", {timeout:240000}, async () => published(async ({page, requests}) => {
  const header = await page.locator(".app-header").boundingBox();
  const frame = await page.locator(".workspace-frame").boundingBox();
  assert.ok(header && frame);
  assert.equal(await page.locator(".workspace-toolbar").count(), 0, "no second action row");
  assert.ok(Math.abs(frame.y - (header.y + header.height)) <= 1, "the panes start right under the header");
  const headerControls = await Promise.all([".brand", ".section-nav", "#checkout-select", "#workspace-index-state", "#app-menu > summary"].map(selector => page.locator(selector).boundingBox()));
  assert.ok(headerControls.every(Boolean));
  assert.ok(Math.max(...headerControls.map(box => box.y)) < Math.min(...headerControls.map(box => box.y + box.height)), "desktop header controls share one row");
  assert.equal(await page.locator("#index").isVisible(), false, "Index workspace is hidden while the index is usable");
  for (const id of ["refresh", "logout"]) assert.equal(await page.locator(`#${id}`).isVisible(), false, `${id} waits in the closed menu`);
  for (const id of ["workspace-meta", "checkout-root", "status", "diagnostics", "job", "stale"]) {
    assert.equal(await page.locator(`#${id}`).evaluate(node => node.closest("#workspace-details") !== null), true, `${id} belongs to folded Diagnostics`);
    assert.equal(await page.locator(`#${id}`).isVisible(), false, `${id} consumes no closed-chrome height`);
  }
  assert.equal(await page.locator("#checkout-state").isVisible(), false, "successful selection adds no guidance row");
  assert.equal(await page.locator("#notice").isVisible(), false, "browse guidance is not a separate connected row");
  await page.locator("#app-menu > summary").click();
  assert.deepEqual(await page.locator("#app-menu .menu-body > :is(button, details)").evaluateAll(nodes => nodes.filter(node => !node.hidden).map(node => node.id)), ["refresh", "workspace-details", "logout"]);
  await page.locator("#workspace-details > summary").click();
  assert.equal(await page.locator("#checkout-root").isVisible(), true, "Diagnostics opens from the menu");
  await capture(page, "chrome-menu-diagnostics");
  await page.mouse.click(700, 500);
  assert.equal(await page.locator("#app-menu").evaluate(node => node.open), false, "an outside click closes the menu");
  for (const selector of ["#checkout-select", "#file-filter", "#refresh", "#metric-color", "#metric-aggregate"]) {
    await H.reveal(page, selector);
    const style = await page.locator(selector).evaluate(node => ({font:getComputedStyle(node).fontSize, radius:getComputedStyle(node).borderTopLeftRadius}));
    assert.deepEqual(style, {font:"14px", radius:"8px"}, `${selector} uses interface label and control tokens`);
  }
  await page.keyboard.press("Escape");
  const refreshed = page.waitForResponse(response => new URL(response.url()).pathname.endsWith("/status"));
  await H.reveal(page, "#refresh"); await page.locator("#refresh").click();
  assert.equal((await refreshed).status(), 200, "Refresh in the menu re-reads status");
  assert.equal(await page.locator("#app-menu").evaluate(node => node.open), false, "choosing a menu action closes the menu");
  await capture(page, "chrome-compact-desktop");
  for (const width of [390, 320]) {
    await page.setViewportSize({width, height:844});
    await noOverflow(page, width);
    for (const selector of ["#checkout-select", "#app-menu > summary", "#workspace-index-state"]) {
      const box = await page.locator(selector).boundingBox();
      assert.ok(box && box.x >= 0 && box.x + box.width <= width + 1, `${selector} remains reachable at ${width}px`);
    }
    await page.locator("#app-menu > summary").click();
    const menu = await page.locator("#app-menu .popover-body").boundingBox();
    assert.ok(menu && menu.x >= 0 && menu.x + menu.width <= width + 1, `the open menu fits ${width}px`);
    await page.keyboard.press("Escape");
    await page.locator("#explorer-toggle").click();
    assert.equal(await page.locator("#explorer-panel").isVisible(), true);
    await page.keyboard.press("Escape");
    assert.equal(await page.locator("#explorer-panel").isVisible(), false);
    await capture(page, `chrome-compact-${width}`);
  }
  assert.equal(requests.some(row => row.method === "POST" && row.url.pathname.endsWith("/index")), false, "layout and drawer actions never request indexing");
}));

test("view toolbars are one row with Options and an About popover; Sequence and Classes have no Color by", {timeout:240000}, async () => published(async ({page}) => {
  const rowOf = async panel => {
    const boxes = await page.locator(`${panel} .view-toolbar > :is(label, select, input, button, details > summary)`).evaluateAll(nodes => nodes.filter(node => node.getClientRects().length).map(node => node.getBoundingClientRect()).map(box => ({top:box.top, bottom:box.bottom})));
    assert.ok(boxes.length > 1, `${panel} has toolbar controls`);
    assert.ok(Math.max(...boxes.map(box => box.top)) < Math.min(...boxes.map(box => box.bottom)), `${panel} controls share one row at 1440px`);
  };
  // Every toolbar popover's full box stays inside the viewport and its visible panel.
  const inside = async (panel, popover, label) => {
    const body = page.locator(`${panel} ${popover} > .popover-body`);
    await body.waitFor({state:"visible"});
    // Placement runs on the popover's toggle event; wait for it, then check the whole box.
    await page.waitForFunction(selector => document.querySelector(selector)?.style.left !== "", `${panel} ${popover} > .popover-body`);
    const {box, frame, width, height} = await body.evaluate((node, selector) => ({box:node.getBoundingClientRect().toJSON(),
      frame:document.querySelector(selector).getBoundingClientRect().toJSON(), width:innerWidth, height:innerHeight}), panel);
    assert.ok(box.width > 0 && box.left >= Math.max(0, frame.left) - 1 && box.right <= Math.min(width, frame.right) + 1 &&
      box.top >= 0 && box.bottom <= height + 1, `${label}: ${JSON.stringify({box, frame, width})}`);
  };
  const popovers = [["architecture", "#architecture-panel", [".view-options", ".view-about"]], ["treemap", "#treemap-panel", [".view-options", ".view-about"]],
    ["classes", "#classes-panel", [".view-about"]], ["sequence", "#sequence-panel", [".view-options", ".view-about"]]];
  const checkAll = async state => {
    for (const [view, panel, kinds] of popovers) {
      await page.locator(`#view-${view}`).click();
      for (const kind of kinds) {
        await page.locator(`${panel} ${kind} > summary`).click();
        await inside(panel, kind, `${view} ${kind} ${state}`);
        await page.keyboard.press("Escape");
      }
    }
  };
  for (const [view, panel] of [["architecture", "#architecture-panel"], ["treemap", "#treemap-panel"], ["classes", "#classes-panel"]]) {
    await page.locator(`#view-${view}`).click();
    await rowOf(panel);
    const about = page.locator(`${panel} .view-about > summary`);
    assert.equal(await about.getAttribute("aria-label"), "About this view");
    await about.click();
    assert.equal(await page.locator(`${panel} .view-about h3`).textContent(), "About this view");
    await capture(page, `toolbar-${view}-about`);
    await page.keyboard.press("Escape");
  }
  await checkAll("at 1440px");
  await page.locator('.directory-pick[title="src/"]').click();
  await page.locator("#call-inspector").waitFor({state:"visible"});
  await checkAll("with the inspector open");
  await capture(page, "toolbar-popover-inspector-open");
  await page.locator("#inspector-clear").click();
  for (const width of [900, 390]) {
    await page.setViewportSize({width, height:844});
    await checkAll(`at ${width}px`);
  }
  await page.setViewportSize({width:1440, height:900});
  await page.locator("#view-architecture").click();
  for (const id of ["architecture-metric-tests", "architecture-relative", "architecture-tests"]) {
    assert.equal(await page.locator(`#${id}`).isVisible(), false, `${id} waits in Options`);
  }
  await page.locator("#architecture-panel .view-options > summary").click();
  for (const id of ["architecture-metric-tests", "architecture-relative", "architecture-tests"]) assert.equal(await page.locator(`#${id}`).isVisible(), true);
  await capture(page, "toolbar-architecture-options");
  await page.keyboard.press("Escape");
  assert.equal(await page.locator(".architecture-basis").first().isVisible(), false, "caveats are behind the ⓘ");
  await page.locator("#view-treemap").click();
  assert.equal(await page.locator(".treemap-basis").isVisible(), false);
  for (const view of ["sequence", "classes"]) {
    await page.locator(`#view-${view}`).click();
    assert.equal(await page.locator(`#${view}-panel`).getByText("Color by", {exact:true}).count(), 0, `${view} has nothing to colour`);
    assert.equal(await page.locator("#metric-color").isVisible(), false);
  }
  assert.equal(await page.locator("#classes-unmatched").count(), 0, "no disabled placeholder option");
  assert.equal(await page.locator(".sequence-context").count(), 0, "the Sequence caveat row moved behind the ⓘ");
  await page.locator("#view-sequence").click();
  assert.equal(await page.locator("#all-steps").isVisible(), false);
  await page.locator("#sequence-panel .view-options > summary").click();
  assert.equal(await page.locator("#all-steps").isVisible(), true);
}));

test("the rail is just the tree: its filter searches unopened folders through the pinned catalog", {timeout:240000}, async () => published(async ({page, server, pin, requests}) => {
  for (const gone of ["#indexed-checkouts", "#browse-scope", ".file-filter-label", "#explorer-metric-legend"]) assert.equal(await page.locator(gone).count(), 0, `${gone} is gone`);
  assert.equal(await page.locator("#browse-root").isVisible(), false, "the absolute path is not repeated");
  assert.doesNotMatch(await page.locator("#explorer-panel").innerText(), /trellis mcp|Expand folders to browse/);
  const filter = page.locator("#file-filter");
  assert.equal(await filter.getAttribute("aria-label"), "Filter files");
  const height = (await filter.boundingBox()).height;
  assert.ok(height <= 40, `the filter is one line (${height}px)`);
  assert.equal(await page.locator('.directory-pick[title="src/nested/"]').count(), 0, "src/nested/ has not been opened");
  const search = page.waitForRequest(request => new URL(request.url()).pathname === H.scoped(server.rootKey, "files") && new URL(request.url()).searchParams.get("q") === "other.js");
  await filter.fill("other.js");
  const url = new URL((await search).url());
  assert.equal(url.searchParams.get("indexGeneration"), pin.indexGeneration);
  assert.equal(url.searchParams.get("indexRevision"), String(pin.indexRevision));
  const match = page.locator('#file-tree .file-pick[title="src/nested/other.js"]');
  await match.waitFor();
  assert.match(await page.locator("#files-state").textContent(), /^1 matching indexed file$/);
  assert.equal(await page.locator("#file-tree .file-pick").count(), 1, "only matches are listed");
  await match.click();
  await page.locator(".method-pick").filter({hasText:"moderate"}).waitFor();
  await capture(page, "rail-filter-unopened-folder");
  // Reveal from the filter opens the never-opened ancestors in the live tree.
  await page.locator(".method-pick").filter({hasText:"moderate"}).click();
  await page.waitForFunction(() => document.querySelector("#sequence-state")?.textContent.includes("moderate"));
  await H.reveal(page, "#reveal-method"); await page.locator("#reveal-method").click();
  await page.locator('.directory-pick[title="src/nested/"][aria-expanded="true"]').waitFor();
  assert.equal(await filter.inputValue(), "", "reveal clears the filter");
  assert.equal(await page.locator('.directory-pick[title="src/"]').getAttribute("aria-expanded"), "true");
  const revealed = page.locator(".method-pick.selected");
  await revealed.waitFor();
  assert.match(await revealed.textContent(), /moderate/);
  assert.equal(await revealed.evaluate(node => document.activeElement === node), true, "focus moves to the revealed method");
  await capture(page, "rail-reveal-from-filter");
  await filter.fill("no-such-path");
  await page.locator("#file-tree").getByText("No indexed files match.").waitFor();
  await filter.fill("");
  await page.locator('.directory-pick[title="src/"]').waitFor();
  assert.equal(requests.filter(row => row.url.searchParams.has("q")).every(row => row.method === "GET"), true);
}));

test("selected status reconciles jobs, catch-up and stale evidence while raw errors stay in Diagnostics and Stop waiting never cancels", {timeout:240000}, async () => published(async ({page, paths, server, pin, requests}) => {
  const fixture = await chromeFixture(page, paths, server);
  const failure = '<img src=x onerror=globalThis.statusExecuted=true> index fixture failure';
  const job = {id:"chrome-fixture-job", state:"failed", progress:{phase:"index", completed:0, total:0},
    error:{code:"index_failed", message:failure}, finishedAt:1, revision:null};
  try {
    fixture.status.catchingUp = false;
    fixture.status.stats.changedFiles = [];
    fixture.job = job;
    await fixture.select();
    // #447 item 9: revision N is ready and fresh, so the chip says so; the failed re-index is a note with its cause.
    await assertIndexState(page, "fresh", /^Fresh · rev /);
    const note = page.locator("#workspace-index-note");
    assert.equal(await note.isVisible(), true);
    assert.equal(await note.textContent(), "Last re-index failed · index_failed");
    assert.match(await page.locator("#workspace-index-announce").textContent(), /Fresh.*Last re-index failed · index_failed/);
    await capture(page, "chrome-failed-reindex-note");
    assert.equal(await page.locator("#job").isVisible(), false);
    assert.equal(await page.locator("body").evaluate((node, text) => node.innerText.includes(text), failure), false, "raw error text is not exposed in the compact chrome");
    await H.reveal(page, "#workspace-details > summary"); await page.locator("#workspace-details > summary").click();
    assert.ok((await page.locator("#job").textContent()).includes(failure));
    assert.match(await page.locator("#job").textContent(), /index_failed/);
    assert.equal(await page.locator("#workspace-details img").count(), 0, "error objects render only as inert text");
    assert.equal(await page.evaluate(() => globalThis.statusExecuted), undefined);
    await capture(page, "chrome-failed-diagnostics");
    await H.reveal(page, "#workspace-details > summary"); await page.locator("#workspace-details > summary").click();

    fixture.job = {...job, state:"done", error:null, revision:pin};
    fixture.status.catchingUp = true;
    await fixture.select();
    await assertIndexState(page, "catching-up", /Catching up/i);
    assert.doesNotMatch(await page.locator("#workspace-index-label").textContent(), /Fresh|0\/0/);
    fixture.status.catchingUp = false;
    fixture.status.stats.changedFiles = ["src/flow.js"];
    await fixture.select();
    await assertIndexState(page, "stale", /Stale evidence/i);
    await H.reveal(page, "#workspace-details > summary"); await page.locator("#workspace-details > summary").click();
    assert.match(await page.locator("#stale").textContent(), /1 inputs differ/);
    await H.reveal(page, "#workspace-details > summary"); await page.locator("#workspace-details > summary").click();
    fixture.status.stats.changedFiles = [];
    await fixture.select();
    await assertIndexState(page, "fresh", /Fresh/i);
    assert.doesNotMatch(await page.locator("#job").textContent(), /0\/0/);
    assert.equal(requests.some(row => row.method === "POST" && row.url.pathname.endsWith("/index")), false, "selecting or refreshing status never indexes");

    assert.equal(await page.locator("#index").isVisible(), false, "a usable index offers no Index workspace button");
    // Without a usable index the button appears and the tabs grey out.
    const readyPin = fixture.status.revision;
    fixture.status.revision = null;
    await fixture.select();
    await page.locator("#index").waitFor({state:"visible"});
    assert.ok((await page.locator("#index").boundingBox()).height >= 44, "Index workspace is a primary action of at least 44px");
    for (const view of ["architecture", "treemap", "sequence", "classes"]) assert.equal(await page.locator(`#view-${view}`).isDisabled(), true, `${view} is greyed out`);
    assert.equal(await page.locator("#section-files").isDisabled(), true);
    await capture(page, "chrome-no-index");
    fixture.job = {...job, state:"running", error:null, finishedAt:null, progress:{phase:"capture", completed:0, total:0}};
    const accepted = page.waitForResponse(response => new URL(response.url()).pathname === H.scoped(server.rootKey, "index"));
    await page.locator("#index").click();
    assert.equal((await accepted).status(), 202);
    await assertIndexState(page, "catching-up", /Indexing.*capture/i);
    assert.doesNotMatch(await page.locator("#workspace-index-label").textContent(), /0\/0/);
    assert.equal(await page.locator("#cancel").isVisible(), true);
    await page.locator("#cancel").click();
    assert.equal(await page.locator("#cancel").isVisible(), false);
    assert.match(await page.locator("#workspace-index-label").textContent(), /no longer waiting/i);
    assert.equal(await page.locator("#index").isEnabled(), true);
    assert.equal(requests.filter(row => row.method === "POST" && row.url.pathname.endsWith("/index")).length, 1, "only the explicit Index button requests a job");
    assert.equal(requests.some(row => row.url.pathname.endsWith("/cancel")), false, "Stop waiting has no durable job cancellation request");
    // Reloading selection re-observes that same accepted job, not a fabricated cancellation.
    await fixture.select();
    assert.equal(await page.locator("#cancel").isVisible(), true);
    await assertIndexState(page, "catching-up", /Indexing/i);
    await page.locator("#cancel").click();
    fixture.status.revision = readyPin; fixture.job = null;
    await fixture.select();
    assert.equal(await page.locator("#view-sequence").isDisabled(), false, "tabs return with a usable index");
  } finally { await fixture.close(); }
}));

test("checkout picker uses observed branch, detached and unavailable Git labels, keeps exact roots discoverable and treats malicious branch text as text", {timeout:240000}, async () => published(async ({page, paths, server, pin, requests}) => {
  const listing = await H.api(server.base, paths.token, "GET", "/api/checkouts");
  assert.equal(listing.status, 200, JSON.stringify(listing.data));
  const observed = listing.data.checkouts.find(row => row.rootKey === server.rootKey);
  const gitResponse = await H.api(server.base, paths.token, "GET", H.scoped(server.rootKey, "git"));
  assert.equal(gitResponse.status, 200, JSON.stringify(gitResponse.data));
  assert.equal(gitResponse.data.git.state, "available");
  assert.equal(gitResponse.data.git.head.name, "measured-branch");
  assert.deepEqual(gitResponse.data.git.workingTree, {state:"available", changedFiles:0});
  await page.waitForFunction(() => document.querySelector("#workspace-git")?.textContent === "measured-branch · No upstream · clean");
  const shortRevision = `rev ${pin.indexGeneration.slice(0, 8)}:${pin.indexRevision}`;
  assert.equal(await page.locator(`#checkout-select option[value="${server.rootKey}"]`).textContent(), `workspace ⎇ measured-branch · ${shortRevision}`, "the real measured Git branch labels the published checkout");
  const branch = '<svg onload="globalThis.branchExecuted=true">' + "-long-branch".repeat(12);
  const upstream = '<img src=x onerror="globalThis.upstreamExecuted=true">';
  const available = {...gitResponse.data.git, head:{...gitResponse.data.git.head, name:branch},
    upstream:{name:upstream, ahead:2, behind:3, basis:"last_fetch"}, workingTree:{state:"available", changedFiles:9}};
  const rows = [
    {...observed, git:available},
    {...observed, rootKey:"b".repeat(64), workspaceRoot:join(paths.workspace, "busy"), state:"storage_busy", git:{state:"unavailable", reason:"not_repository"}},
    {...observed, rootKey:"c".repeat(64), workspaceRoot:join(paths.workspace, "unavailable"), state:"unavailable", git:{state:"unavailable", reason:"not_repository"}},
  ];
  const pattern = `${server.base}/api/checkouts`;
  const respond = route => route.fulfill({status:200, contentType:"application/json", body:JSON.stringify({...listing.data, checkouts:rows})});
  const gitPattern = `${server.base}${H.scoped(server.rootKey, "git")}`;
  const respondGit = route => route.fulfill({status:200, contentType:"application/json", body:JSON.stringify({...gitResponse.data, git:rows[0].git})});
  await page.route(pattern, respond);
  await page.route(gitPattern, respondGit);
  try {
    await H.reveal(page, "#logout"); await page.locator("#logout").click();
    await page.locator("#token").fill(paths.token);
    await page.locator("#connect-form button[type=submit]").click();
    await page.waitForFunction(key => document.querySelector("#checkout-select").value === key, server.rootKey);
    assert.equal(await page.locator("#checkout-select").inputValue(), server.rootKey, "the only selectable checkout opens on arrival; busy and unavailable rows do not count");
    const option = page.locator(`#checkout-select option[value="${server.rootKey}"]`);
    assert.equal(await option.textContent(), `workspace ⎇ ${branch} · ${shortRevision}`);
    assert.equal(await option.getAttribute("title"), observed.workspaceRoot);
    for (const row of rows.slice(1)) assert.equal(await page.locator(`#checkout-select option[value="${row.rootKey}"]`).isDisabled(), true, `${row.state} entries cannot be selected`);
    assert.equal(await page.locator("#checkout-retry").isVisible(), true);
    await page.locator("#checkout-select").selectOption(server.rootKey);
    await page.waitForFunction(() => document.querySelector("#checkout-state")?.textContent === "");
    assert.equal(await page.locator("#checkout-select").getAttribute("title"), observed.workspaceRoot);
    assert.equal(await page.locator("#workspace-name").getAttribute("title"), observed.workspaceRoot);
    const hostileGit = `${branch} · 9 uncommitted · ${upstream} ↑2 ↓3 · As of last fetch`;
    await page.waitForFunction(text => document.querySelector("#workspace-git")?.textContent === text, hostileGit);
    assert.equal(await page.locator(".app-header svg, #workspace-git img").count(), 0);
    assert.equal(await page.evaluate(() => globalThis.branchExecuted || globalThis.upstreamExecuted), undefined);
    for (const width of [390, 320]) {
      await page.setViewportSize({width, height:844});
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true, `${width}px long branch does not create document overflow`);
      const picker = await page.locator("#checkout-select").boundingBox();
      assert.ok(picker && picker.x >= 0 && picker.x + picker.width <= width + 1);
    }
    await page.setViewportSize({width:1440, height:900});
    assert.equal(await page.locator("#checkout-root").isVisible(), false);
    await H.reveal(page, "#workspace-details > summary"); await page.locator("#workspace-details > summary").click();
    assert.equal(await page.locator("#checkout-root").textContent(), observed.workspaceRoot);
    await H.reveal(page, "#workspace-details > summary"); await page.locator("#workspace-details > summary").click();
    for (const [git, identity, words] of [
      [{...available, upstream:null, workingTree:{state:"unavailable", reason:"deadline_exceeded"}}, branch, /No upstream · Changes unavailable/],
      [{...available, upstream:null, head:{...available.head, kind:"detached", name:null}}, `Detached ${available.head.shortCommit}`, /Detached HEAD .*No upstream.*9 uncommitted/],
      [{...available, upstream:null, head:{kind:"detached", name:null, shortCommit:null}}, "Detached", /Detached HEAD · No upstream · 9 uncommitted/],
      [{state:"unavailable", reason:"not_repository"}, "Git unavailable", /^Git unavailable$/],
      [{state:"unavailable", reason:"conflicted"}, "Git unavailable", /^Git unavailable$/],
      [{state:"unavailable", reason:"tracking_unavailable"}, "Git unavailable", /^Git unavailable$/],
    ]) {
      rows[0].git = git;
      const refreshed = page.waitForResponse(response => response.url() === pattern);
      await page.locator("#checkout-retry").click();
      assert.equal((await refreshed).status(), 200);
      await page.waitForFunction(({key, label}) => [...document.querySelector("#checkout-select").options].find(option => option.value === key)?.textContent.includes(label), {key:server.rootKey, label:identity});
      assert.equal(await option.textContent(), `workspace ⎇ ${identity} · ${shortRevision}`);
      const shown = await page.locator("#workspace-git").textContent();
      assert.match(shown, words);
      if (git.head?.kind === "detached") assert.doesNotMatch(shown, /Git unavailable/, "a detached HEAD is still Git");
      if (git.state === "unavailable") assert.doesNotMatch(shown, /uncommitted|clean|↑|↓/);
      assert.equal(await page.locator("#checkout-select").inputValue(), server.rootKey, "retry retains explicit selection of the same available checkout");
    }
    // Git remains independent of a missing semantic index; no fallback zero is invented.
    rows[0].git = {...available, upstream:null, workingTree:{state:"available", changedFiles:4}};
    const statusPattern = `${server.base}${H.scoped(server.rootKey, "status")}`;
    const coldStatus = route => route.fulfill({status:409, contentType:"application/json", body:JSON.stringify({error:{code:"index_not_ready", message:"No validated index", workspaceRoot:observed.workspaceRoot}})});
    await page.route(statusPattern, coldStatus);
    try {
      await page.locator("#checkout-select").selectOption(server.rootKey);
      await page.waitForFunction(() => document.querySelector("#checkout-state")?.textContent.includes("Index not ready"));
      await page.waitForFunction(() => document.querySelector("#workspace-git")?.textContent.includes("4 uncommitted"));
      assert.ok((await page.locator("#workspace-git").textContent()).startsWith(branch));
    } finally { await page.unroute(statusPattern, coldStatus); }
    assert.equal(requests.some(row => row.method === "POST" && row.url.pathname.endsWith("/index")), false);
    await capture(page, "chrome-checkout-git-unavailable");
  } finally {
    await page.unroute(pattern, respond);
    await page.unroute(gitPattern, respondGit);
  }
}, undefined, {git:true}));

test("a failed job lookup reads as unavailable until Refresh retries it, and the live region skips progress-only polls", {timeout:240000}, async () => published(async ({page, paths, server, requests}) => {
  const fixture = await chromeFixture(page, paths, server);
  const prefix = H.scoped(server.rootKey, "");
  const jobsPattern = `${server.base}${prefix}jobs/current`;
  const failure = '<img src=x onerror=globalThis.jobExecuted=true> job lookup failed';
  const failJobs = route => route.fulfill({status:500, contentType:"application/json", body:JSON.stringify({error:{code:"storage_io", message:failure}})});
  try {
    fixture.status.catchingUp = false;
    fixture.status.stats.changedFiles = [];
    assert.equal(await page.locator("#workspace-index-state").getAttribute("aria-live"), null, "the visible label is not itself a live region");
    assert.equal(await page.locator("#workspace-index-announce").getAttribute("aria-live"), "polite");
    await page.route(jobsPattern, failJobs);
    await page.locator("#checkout-select").selectOption(server.rootKey);
    await assertIndexState(page, "failed", /^Index job unavailable · retry status$/);
    assert.doesNotMatch(await page.locator("#workspace-index-label").textContent(), /Stale|Fresh/);
    assert.equal(await page.locator("#workspace-index-announce").textContent(), "Index job unavailable · retry status");
    assert.ok((await page.locator("#checkout-state").textContent()).includes(failure), "the lookup error is shown as text");
    assert.equal(await page.locator("#checkout-state img, #workspace-index-state img, #stale img").count(), 0);
    assert.equal(await page.evaluate(() => globalThis.jobExecuted), undefined);
    await capture(page, "chrome-job-lookup-unavailable");

    await page.unroute(jobsPattern, failJobs);
    const retried = page.waitForResponse(response => new URL(response.url()).pathname === `${prefix}jobs/current`);
    await H.reveal(page, "#refresh"); await page.locator("#refresh").click();
    assert.equal((await retried).status(), 200, "Refresh retries the failed job lookup");
    await assertIndexState(page, "fresh", /^Fresh/);
    assert.equal(await page.locator("#checkout-state").isVisible(), false);

    fixture.status.revision = null;
    await H.reveal(page, "#refresh"); await page.locator("#refresh").click();
    await page.locator("#index").waitFor({state:"visible"});
    fixture.job = {id:"live-job", state:"running", error:null, finishedAt:null, revision:null, progress:{phase:"capture", completed:1, total:10}};
    await page.locator("#index").click();
    await page.waitForFunction(() => document.querySelector("#workspace-index-label")?.textContent === "Indexing · capture · 1/10");
    assert.equal(await page.locator("#workspace-index-announce").textContent(), "Indexing · capture · 1/10");
    fixture.job = {...fixture.job, progress:{phase:"capture", completed:6, total:10}};
    await page.waitForFunction(() => document.querySelector("#workspace-index-label")?.textContent === "Indexing · capture · 6/10");
    assert.equal(await page.locator("#workspace-index-announce").textContent(), "Indexing · capture · 1/10", "a progress-only poll is not announced");
    fixture.job = {...fixture.job, state:"failed", error:{code:"index_failed", message:"fixture"}, finishedAt:1};
    await page.waitForFunction(() => document.querySelector("#workspace-index-announce")?.textContent === "Index failed · index_failed");
    assert.equal(requests.filter(row => row.method === "POST" && row.url.pathname.endsWith("/index")).length, 1, "only the explicit Index click requests a job");
  } finally {
    await page.unroute(jobsPattern, failJobs).catch(() => {});
    await fixture.close();
  }
}));

test("an unsaved token warning stays visible in the header after a checkout is selected", {timeout:240000}, async () => published(async ({page, paths, server}) => {
  const warning = "Browser storage is unavailable; token was not saved.";
  await H.reveal(page, "#logout"); await page.locator("#logout").click();
  await page.evaluate(() => { Storage.prototype.setItem = () => { throw new DOMException("blocked", "SecurityError"); }; });
  await page.locator("#token").fill(paths.token);
  await page.locator("#remember-token").check();
  await page.locator("#connect-form button[type=submit]").click();
  await page.waitForFunction(() => !document.querySelector("#checkout-select").disabled);
  assert.ok((await page.locator("#notice").textContent()).includes(warning), "the warning is reported before selection");
  await page.locator("#checkout-select").selectOption(server.rootKey);
  await page.locator("#workspace").waitFor({state:"visible"});
  await page.waitForFunction(text => document.querySelector("#checkout-state")?.textContent === text, warning);
  assert.equal(await page.locator("#notice").isVisible(), false, "the connected browse notice row stays folded");
  assert.equal(await page.locator("#checkout-state").isVisible(), true, "the warning remains visible once a checkout is selected");
  const box = await page.locator("#checkout-state").boundingBox();
  assert.ok(box && box.width > 0 && box.height > 0);
  await capture(page, "chrome-storage-warning");
}));

test("late Git from an old root and an older checkout list cannot regress selected Git", {timeout:240000}, async () => published(async ({page, paths, server, requests}) => {
  const discovery = await H.api(server.base, paths.token, "GET", "/api/checkouts");
  const observed = discovery.data.checkouts.find(row => row.rootKey === server.rootKey);
  const oldStatus = await H.api(server.base, paths.token, "GET", H.scoped(server.rootKey, "git"));
  const otherKey = "d".repeat(64);
  const summary = name => ({...oldStatus.data.git, head:{...oldStatus.data.git.head, name}, upstream:null, workingTree:{state:"available", changedFiles:1}});
  const current = {git:summary("current-root"), changes:[{path:"src/flow.js", status:"modified"}]};
  const old = {git:summary("obsolete-root"), changes:[{path:maliciousName, status:"added"}]};
  const rows = [{...observed, git:old.git}, {...observed, rootKey:otherKey, git:current.git},
    {...observed, rootKey:"e".repeat(64), state:"unavailable", git:{state:"unavailable", reason:"not_repository"}}];
  const listPattern = `${server.base}/api/checkouts`;
  const respondList = route => route.fulfill({status:200, contentType:"application/json", body:JSON.stringify({...discovery.data, checkouts:rows})});
  const rootPattern = `${server.base}${H.scoped(server.rootKey, "git")}`;
  const respondOld = route => route.fulfill({status:200, contentType:"application/json", body:JSON.stringify(old)});
  const otherPattern = `${server.base}${H.scoped(otherKey, "")}**`;
  const respondOther = async route => {
    const url = route.request().url();
    if (new URL(url).pathname.endsWith("/git")) return route.fulfill({status:200, contentType:"application/json", body:JSON.stringify(current)});
    await route.continue({url:url.replace(`/checkouts/${otherKey}/`, `/checkouts/${server.rootKey}/`)});
  };
  await page.route(listPattern, respondList);
  await page.route(rootPattern, respondOld);
  await page.route(otherPattern, respondOther);
  await page.evaluate(() => {
    const original = window.fetch;
    window.__gitOrdering = {original, held:null, entered:false, consumed:false};
    window.fetch = async (input, options) => {
      const state = window.__gitOrdering, path = new URL(input, location.href).pathname;
      const hold = state.held && path === state.held;
      // Let the old response reach the existing epoch/serial guards even after abort.
      const response = await original(input, hold ? {...options, signal:undefined} : options);
      if (!hold) return response;
      const json = response.json.bind(response);
      response.json = async () => {
        const value = await json(); state.entered = true;
        await new Promise(resolve => { state.release = resolve; });
        state.consumed = true; return value;
      };
      return response;
    };
  });
  const hold = path => page.evaluate(path => {
    Object.assign(window.__gitOrdering, {held:path, entered:false, consumed:false});
  }, path);
  const release = async () => {
    await page.evaluate(() => { window.__gitOrdering.held = null; window.__gitOrdering.release(); });
    await page.waitForFunction(() => window.__gitOrdering.consumed);
    await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  };
  const assertCurrent = async name => {
    const expected = `${name} · No upstream · 1 uncommitted`;
    await page.waitForFunction(text => document.querySelector("#workspace-git")?.textContent === text, expected);
    assert.ok((await page.locator(`#checkout-select option[value="${otherKey}"]`).textContent()).includes(name));
    assert.equal(await page.locator('.file-pick[title="src/flow.js"] .git-marker[data-status="modified"]').count(), 1);
    assert.equal(await page.locator("#file-tree .git-marker").evaluateAll((nodes, path) => nodes.filter(node => node.dataset.path === path).length, maliciousName), 0);
  };
  try {
    // Refresh discovery once to admit the second explicit checkout.
    await H.reveal(page, "#logout"); await page.locator("#logout").click();
    await page.evaluate(() => localStorage.removeItem("trellis.lastCheckout.v1"));
    await page.locator("#token").fill(paths.token);
    await page.locator("#connect-form button[type=submit]").click();
    await page.waitForFunction(() => !document.querySelector("#checkout-select").disabled);
    await hold(H.scoped(server.rootKey, "git"));
    await page.locator("#checkout-select").selectOption(server.rootKey);
    await page.waitForFunction(() => window.__gitOrdering.entered);
    await page.locator("#checkout-select").selectOption(otherKey);
    await page.locator('.directory-pick[title="src/"]').click();
    await page.locator('.file-pick[title="src/flow.js"] .git-marker').waitFor();
    await assertCurrent("current-root");
    await release();
    await assertCurrent("current-root");
    // The list captures old-root summaries first; a newer selected read then wins.
    await hold("/api/checkouts");
    await page.locator("#checkout-retry").click();
    await page.waitForFunction(() => window.__gitOrdering.entered);
    current.git = summary("newer-selected");
    const refreshed = page.waitForResponse(response => new URL(response.url()).pathname === H.scoped(otherKey, "git"));
    await H.reveal(page, "#refresh"); await page.locator("#refresh").click();
    assert.equal((await refreshed).status(), 200);
    await page.waitForFunction(() => document.querySelector("#workspace-git")?.textContent.startsWith("newer-selected"));
    await release();
    await assertCurrent("newer-selected");
    current.git = {state:"unavailable", reason:"git_changed"}; current.changes = null;
    await H.reveal(page, "#refresh"); await page.locator("#refresh").click();
    await page.waitForFunction(() => document.querySelector("#workspace-git")?.textContent === "Git unavailable");
    assert.equal(await page.locator("#file-tree .git-marker").count(), 0, "unavailable observation clears previously measured paths");
    await page.waitForFunction(() => !document.querySelector("#refresh").disabled);
    current.git = {...summary("newer-selected"), workingTree:{state:"available", changedFiles:0}}; current.changes = [];
    await H.reveal(page, "#refresh"); await page.locator("#refresh").click();
    await page.waitForFunction(() => document.querySelector("#workspace-git")?.textContent === "newer-selected · No upstream · clean");
    assert.equal(await page.locator("#file-tree .git-marker").count(), 0, "measured clean never retains dirty path markers");
    assert.equal(requests.some(row => row.method === "POST" && row.url.pathname.endsWith("/index")), false);
  } finally {
    await page.evaluate(() => { const state = window.__gitOrdering; state.held = null; state.release?.(); window.fetch = state.original; });
    await page.unroute(listPattern, respondList); await page.unroute(rootPattern, respondOld); await page.unroute(otherPattern, respondOther);
  }
}, undefined, {git:true, additionalRootKeys:["d".repeat(64)]}));
