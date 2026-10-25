"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const {mkdirSync, writeFileSync, rmSync} = require("node:fs");
const {dirname, join} = require("node:path");
const {chromium} = require("playwright");
const H = require("./sequence-harness.cjs");

const hostileLayer = '<img src=x onerror="globalThis.architectureExecuted=true">';
const lowSource = `pub fn low() { crate::top::target(); external::unknown(); }
#[cfg(test)] mod tests {
    fn first() { if true { if true { crate::top::target(); } } }
    fn second() { crate::top::target(); }
}
#[cfg(feature = "test-support")]
pub fn support() { crate::top::target(); }
`;
const sources = new Map([
  ["src/top.rs", "pub fn target() {}\npub fn top() { crate::mid::target(); crate::mid::target(); crate::low::low(); crate::shared::target(); crate::top::target(); }\n"],
  ["src/mid.rs", "pub fn target() { crate::peer::target(); }\n#[cfg(test)] fn lower_test() { crate::low::low(); }\n"],
  ["src/peer.rs", "pub fn target() {}\n"],
  ["src/low.rs", lowSource],
  ["src/shared.rs", "pub fn target() { crate::mid::target(); }\n"],
  ["src/unassigned.rs", "pub fn unknown_layer() { crate::top::target(); }\n"],
  ["src/only.rs", "#![cfg(test)]\nfn only(value: i32) -> i32 { if value > 0 { return value; } 0 }\n"],
]);
// Normal-sized layout: native files, not authored package/group nodes.
for (let i = 0; i < 33; i++) sources.set(`src/leaf_${i}.rs`, `pub fn leaf_${i}(value: i32) -> i32 { ${i % 3 ? "value + 1" : "if value > 0 { value } else { 0 }"} }\n`);
const names = [...sources.keys()].map(path => path.slice(4, -3));
sources.set("src/lib.rs", names.map(name => `pub mod ${name};`).join("\n") + "\n");
const rules = `version = 1
[layers.top]
order = 1
paths = ["src/lib.rs", "src/top.rs", "src/leaf_0.rs", "src/leaf_1.rs", "src/leaf_2.rs", "src/leaf_3.rs", "src/leaf_4.rs", "src/leaf_5.rs", "src/leaf_6.rs", "src/leaf_7.rs", "src/leaf_8.rs", "src/leaf_9.rs"]
[layers.mid]
order = 2
paths = ["src/mid.rs", "src/leaf_1?.rs"]
[layers.peer]
order = 2
paths = ["src/peer.rs"]
[layers.low]
order = 3
paths = ["src/low.rs", "src/leaf_2?.rs", "src/leaf_3?.rs"]
[layers.shared]
shared = true
paths = ["src/shared.rs"]
[layers.'${hostileLayer}']
shared = true
paths = ["src/no-match.rs"]
`;

async function capture(page, name) {
  const directory = process.env.TRELLIS_BROWSER_SCREENSHOTS;
  if (!directory) return;
  mkdirSync(directory, {recursive:true});
  await page.screenshot({path:join(directory, `${name}.png`), fullPage:true});
}
test.before(async () => H.setup(), {timeout:960000});
test.after(async () => H.cleanup());

