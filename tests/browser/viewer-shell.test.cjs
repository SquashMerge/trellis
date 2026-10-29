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

async function published(callback, signal, {library = false, rules = null, git = false} = {}) {
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
  if (library) {
    mkdirSync(join(paths.workspace, "vendor", "local-library", "src"), {recursive:true});
    writeFileSync(join(paths.workspace, "Cargo.toml"), "[package]\nname='browser-app'\nversion='0.1.0'\n[dependencies]\nlocal-library={path='vendor/local-library'}\n");
    writeFileSync(join(paths.workspace, "Cargo.lock"), "version=4\n[[package]]\nname='browser-app'\nversion='0.1.0'\ndependencies=['local-library']\n[[package]]\nname='local-library'\nversion='0.1.0'\n");
    writeFileSync(join(paths.workspace, "vendor", "local-library", "Cargo.toml"), "[package]\nname='local-library'\nversion='0.1.0'\n");
    writeFileSync(join(paths.workspace, "vendor", "local-library", "src", "lib.rs"), "// <img src=x onerror='globalThis.libraryExecuted=true'>\npub fn candidate_only() { unknown_external(); }\n");
  }
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
    const pin = JSON.parse(indexed.stdout).status.revision;
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
    await page.waitForFunction(() => !document.querySelector("#checkout-select").disabled);
    assert.equal(await page.locator("#checkout-select").inputValue(), "", "one checkout still requires explicit selection");
    assert.equal(await page.locator("#workspace").isVisible(), false);
    assert.ok(requests.every(row => ["/api/checkouts", "/api/daemon/status"].includes(row.url.pathname)), "Connect loads discovery only");
    await page.locator("#checkout-select").selectOption(server.rootKey);
    await page.locator("#workspace").waitFor({state:"visible"});
    await page.waitForFunction(() => document.querySelector("#metrics-state")?.textContent.includes("Indexed snapshot · max"));
    await page.locator('.directory-pick[title="src/"]').waitFor();
    const reloadedPin = await callback({page, paths, server, pin, requests});
    const observedPins = reloadedPin ? [pin, reloadedPin] : [pin];
    assert.deepEqual(errors, []);
    const metricsRequests = requests.filter(row => row.url.pathname.endsWith("/metrics"));
    assert.ok(metricsRequests.length > 0);
    for (const {url, method} of metricsRequests) {
      assert.equal(method, "GET"); assert.equal(url.searchParams.get("workbench"), "true");
      assert.equal(url.pathname, H.scoped(server.rootKey, "metrics"));
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
  }
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
  assert.match(await page.locator("#workspace-index-label").textContent(), new RegExp(`rev(?:ision)? ${pin.indexGeneration.slice(0, 8)}:${pin.indexRevision}`));
  await expandSources(page);
  let payload = await metrics(server, paths, pin, "max");
  await assertLegend(page, payload, "cognitive");
  assert.equal(await page.locator("#view-sequence").getAttribute("aria-selected"), "true", "Sequence shares the explorer heat surface");
  assert.equal(await page.locator("#metric-relative").isChecked(), false);
  const heatBefore = await page.locator('.file-pick[title="src/flow.js"] .metric-swatch').getAttribute("class");
  await page.locator("#file-filter").fill("src/flow.js");
  assert.equal(await page.locator('.file-pick[title="src/flow.js"] .metric-swatch').getAttribute("class"), heatBefore, "same file keeps fixed color when explorer filtering changes");
  await page.locator("#file-filter").fill("");
  const readsBefore = requests.filter(row => row.url.pathname.endsWith("/metrics")).length;
  await page.locator("#metric-relative").check(); await assertLegend(page, payload, "cognitive");
  assert.match(await page.locator("#metric-legend").textContent(), /Relative to visible set/);
  assert.equal(requests.filter(row => row.url.pathname.endsWith("/metrics")).length, readsBefore);
  await page.locator("#metric-relative").uncheck(); await assertLegend(page, payload, "cognitive");
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
      await page.locator("#metric-include-tests").check();
      await page.waitForFunction(() => document.querySelector("#metrics-state")?.textContent.includes("all indexed callables"));
    }
    for (const aggregate of ["max", "sum", "mean"]) {
      await page.locator("#metric-aggregate").selectOption(aggregate);
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
    assert.equal(await page.locator("#metric-include-tests").isVisible(), true);
    await capture(page, `viewer-${role}-mobile`);
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
    await page.locator("#metric-include-tests").check();
    await page.waitForFunction(() => document.querySelector("#metrics-state")?.textContent.includes("all indexed callables"));
    await page.locator('.directory-pick[title="roles/"]').click();
    await page.locator('.file-pick[title="roles/mixed.rs"]').click();
    await page.waitForFunction(() => document.querySelector("#metric-inspector-values dt"));
    await page.route(pattern, respond);
    await page.locator("#metric-include-tests").uncheck();
    await seen;
    assert.equal(await page.locator("#metric-inspector-values dt").count(), 0, "changing role clears inspection before a response exists");
    assert.equal(await page.locator("#file-tree .heat-0, #file-tree .heat-1, #file-tree .heat-2, #file-tree .heat-3").count(), 0, "old population heat is cleared while production is blocked");
    await page.locator("#metric-include-tests").check();
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



test("Sequence URLs wait for authentication and checkout, use the returned seed, and refuse invalid context", {timeout:240000}, async () => published(async ({page, paths, server, pin, requests}) => {
  const seed = await H.seedId(server, paths.token, "transform");
  const sequenceRequests = () => requests.filter(row => row.url.pathname.endsWith("/sequence"));
  const pinned = new URLSearchParams({sid:seed, rootKey:server.rootKey, ...pin});
  async function authenticateRoute(query, {valid = false, backend = false} = {}) {
    await page.goto("about:blank");
    const before = sequenceRequests().length;
    await page.goto(`${server.base}/#sequence?${query}`, {waitUntil:"domcontentloaded"});
    assert.equal(sequenceRequests().length, before, "a URL never reads evidence before Connect");
    await page.locator("#token").fill(paths.token);
    await page.locator("#connect-form button[type=submit]").click();
    await page.waitForFunction(() => !document.querySelector("#checkout-select").disabled);
    assert.equal(await page.locator("#checkout-select").inputValue(), "");
    assert.equal(sequenceRequests().length, before, "authentication alone does not choose a checkout for the route");
    const responsePromise = valid || backend ? page.waitForResponse(response => new URL(response.url()).pathname.endsWith("/sequence")) : null;
    await page.locator("#checkout-select").selectOption(server.rootKey);
    await page.locator("#workspace").waitFor({state:"visible"});
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

test("Libraries candidate source never sends candidate IDs to workspace sequence or navigation", {timeout:240000}, async () => published(async ({page, paths, server, requests}) => {
  await expandSources(page); await page.locator('.file-pick[title="src/flow.js"]').click();
  await page.locator(".method-pick").filter({hasText:"measured"}).click();
  await page.waitForFunction(() => document.querySelector("#sequence-state")?.textContent.includes("Pele rendered"));
  const selection = await page.locator(".method-pick.selected").textContent();
  const diagram = await page.locator("#sequence-diagram").textContent();
  await page.locator("#view-libraries").click();
  // Explicit fixture setup: serve registration can leave the automatic catalog
  // loading without a worker (tracked separately from #392 source actions).
  const started = await H.api(server.base, paths.token, "POST", H.scoped(server.rootKey, "dependencies/refresh"), {});
  assert.equal(started.status, 202, JSON.stringify(started.data));
  // Wait for that requested producer, then bind the browser status refresh to
  // its own new request rather than an older in-flight response.
  const readyCatalog = await page.evaluate(async ({rootKey, token}) => {
    const deadline = Date.now() + 30000;
    for (;;) {
      const response = await fetch(`/api/checkouts/${rootKey}/dependencies`, {cache:"no-store", headers:{Authorization:`Bearer ${token}`}});
      const data = await response.json();
      if (!response.ok || ["failed", "disabled"].includes(data.state)) throw new Error(`Catalog unavailable: ${JSON.stringify(data)}`);
      if (data.state === "ready") return data;
      if (Date.now() >= deadline) throw new Error(`Catalog readiness timed out: ${JSON.stringify(data)}`);
      await new Promise(resolve => setTimeout(resolve, 250));
    }
  }, {rootKey:server.rootKey, token:paths.token});
  assert.equal(readyCatalog.state, "ready", JSON.stringify(readyCatalog));
  const catalogRequest = page.waitForRequest(request => new URL(request.url()).pathname.endsWith("/dependencies"));
  await page.locator("#dependency-refresh").click();
  const refreshRequest = await catalogRequest;
  assert.equal(refreshRequest.method(), "GET");
  const catalogReply = await refreshRequest.response();
  assert.ok(catalogReply, "the explicit catalog refresh must return a response");
  const catalog = await catalogReply.json();
  assert.equal(catalogReply.status(), 200, JSON.stringify(catalog));
  assert.equal(catalog.state, "ready", JSON.stringify(catalog));
  const localPackage = catalog.packages.find(pkg => pkg.name === "local-library");
  const libraryPresentation = async () => page.evaluate(() => ({state:document.querySelector("#dependency-state")?.textContent,
    warnings:document.querySelector("#dependency-warnings")?.textContent,
    buttons:[...document.querySelectorAll("#dependency-packages button")].map(button => ({text:button.textContent, disabled:button.disabled}))}));
  assert.ok(localPackage, `Missing local path dependency: catalog=${JSON.stringify(catalog)}; UI=${JSON.stringify(await libraryPresentation())}`);
  assert.equal(localPackage.version, "0.1.0", JSON.stringify(localPackage));
  assert.equal(localPackage.sourceState, "present", JSON.stringify(localPackage));
  const pkg = page.locator("#dependency-packages button").filter({hasText:`${localPackage.name} ${localPackage.version}`});
  try { await pkg.waitFor(); }
  catch (error) { assert.fail(`Native package not displayed: package=${JSON.stringify(localPackage)}; catalog=${JSON.stringify(catalog)}; UI=${JSON.stringify(await libraryPresentation())}; ${error.message}`); }
  await pkg.click();
  const symbolResponse = page.waitForResponse(response => new URL(response.url()).pathname.endsWith("/dependencies/source"));
  await page.locator("#dependency-symbols button").filter({hasText:"candidate_only"}).click();
  const response = await symbolResponse, snapshot = await response.json();
  assert.equal(response.status(), 200, JSON.stringify(snapshot));
  assert.ok(snapshot.id && snapshot.hash && snapshot.rootId);
  assert.equal(new URL(response.url()).searchParams.get("sourceRef"), snapshot.id);
  assert.ok(snapshot.definitions.some(item => item.name === "candidate_only"));
  await page.waitForFunction(hash => document.querySelector("#external-source-path")?.textContent.includes(hash), snapshot.hash);
  assert.match(await page.locator("#external-source-path").textContent(), /immutable candidate snapshot.*terminal library boundary/);
  assert.equal(await page.locator("#external-source").isVisible(), true);
  assert.equal(await page.locator("#external-source .source-diagram-action").count(), 0);
  assert.equal(await page.locator("#external-source-hint").isVisible(), true);
  assert.match(await page.locator("#external-source-hint").textContent(), /Workspace diagrams are unavailable/);
  assert.doesNotMatch(await page.locator("#external-source").textContent(), /Workspace diagrams are unavailable/);
  assert.equal(await page.locator("#external-source .source-line").evaluateAll(lines => lines.map(line =>
    [...line.childNodes].filter(node => !node.classList?.contains("line-number")).map(node => node.textContent).join("")).join("\n")), snapshot.file.text);
  assert.equal(await page.locator("#external-source img").count(), 0);
  assert.equal(await page.evaluate(() => globalThis.libraryExecuted), undefined);
  const before = requests.filter(row => /\/(sequence|navigation)$/.test(row.url.pathname)).length;
  await page.locator("#external-source .source-line").filter({hasText:"pub fn candidate_only"}).click();
  await page.locator("#external-source").focus(); await page.keyboard.press("Shift+F10");
  assert.equal(await page.locator(".classes-context-menu").count(), 0, "external source has no workspace action menu");
  await page.keyboard.press("Escape");
  assert.equal(requests.filter(row => /\/(sequence|navigation)$/.test(row.url.pathname)).length, before);
  const ids = new Set(snapshot.definitions.map(item => item.id).filter(Boolean));
  assert.ok(!requests.some(row => row.body && ids.has(row.body.seed)), "candidate IDs are never workspace sequence seeds");
  assert.equal(await page.locator(".method-pick.selected").textContent(), selection);
  await page.locator("#view-sequence").click();
  assert.equal(await page.locator("#sequence-diagram").textContent(), diagram);
}, undefined, {library:true}));

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
  await page.locator("#all-steps").check();
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
  assert.match(await page.locator("#files-state").textContent(), /2 hidden or unindexed entries not shown/);
  const toggle = page.getByRole("checkbox", {name:"Show hidden/ignored", exact:true});
  await toggle.focus(); await page.keyboard.press("Space");
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
  await page.locator("#metric-include-tests").check();
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
  assert.ok(await page.locator("#metrics-state").isVisible());
  assert.ok((await page.locator("#metrics-state").textContent()).includes(warning));
  const measured = payload.files.find(row => row.path === "src/flow.js");
  assert.ok(measured.metrics.cognitive > 0);
  const fixture = payload.files.find(row => row.path === "tests/fixtures/input.js");
  assert.equal(fixture.excludedReason, "fixture"); assert.equal(fixture.metrics.cognitive, null);
  const vendored = payload.files.find(row => row.path.startsWith("web/vendor/"));
  assert.equal(vendored.excludedReason, "vendored"); assert.equal(vendored.metrics.cognitive, null);
  await expandSources(page); await assertLegend(page, payload, "cognitive");
  assert.doesNotMatch(await page.locator('.file-pick[title="src/flow.js"] .metric-swatch').getAttribute("class"), /neutral/);

  await page.locator("#view-treemap").click();
  await page.locator("#treemap-state[data-state=ready]").waitFor();
  assert.ok(await page.locator("#treemap-state").isVisible());
  assert.ok((await page.locator("#treemap-state").textContent()).includes(warning));
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

test("connected chrome uses one desktop action line, folded details and brand-sized controls without losing narrow drawers", {timeout:240000}, async () => published(async ({page, requests}) => {
  const toolbar = page.locator(".workspace-toolbar");
  const header = await page.locator(".app-header").boundingBox();
  const actions = await toolbar.boundingBox();
  const frame = await page.locator(".workspace-frame").boundingBox();
  const primary = await page.locator("#index").boundingBox();
  assert.ok(header && actions && frame && primary);
  assert.ok(primary.height >= 44, "explicit Index is a primary action of at least 44px");
  assert.ok(actions.height <= primary.height + 20, "closed desktop status/actions occupy one compact line");
  assert.ok(Math.abs(actions.y - (header.y + header.height)) <= 1, "no revision or browse-notice row separates header and actions");
  assert.ok(Math.abs(frame.y - (actions.y + actions.height)) <= 1, "no job row separates actions from the panes");
  const headerControls = await Promise.all([".brand", ".section-nav", "#checkout-select", "#logout"].map(selector => page.locator(selector).boundingBox()));
  assert.ok(headerControls.every(Boolean));
  assert.ok(Math.max(...headerControls.map(box => box.y)) < Math.min(...headerControls.map(box => box.y + box.height)), "desktop header controls share one row");
  for (const id of ["workspace-meta", "checkout-root", "status", "diagnostics", "job", "stale"]) {
    assert.equal(await page.locator(`#${id}`).evaluate(node => node.closest("#workspace-details") !== null), true, `${id} belongs to folded Diagnostics`);
    assert.equal(await page.locator(`#${id}`).isVisible(), false, `${id} consumes no closed-chrome height`);
  }
  assert.equal(await page.locator("#checkout-state").isVisible(), false, "successful selection adds no guidance row");
  assert.equal(await page.locator("#notice").isVisible(), false, "browse guidance is not a separate connected row");
  for (const selector of ["#refresh", "#index", "#checkout-select", "#file-filter", "#metric-color", "#metric-aggregate"]) {
    const style = await page.locator(selector).evaluate(node => ({font:getComputedStyle(node).fontSize, radius:getComputedStyle(node).borderTopLeftRadius}));
    assert.deepEqual(style, {font:"14px", radius:"8px"}, `${selector} uses interface label and control tokens`);
  }
  assert.ok((await page.locator("#refresh").boundingBox()).height < primary.height, "secondary controls stay compact rather than all becoming primary-sized");
  await capture(page, "chrome-compact-desktop");
  for (const width of [390, 320]) {
    await page.setViewportSize({width, height:844});
    await noOverflow(page, width);
    for (const selector of ["#checkout-select", "#refresh", "#index", "#workspace-details > summary"]) {
      const box = await page.locator(selector).boundingBox();
      assert.ok(box && box.x >= 0 && box.x + box.width <= width + 1, `${selector} remains reachable at ${width}px`);
    }
    await page.locator("#explorer-toggle").click();
    assert.equal(await page.locator("#explorer-panel").isVisible(), true);
    await page.keyboard.press("Escape");
    assert.equal(await page.locator("#explorer-panel").isVisible(), false);
    await capture(page, `chrome-compact-${width}`);
  }
  assert.equal(requests.some(row => row.method === "POST" && row.url.pathname.endsWith("/index")), false, "layout and drawer actions never request indexing");
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
    await assertIndexState(page, "failed", /Index failed/i);
    assert.doesNotMatch(await page.locator("#workspace-index-label").textContent(), /Fresh|0\/0|index_failed/);
    const selectedCheckout = page.locator('#indexed-checkouts .checkout-pick[aria-pressed="true"] .checkout-freshness');
    assert.match(await selectedCheckout.textContent(), /Index failed/i);
    assert.doesNotMatch(await selectedCheckout.textContent(), /Fresh/);
    assert.equal(await page.locator("#job").isVisible(), false);
    assert.equal(await page.locator("body").evaluate((node, text) => node.innerText.includes(text), failure), false, "raw error text is not exposed in the compact chrome");
    await page.locator("#workspace-details > summary").click();
    assert.ok((await page.locator("#job").textContent()).includes(failure));
    assert.match(await page.locator("#job").textContent(), /index_failed/);
    assert.equal(await page.locator("#workspace-details img").count(), 0, "error objects render only as inert text");
    assert.equal(await page.evaluate(() => globalThis.statusExecuted), undefined);
    await capture(page, "chrome-failed-diagnostics");
    await page.locator("#workspace-details > summary").click();

    fixture.job = {...job, state:"done", error:null, revision:pin};
    fixture.status.catchingUp = true;
    await fixture.select();
    await assertIndexState(page, "catching-up", /Catching up/i);
    assert.doesNotMatch(await page.locator("#workspace-index-label").textContent(), /Fresh|0\/0/);
    fixture.status.catchingUp = false;
    fixture.status.stats.changedFiles = ["src/flow.js"];
    await fixture.select();
    await assertIndexState(page, "stale", /Stale evidence/i);
    await page.locator("#workspace-details > summary").click();
    assert.match(await page.locator("#stale").textContent(), /1 inputs differ/);
    await page.locator("#workspace-details > summary").click();
    fixture.status.stats.changedFiles = [];
    await fixture.select();
    await assertIndexState(page, "fresh", /Fresh/i);
    assert.doesNotMatch(await page.locator("#job").textContent(), /0\/0/);
    assert.equal(requests.some(row => row.method === "POST" && row.url.pathname.endsWith("/index")), false, "selecting or refreshing status never indexes");

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
  const shortRevision = `rev ${pin.indexGeneration.slice(0, 8)}:${pin.indexRevision}`;
  assert.equal(await page.locator(`#checkout-select option[value="${server.rootKey}"]`).textContent(), `workspace ⎇ measured-branch · ${shortRevision}`, "the real measured Git branch labels the published checkout");
  const branch = '<svg onload="globalThis.branchExecuted=true">' + "-long-branch".repeat(12);
  const available = {...gitResponse.data.git, head:{...gitResponse.data.git.head, name:branch}};
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
    await page.locator("#logout").click();
    await page.locator("#token").fill(paths.token);
    await page.locator("#connect-form button[type=submit]").click();
    await page.waitForFunction(() => !document.querySelector("#checkout-select").disabled);
    assert.equal(await page.locator("#checkout-select").inputValue(), "", "even a measured branch never auto-selects a checkout");
    const option = page.locator(`#checkout-select option[value="${server.rootKey}"]`);
    assert.equal(await option.textContent(), `workspace ⎇ ${branch} · ${shortRevision}`);
    assert.equal(await option.getAttribute("title"), observed.workspaceRoot);
    for (const row of rows.slice(1)) assert.equal(await page.locator(`#checkout-select option[value="${row.rootKey}"]`).isDisabled(), true, `${row.state} entries cannot be selected`);
    assert.equal(await page.locator("#checkout-retry").isVisible(), true);
    await page.locator("#checkout-select").selectOption(server.rootKey);
    await page.waitForFunction(() => document.querySelector("#checkout-state")?.textContent === "");
    assert.equal(await page.locator("#checkout-select").getAttribute("title"), observed.workspaceRoot);
    assert.equal(await page.locator("#workspace-name").getAttribute("title"), observed.workspaceRoot);
    assert.equal(await page.locator(".app-header svg, #indexed-checkouts svg").count(), 0);
    assert.equal(await page.evaluate(() => globalThis.branchExecuted), undefined);
    for (const width of [390, 320]) {
      await page.setViewportSize({width, height:844});
      assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true, `${width}px long branch does not create document overflow`);
      const picker = await page.locator("#checkout-select").boundingBox();
      assert.ok(picker && picker.x >= 0 && picker.x + picker.width <= width + 1);
    }
    await page.setViewportSize({width:1440, height:900});
    assert.equal(await page.locator("#checkout-root").isVisible(), false);
    await page.locator("#workspace-details > summary").click();
    assert.equal(await page.locator("#checkout-root").textContent(), observed.workspaceRoot);
    await page.locator("#workspace-details > summary").click();
    for (const [git, expected] of [
      [{...available, head:{...available.head, kind:"detached", name:null}}, `Detached ${available.head.shortCommit}`],
      [{state:"unavailable", reason:"not_repository"}, "Git unavailable"],
      [{...available, head:{kind:"detached", name:null, shortCommit:null}}, "Detached"],
    ]) {
      rows[0].git = git;
      const refreshed = page.waitForResponse(response => response.url() === pattern);
      await page.locator("#checkout-retry").click();
      assert.equal((await refreshed).status(), 200);
      await page.waitForFunction(({key, label}) => [...document.querySelector("#checkout-select").options].find(option => option.value === key)?.textContent.includes(label), {key:server.rootKey, label:expected});
      assert.equal(await option.textContent(), `workspace ⎇ ${expected} · ${shortRevision}`);
      assert.equal(await page.locator("#checkout-select").inputValue(), server.rootKey, "retry retains explicit selection of the same available checkout");
    }
    const listedGit = page.locator('#indexed-checkouts .checkout-pick[aria-pressed="true"] .checkout-git');
    await page.waitForFunction(() => /^Detached HEAD/.test(document.querySelector('#indexed-checkouts .checkout-pick[aria-pressed="true"] .checkout-git')?.textContent || ""));
    assert.doesNotMatch(await listedGit.textContent(), /Git unavailable/, "a detached HEAD without a short commit is still Git");
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
    await page.locator("#refresh").click();
    assert.equal((await retried).status(), 200, "Refresh retries the failed job lookup");
    await assertIndexState(page, "fresh", /^Fresh/);
    assert.equal(await page.locator("#checkout-state").isVisible(), false);

    fixture.job = {id:"live-job", state:"running", error:null, finishedAt:null, revision:null, progress:{phase:"capture", completed:1, total:10}};
    await page.locator("#index").click();
    await page.waitForFunction(() => document.querySelector("#workspace-index-label")?.textContent === "Indexing · capture · 1/10");
    assert.equal(await page.locator("#workspace-index-announce").textContent(), "Indexing · capture · 1/10");
    fixture.job = {...fixture.job, progress:{phase:"capture", completed:6, total:10}};
    await page.waitForFunction(() => document.querySelector("#workspace-index-label")?.textContent === "Indexing · capture · 6/10");
    assert.equal(await page.locator("#workspace-index-announce").textContent(), "Indexing · capture · 1/10", "a progress-only poll is not announced");
    fixture.job = {...fixture.job, state:"failed", error:{code:"index_failed", message:"fixture"}, finishedAt:1};
    await page.waitForFunction(() => document.querySelector("#workspace-index-announce")?.textContent === "Index failed · see Diagnostics");
    assert.equal(requests.filter(row => row.method === "POST" && row.url.pathname.endsWith("/index")).length, 1, "only the explicit Index click requests a job");
  } finally {
    await page.unroute(jobsPattern, failJobs).catch(() => {});
    await fixture.close();
  }
}));

test("an unsaved token warning stays visible in the header after a checkout is selected", {timeout:240000}, async () => published(async ({page, paths, server}) => {
  const warning = "Browser storage is unavailable; token was not saved.";
  await page.locator("#logout").click();
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