async function published(config, callback) {
  const paths = H.fixture("", "placeholder.txt");
  rmSync(paths.source);
  for (const [path, text] of sources) {
    const target = join(paths.workspace, path);
    mkdirSync(dirname(target), {recursive:true}); writeFileSync(target, text);
  }
  if (config !== null) {
    mkdirSync(join(paths.workspace, ".trellis"));
    writeFileSync(join(paths.workspace, ".trellis/rules.toml"), config);
  }
  let server, browser, context;
  try {
    const indexed = await H.run(H.binary, ["index", "--workspace", paths.workspace], {env:paths.env, timeout:120000});
    const pin = JSON.parse(indexed.stdout).status.revision;
    server = await H.startDaemon(paths, pin.indexRevision);
    browser = await chromium.launch();
    context = await browser.newContext({viewport:{width:1440, height:1000}});
    const page = await context.newPage(), errors = [], requests = [];
    page.on("pageerror", error => errors.push(error.message));
    page.on("console", message => {
      if (message.type() === "error" && /Content Security Policy|CSP/i.test(message.text())) errors.push(message.text());
    });
    page.on("request", request => {
      const url = new URL(request.url());
      if (url.pathname.startsWith("/api/")) requests.push({url, method:request.method()});
    });
    await H.connect(page, server, paths.token);
    await page.locator("#view-architecture").click();
    await callback({page, paths, pin, server, requests});
    assert.deepEqual(errors, [], "real local assets and repository text obey the unchanged CSP");
    assert.equal(requests.some(row => row.method === "POST" && row.url.pathname.endsWith("/index")), false, "browsing does not index");
    for (const row of requests.filter(row => row.url.pathname.endsWith("/architecture"))) {
      assert.equal(row.method, "GET");
      assert.ok(["production", "all"].includes(row.url.searchParams.get("role")), "browser explicitly selects its metric population");
      assert.equal(row.url.searchParams.get("indexGeneration"), pin.indexGeneration);
      assert.equal(row.url.searchParams.get("indexRevision"), String(pin.indexRevision));
      assert.ok(row.url.pathname.startsWith(`/api/checkouts/${server.rootKey}/`));
    }
    for (const row of requests.filter(row => row.url.pathname.endsWith("/source"))) {
      assert.equal(row.url.searchParams.get("indexGeneration"), pin.indexGeneration);
      assert.equal(row.url.searchParams.get("indexRevision"), String(pin.indexRevision));
    }
  } finally {
    await context?.close(); await browser?.close(); await H.stopDaemon(server);
    rmSync(paths.temp, {recursive:true, force:true, maxRetries:10, retryDelay:100});
  }
}
async function architecture(selected, aggregate = "max", role = "production") {
  const query = new URLSearchParams({aggregate, role, indexGeneration:selected.pin.indexGeneration, indexRevision:selected.pin.indexRevision});
  return H.api(selected.server.base, selected.paths.token, "GET", H.scoped(selected.server.rootKey, `architecture?${query}`));
}
const identityKey = identity => JSON.stringify([identity.sourceSetId, identity.language, identity.path, identity.sid]);
const siteTotal = edges => edges.reduce((sum, edge) => sum + edge.count, 0);
const summaryKey = edge => JSON.stringify([edge.sourceLayer, edge.destinationLayer, edge.role, edge.status, edge.excepted, edge.exceptionReason]);
async function ready(page) {
  await page.locator("#architecture-state[data-state=ready]").waitFor();
  await page.locator("#architecture-diagram svg").waitFor({state:"attached"});
}
async function noOverflow(page) {
  assert.equal(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), true, "only the contained diagram may overflow horizontally");
}

test("Architecture uses real pinned structural sites, authored native-file layers and stored production/test roles", {timeout:240000}, async () => published(rules, async selected => {
  const {page, pin, paths} = selected;
  const result = await architecture(selected);
  assert.equal(result.status, 200, JSON.stringify(result.data));
  const dto = result.data.payload;
  assert.equal(dto.indexGeneration, pin.indexGeneration); assert.equal(dto.indexRevision, pin.indexRevision);
  assert.equal(dto.state, "ready"); assert.equal(dto.evidenceTier, "syntax"); assert.equal(dto.semanticComplete, false);
  assert.equal(dto.role, "production");
  assert.equal(dto.modules.length, 41);
  assert.equal(new Set(dto.modules.map(module => identityKey(module.identity))).size, dto.modules.length);
  const cli = await H.run(H.binary, ["references", "--workspace", paths.workspace, "--pin", `${pin.indexGeneration}:${pin.indexRevision}`, "--limit", "5"], {env:paths.env});
  const references = JSON.parse(cli.stdout).payload;
  assert.deepEqual(dto.references, references, "browser reads exactly the same references projection at the complete pin");
  assert.equal(siteTotal(references.production), 10); assert.equal(siteTotal(references.tests), 3);
  assert.equal(references.counts.mappedSites, 13); assert.equal(references.counts.unresolvedSites, 1);
  const reverse = references.production.find(edge => edge.source.path === "src/low.rs" && edge.target.path === "src/top.rs");
  assert.equal(reverse.count, 2, "feature test-support is stored production, not inferred test");
  assert.equal(references.tests.find(edge => edge.source.path === "src/low.rs" && edge.target.path === "src/top.rs").count, 2);
  const layerFor = new Map(dto.modules.map(module => [identityKey(module.identity), module.layer]));
  const expected = new Map();
  for (const [role, edges] of [["production", references.production], ["test", references.tests]]) {
    for (const edge of edges) {
      const sourceLayer = layerFor.get(identityKey(edge.source)), destinationLayer = layerFor.get(identityKey(edge.target));
      if (!sourceLayer || !destinationLayer || sourceLayer === destinationLayer) continue;
      const instances = role === "production" ? dto.rules.instances : dto.rules.testOnly;
      const instance = instances.find(row => row.rule === "$layers" && row.destination !== null && identityKey(row.target) === identityKey(edge.source) && identityKey(row.destination) === identityKey(edge.target));
      assert.ok(instance, JSON.stringify(edge));
      const key = summaryKey({sourceLayer, destinationLayer, role, ...instance});
      expected.set(key, (expected.get(key) || 0) + edge.count);
    }
  }
  assert.deepEqual(new Map(dto.edges.map(edge => [summaryKey(edge), edge.count])), expected);
  for (const edge of dto.edges) assert.equal(siteTotal(edge.pairs), edge.count, "samples cannot replace exact site counts");
  assert.equal(siteTotal(dto.edges.filter(edge => edge.role === "production")), 8, "same-layer and unassigned sites stay outside summary arrows");
  assert.ok(dto.edges.some(edge => edge.sourceLayer === "top" && edge.destinationLayer === "low" && edge.status === "conforms"), "skipped orders conform");
  for (const [sourceLayer, destinationLayer] of [["low", "top"], ["mid", "peer"], ["shared", "mid"]]) {
    assert.equal(dto.edges.find(edge => edge.role === "production" && edge.sourceLayer === sourceLayer && edge.destinationLayer === destinationLayer).status, "deviates");
  }
  assert.equal(dto.modules.find(module => module.identity.path === "src/unassigned.rs").layer, null);
  await ready(page);
  await page.locator("#section-files").click();
  await page.waitForFunction(() => document.body.classList.contains("files-section"));
  assert.equal(await page.locator("#architecture-panel").isVisible(), false);
  await page.locator("#section-diagrams").click();
  await page.waitForFunction(() => !document.body.classList.contains("files-section"));
  await ready(page);
  assert.equal(await page.locator("#view-architecture").getAttribute("aria-selected"), "true");
  assert.equal(await page.locator("#diagram-metrics").isVisible(), false, "Architecture hides the independently owned file-tree metric controls after Files round-trip");
  assert.equal(await page.locator("#architecture-diagram [data-trellis-module-id]").count(), dto.modules.length, "returning from Files restores a rendered Architecture, not an empty stale tab");
  assert.ok((await page.locator("#architecture-diagram [data-trellis-module-id]").allTextContents()).some(text => text.includes("Unavailable")), "numeric mode keeps null metrics explicitly unavailable");
  assert.match(await page.locator("#architecture-panel").textContent(), /structural references.*syntax evidence/is);
  assert.equal(await page.locator("#architecture-metric-tests").isChecked(), false);
  assert.match(await page.locator("#architecture-panel").textContent(), /production callables/i);
  assert.ok(selected.requests.filter(row => row.url.pathname.endsWith("/architecture")).every(row => row.url.searchParams.get("role") === "production"));
  assert.match(await page.locator("#architecture-panel").textContent(), /Unassigned|unknown/i);
  assert.ok((await page.locator("#architecture-diagram [role=button]").evaluateAll(nodes => nodes.map(node => node.getAttribute("aria-label")))).some(label => label.includes(hostileLayer)), "clipped labels retain their complete accessible text");
  assert.equal(await page.locator("#architecture-diagram img, #architecture-diagram script, #architecture-diagram foreignObject").count(), 0);
  assert.equal(await page.evaluate(() => globalThis.architectureExecuted), undefined);
  assert.equal(await page.locator("#architecture-inspector a").filter({hasText:/treemap/i}).count(), 0, "#121 has no placeholder action");

  await page.locator("#architecture-notices > summary").click();
  await page.getByRole("button", {name:"Inspect unresolved sites", exact:true}).click();
  assert.equal(await page.locator("#architecture-inspector-title").textContent(), "Unresolved structural references");
  assert.match(await page.locator("#architecture-inspector-content").textContent(), /rust_non_crate_path/);
  await page.getByRole("button", {name:"Inspect same-layer and unassigned references", exact:true}).click();
  assert.match(await page.locator("#architecture-inspector-content").textContent(), /src\/unassigned\.rs/);
  assert.match(await page.locator("#architecture-inspector-content").textContent(), /src\/top\.rs.*1 reference sites/is);
  const module = page.locator("#architecture-diagram").getByRole("button", {name:/^src\/low\.rs(?:\s|$)/});
  assert.equal(await page.locator("#architecture-diagram").getByRole("button", {name:/^src\//}).count(), dto.modules.length, "every indexed file module has one interactive mapping");
  assert.deepEqual(await page.locator("#architecture-diagram [data-trellis-module-id]").evaluateAll(nodes => nodes.map(node => node.getAttribute("data-trellis-module-id")).sort()), dto.modules.map(row => identityKey(row.identity)).sort(), "complete native identities map once, never by label or position");
  assert.deepEqual(await page.locator("#architecture-diagram [data-trellis-edge-id]").evaluateAll(nodes => nodes.map(node => node.getAttribute("data-trellis-edge-id")).sort()), dto.edges.filter(row => row.role === "production").map(summaryKey).sort(), "summary identity includes direction, role, policy and exception");
  await module.click();
  await page.waitForFunction(() => document.querySelector("#architecture-inspector-title")?.textContent === "src/low.rs");
  const low = dto.modules.find(row => row.identity.path === "src/low.rs");
  const detail = await page.locator("#architecture-inspector-content").textContent();
  assert.ok(detail.includes(`${low.physicalLines} physical file lines`));
  assert.equal(low.declarationCount, 2);
  assert.equal(low.metrics.cognitive, 0, "both production low and feature test-support have no decisions");
  assert.ok(detail.includes(`${low.declarationCount} production callables`));
  for (const value of Object.values(low.identity)) assert.ok(detail.includes(value), "inspector retains complete native identity");
  for (const label of ["Cognitive", "Cyclomatic", "Max nesting", "Maintainability", "Halstead", "Fan-in", "Fan-out"]) assert.ok(detail.includes(label), label);
  assert.match(detail, /—/, "unavailable values remain null, not zero");
  const sourceCount = selected.requests.filter(row => row.url.pathname.endsWith("/source")).length;
  await page.locator("#architecture-inspector").getByRole("button", {name:"Read indexed file source", exact:true}).click();
  await page.waitForFunction(() => document.querySelector("#source-path")?.textContent.includes("src/low.rs") && document.querySelector("#source .source-line"));
  assert.match(await page.locator("#source-path").textContent(), /cached snapshot/);
  assert.equal(await page.locator("#source .source-line").evaluateAll(lines => lines.map(line => [...line.childNodes].filter(node => !node.classList?.contains("line-number")).map(node => node.textContent).join("")).join("\n")), lowSource);
  assert.ok(selected.requests.filter(row => row.url.pathname.endsWith("/source")).length > sourceCount);
  await module.focus(); await page.keyboard.press("Enter");
  assert.equal(await page.locator("#architecture-inspector-title").textContent(), "src/low.rs");
  const top = page.locator("#architecture-diagram").getByRole("button", {name:/^src\/top\.rs(?:\s|$)/});
  await top.focus(); await page.keyboard.press("Space");
  assert.equal(await page.locator("#architecture-inspector-title").textContent(), "src/top.rs");
  // Exact counts live in the complete accessible label even when the drawn label is clipped.
  const beforeProduction = await page.locator('#architecture-diagram [role=button][aria-label*="production sites"]').evaluateAll(nodes => nodes.map(node => node.getAttribute("aria-label")).sort());
  assert.equal(beforeProduction.length, dto.edges.filter(edge => edge.role === "production").length);
  assert.equal(await page.locator('#architecture-diagram [role=button][aria-label*="test sites"]').count(), 0);
  const heat = await module.locator("rect").first().getAttribute("fill"), legend = await page.locator("#architecture-legend").textContent();
  await page.locator("#architecture-tests").check();
  await page.waitForFunction(count => [...document.querySelectorAll('#architecture-diagram [role=button]')].filter(node => node.getAttribute("aria-label")?.includes("test sites")).length === count, dto.edges.filter(edge => edge.role === "test").length);
  assert.deepEqual(await page.locator('#architecture-diagram [role=button][aria-label*="production sites"]').evaluateAll(nodes => nodes.map(node => node.getAttribute("aria-label")).sort()), beforeProduction);
  assert.deepEqual(await page.locator("#architecture-diagram [data-trellis-edge-id]").evaluateAll(nodes => nodes.map(node => node.getAttribute("data-trellis-edge-id")).sort()), dto.edges.map(summaryKey).sort(), "test toggle preserves complete exact summary mapping");
  assert.equal(await module.locator("rect").first().getAttribute("fill"), heat);
  assert.equal(await page.locator("#architecture-legend").textContent(), legend, "hiding/showing tests does not reclassify metric population");
  const only = dto.modules.find(row => row.identity.path === "src/only.rs");
  assert.equal(only.declarationCount, 0);
  assert.equal(only.metrics.cognitive, null, "a test-only file has unavailable production heat, never zero");
  const onlyModule = page.locator("#architecture-diagram").getByRole("button", {name:/^src\/only\.rs(?:\s|$)/});
  assert.match(await onlyModule.textContent(), /Unavailable/);
  await onlyModule.click();
  assert.match(await page.locator("#architecture-inspector-content").textContent(), /No production callables/);
  await module.click();
  await capture(page, "architecture-production-desktop");
  await page.setViewportSize({width:390, height:844});
  if (await page.locator("#inspector-toggle").getAttribute("aria-expanded") === "true") await page.locator("#inspector-toggle").click();
  assert.equal(await page.locator("#architecture-metric-tests").isVisible(), true);
  await capture(page, "architecture-production-mobile");
  await page.locator("#inspector-toggle").click();
  await capture(page, "architecture-production-mobile-inspector");
  await page.setViewportSize({width:1440, height:1000});
  const withTests = await architecture(selected, "max", "all");
  assert.equal(withTests.status, 200, JSON.stringify(withTests.data));
  const all = withTests.data.payload;
  assert.equal(all.role, "all");
  assert.equal(all.indexGeneration, pin.indexGeneration); assert.equal(all.indexRevision, pin.indexRevision);
  const allLow = all.modules.find(row => row.identity.path === "src/low.rs");
  assert.equal(allLow.declarationCount, 4);
  assert.equal(allLow.metrics.cognitive, 3, "two nested test ifs contribute 1 + 2, while low and test-support stay production");
  assert.deepEqual(all.edges, dto.edges, "metric population cannot change reference roles, counts or policy");
  assert.equal(all.modules.find(row => row.identity.path === "src/only.rs").metrics.cognitive, 1);
  await page.locator("#architecture-metric-tests").check();
  await page.waitForFunction(() => document.querySelector("#architecture-state")?.textContent.includes("Metrics: max, all indexed callables"));
  await ready(page);
  assert.equal(await page.locator("#architecture-tests").isChecked(), true, "metric inclusion does not alter the independent reference overlay");
  assert.deepEqual(await page.locator("#architecture-diagram [data-trellis-edge-id]").evaluateAll(nodes => nodes.map(node => node.getAttribute("data-trellis-edge-id")).sort()), dto.edges.map(summaryKey).sort());
  assert.notEqual(await module.locator("rect").first().getAttribute("fill"), heat, "real higher-complexity tests visibly change the low module's heat");
  assert.match(await page.locator("#architecture-legend").textContent(), /all indexed callables/);
  assert.doesNotMatch(await onlyModule.textContent(), /Unavailable/, "test-only module becomes measured only when tests are included");
  await module.click();
  assert.match(await page.locator("#architecture-inspector-content").textContent(), /4 indexed callables.*all indexed callables/is);
  await capture(page, "architecture-all-desktop");
  await page.setViewportSize({width:390, height:844});
  if (await page.locator("#inspector-toggle").getAttribute("aria-expanded") === "true") await page.locator("#inspector-toggle").click();
  assert.equal(await page.locator("#architecture-metric-tests").isVisible(), true);
  await capture(page, "architecture-all-mobile");
  await page.locator("#inspector-toggle").click();
  await capture(page, "architecture-all-mobile-inspector");
  await page.setViewportSize({width:1440, height:1000});
  const allHeat = await module.locator("rect").first().getAttribute("fill"), allLegend = await page.locator("#architecture-legend").textContent();
  await page.locator("#architecture-tests").uncheck();
  await page.waitForFunction(() => ![...document.querySelectorAll('#architecture-diagram [role=button]')].some(node => node.getAttribute("aria-label")?.includes("test sites")));
  assert.equal(await page.locator("#architecture-metric-tests").isChecked(), true);
  assert.equal(await module.locator("rect").first().getAttribute("fill"), allHeat);
  assert.equal(await page.locator("#architecture-legend").textContent(), allLegend);
  await page.locator("#architecture-tests").check();
  await ready(page);
  await page.locator("#architecture-metric-tests").uncheck();
  await page.waitForFunction(() => document.querySelector("#architecture-state")?.textContent.includes("Metrics: max, production callables"));
  await ready(page);
  assert.equal(await page.locator("#architecture-tests").isChecked(), true);
  assert.equal(await module.locator("rect").first().getAttribute("fill"), heat);
  const testEdge = page.locator("#architecture-diagram").getByRole("button", {name:/^2 test sites\s+deviation$/});
  await testEdge.focus(); await page.keyboard.press("Enter");
  assert.equal(await page.locator("#architecture-inspector-title").textContent(), "low → top");
  assert.match(await page.locator("#architecture-inspector-content").textContent(), /2 exact test structural reference sites.*hypothetical policy only/is);
  await page.locator("#architecture-tests").uncheck();
  await page.waitForFunction(() => ![...document.querySelectorAll('#architecture-diagram [role=button]')].some(node => node.getAttribute("aria-label")?.includes("test sites")));
  const edge = page.locator("#architecture-diagram").getByRole("button", {name:/^2 production sites\s+deviation$/});
  await edge.click();
  const edgeDetail = await page.locator("#architecture-inspector-content").textContent();
  assert.match(edgeDetail, /2 exact production structural reference sites.*deviates/is);
  assert.match(edgeDetail, /src\/low\.rs.*src\/top\.rs/is);
  assert.match(edgeDetail, /2 sampled sites.*0 omitted sites/is);
  const pair = page.locator("#architecture-inspector-content > details").first();
  await pair.locator(":scope > summary").click();
  const sample = pair.locator(":scope > details").filter({has:page.locator('summary', {hasText:/production · reference/})}).first();
  await sample.locator(":scope > summary").click();
  await sample.getByRole("button", {name:"Read indexed reference source"}).click();
  await page.waitForFunction(() => document.querySelector("#source .source-line.highlight"));
  assert.equal(await page.locator("#source .source-line.highlight .line-number").first().textContent(), String(reverse.sites[0].lines.start));
  assert.equal(await page.locator("#architecture-inspector a").filter({hasText:/treemap/i}).count(), 0);
  await page.locator("#architecture-color").selectOption("layer");
  await page.waitForFunction(() => document.querySelector("#architecture-legend")?.textContent.includes("neutral chips"));
  await ready(page);
  assert.equal(await page.locator('#architecture-diagram [role=button][aria-label^="src/"] rect').evaluateAll(nodes => new Set(nodes.map(node => node.getAttribute("fill"))).size), 1, "Layer mode uses neutral module chips");
  assert.equal((await page.locator("#architecture-diagram [data-trellis-module-id]").allTextContents()).some(text => text.includes("Unavailable")), false, "Layer membership is not a missing numeric metric");
  await page.locator("#architecture-color").selectOption("maintainabilityIndex");
  await page.waitForFunction(() => document.querySelector("#architecture-legend")?.textContent.includes("higher is better; heat inverted"));
  await ready(page);
  await page.locator("#architecture-aggregate").selectOption("mean");
  await page.waitForFunction(() => document.querySelector("#architecture-state")?.textContent.includes("Metrics: mean"));
  await ready(page);
  const means = await architecture(selected, "mean");
  assert.equal(means.status, 200, JSON.stringify(means.data));
  assert.equal(means.data.payload.aggregate, "mean");
  assert.deepEqual(means.data.payload.edges, dto.edges, "metric aggregation cannot change reference counts or policy");
  await page.setViewportSize({width:540, height:780});
  const canvas = await page.locator("#architecture-diagram").evaluate(node => ({overflow:getComputedStyle(node).overflowX, scroll:node.scrollWidth, client:node.clientWidth}));
  assert.equal(canvas.overflow, "auto"); assert.ok(canvas.scroll >= canvas.client);
  await module.focus(); await page.keyboard.press("Space");
  assert.equal(await page.locator("#architecture-inspector-title").textContent(), "src/low.rs");
  assert.equal(await page.locator("#architecture-inspector").isVisible(), true);
  await noOverflow(page);
}));

test("Architecture rejects a route-gated production response after all-callable heat wins", {timeout:240000}, async () => published(rules, async selected => {
  const {page} = selected;
  await ready(page);
  const production = await architecture(selected);
  assert.equal(production.status, 200, JSON.stringify(production.data));
  let entered, release;
  const seen = new Promise(resolve => { entered = resolve; });
  const gate = new Promise(resolve => { release = resolve; });
  const pattern = "**/api/checkouts/*/architecture?*";
  const respond = async route => {
    const url = new URL(route.request().url());
    if (url.searchParams.get("role") !== "production") return route.continue();
    entered(); await gate;
    await route.fulfill({status:200, contentType:"application/json", body:JSON.stringify(production.data)});
  };
  await page.evaluate(() => {
    const original = window.fetch;
    window.__architectureRoleFetch = original;
    window.fetch = async (...args) => {
      const response = await original(...args), url = new URL(args[0], location.href);
      if (url.pathname.endsWith("/architecture") && url.searchParams.get("role") === "production") window.__lateArchitectureDelivered = true;
      return response;
    };
  });
  try {
    await page.locator("#architecture-metric-tests").check();
    await page.waitForFunction(() => document.querySelector("#architecture-state")?.textContent.includes("Metrics: max, all indexed callables"));
    await ready(page);
    await page.locator("#architecture-diagram").getByRole("button", {name:/^src\/low\.rs(?:\s|$)/}).click();
    assert.match(await page.locator("#architecture-inspector-content").textContent(), /4 indexed callables/);
    await page.route(pattern, respond);
    await page.locator("#architecture-metric-tests").uncheck();
    await seen;
    assert.equal(await page.locator("#architecture-diagram svg").count(), 0, "changing role removes old heat before the blocked read");
    assert.equal(await page.locator("#architecture-inspector-content").textContent(), "");
    await page.locator("#architecture-metric-tests").check();
    await page.waitForFunction(() => document.querySelector("#architecture-state")?.textContent.includes("Metrics: max, all indexed callables"));
    await ready(page);
    const allCanvas = await page.locator("#architecture-diagram").textContent();
    release();
    await page.waitForFunction(() => window.__lateArchitectureDelivered === true);
    await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
    assert.match(await page.locator("#architecture-state").textContent(), /all indexed callables/);
    assert.equal(await page.locator("#architecture-metric-tests").isChecked(), true);
    assert.equal(await page.locator("#architecture-diagram").textContent(), allCanvas, "old delivered response cannot replace newer heat");
  } finally {
    release(); await page.unroute(pattern, respond);
    await page.evaluate(() => { window.fetch = window.__architectureRoleFetch; delete window.__architectureRoleFetch; delete window.__lateArchitectureDelivered; });
  }
}));

// Setup/error states use separate pre-start configurations, never racing live edits.

for (const [name, config, state] of [["missing", null, "missing_rules"], ["empty", "version = 1\n", "no_layers"]]) {
  test(`Architecture ${name} rules show proposal help without inventing a diagram`, {timeout:240000}, async () => published(config, async selected => {
    const response = await architecture(selected);
    assert.equal(response.status, 200, JSON.stringify(response.data));
    assert.equal(response.data.payload.state, state);
    assert.deepEqual(response.data.payload.modules, []); assert.deepEqual(response.data.payload.edges, []);
    await selected.page.waitForFunction(() => document.querySelector("#architecture-state")?.textContent.includes("trellis_propose_layers"));
    const message = await selected.page.locator("#architecture-panel").textContent();
    assert.match(message, /\.trellis\/rules\.toml/); assert.match(message, /trellis_propose_layers/);
    assert.equal(await selected.page.locator("#architecture-diagram svg").count(), 0);
    assert.equal(selected.requests.some(row => row.url.pathname.endsWith("/source")), false, "setup does not read a source");
  }));
}

test("Architecture malformed rules preserve the typed located error and no partial interactive canvas", {timeout:240000}, async () => published('version = 1\n[layers.broken]\norder = "not an integer"\npaths = ["src/**"]\n', async selected => {
  const response = await architecture(selected);
  assert.equal(response.status, 422, JSON.stringify(response.data));
  const error = response.data.error;
  assert.equal(error.code, "invalid_rules"); assert.equal(error.path, ".trellis/rules.toml");
  assert.ok(error.line >= 1 && error.column >= 1); assert.ok(error.range.end >= error.range.start);
  assert.equal(typeof error.kind, "string"); assert.equal(typeof error.attribution, "string");
  assert.equal(Object.hasOwn(response.data, "payload"), false);
  await selected.page.locator("#architecture-state[data-state=error]").waitFor();
  assert.match(await selected.page.locator("#architecture-notice-content").textContent(), /invalid_rules/);
  const message = await selected.page.locator("#architecture-state").textContent();
  assert.ok(message.includes(error.path)); assert.ok(message.includes(String(error.line)));
  assert.equal(await selected.page.locator("#architecture-diagram svg, #architecture-diagram [data-trellis-module-id], #architecture-diagram [data-trellis-edge-id]").count(), 0);
}));
